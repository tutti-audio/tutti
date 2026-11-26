//! The encode stage: the pipeline in front of the encoder.
//!
//! # Why the pipeline pulls
//!
//! The pipeline drives the render rather than being fed by it: it takes the
//! graph's blocks (a live `GraphSource`) or a replay (a rendered [`PlaneSource`], or a spill) and pulls it block by block through
//! `drive` (the gate) and [`Chain`] (the resample and the quantize, the one
//! place they live), and hands each block to a `tutti_codec::Encoder`. An encoder only writes
//! what it is given; this module is the one path through those stages that
//! export has (the encoder types are public, and a caller using them directly
//! does its own pulling). Every format streams, FLAC
//! included (it encodes a frame per full block as it arrives), so there is no
//! buffered-vs-streaming decision for a caller to get wrong.
//!
//! The width comes from the caller's [`ChannelLayout`] at runtime, the render
//! folds the graph onto it once, and the file header is simply that width.
//! Nothing here is generic over the width — [`Interleaved`] carries it — so
//! *any* width encodes.

use crate::control::{Control, Outcome, Stage};
use crate::error::Result;
use crate::output::FileTarget;
use crate::render::{PlaneSource, REPLAY_BLOCK, RenderPlan, Replay};
use crate::{RenderError, Written};
use std::path::Path;
use tutti_buffers::{ChannelLayout, ChannelSet};
use tutti_buffers::{Float, Interleaved, InterleavedMut, Planes, Sample, Sink, convert};
use tutti_codec::{EncoderOptions, EncoderSink, FileEncoder, FinalizedFile, Spec};
use tutti_dsp::{
    FftFloat, StreamingResampler, StreamingResamplerConfig, needs_resampling, resampled_frames,
};
use tutti_rt::SplitMix64;
use tutti_units::{Amplitude, SampleRate, Samples};

/// The rate the file is written at: the resample target, else the rate it is
/// rendered at.
pub(crate) fn output_rate(target: &FileTarget, render_rate: SampleRate) -> SampleRate {
    target.resample.map_or(render_rate, |r| r.target_rate)
}

/// What the file is written as: the target's format, width and speaker order at
/// the output rate.
fn spec(target: &FileTarget, source_rate: SampleRate) -> Spec {
    let rate = output_rate(target, source_rate);
    Spec::of_channels(target.format(), target.channel_set(), rate)
}

/// The file of `target`, begun: written to a temporary beside its path (which
/// the encoder removes unless it is finished and committed), through the
/// converter and the encoder's own dither, whose noise is the sequence of `seed`.
///
/// Outputs of one render are summed later (a bounce's stems and its master), so each takes the
/// seed [`SplitMix64::mix`] of its index, never one seed: two files with one noise sequence would
/// add it coherently.
///
/// Refuses a render the format cannot hold before the temporary exists: the
/// encoder judges the frames the file will hold, which are the plan's kept
/// frames at the output rate (`tutti_dsp::resampled_frames`), and the
/// error is its own. The frames of `plan` are at `source_rate`.
pub(crate) fn open_file<F: FftFloat>(
    target: &FileTarget,
    plan: &RenderPlan,
    source_rate: SampleRate,
    seed: u64,
) -> Result<Chain<F>> {
    let spec = spec(target, source_rate);
    let planned = resampled_frames(source_rate, spec.rate(), plan.output_length);
    let options = EncoderOptions::new(&spec)
        .planned(planned)
        .dither_seed(seed)
        .file(target.path());
    let options = if target.replaces_existing() {
        options.replace()
    } else {
        options
    };
    let encoder = options.create()?;
    Chain::new(target, source_rate, encoder)
}

/// What a [`Chain`] finalized into: the file closed beside its path and not
/// yet put in place, and what the pass counted of it.
pub(crate) struct Closed {
    file: FinalizedFile,
    clipped: u64,
    frames: Samples,
}

impl Closed {
    /// Puts the file in place. A file that could not be put in place is kept
    /// where it was written, and the error ([`tutti_codec::EncodeError::Commit`])
    /// names it.
    pub(crate) fn commit(self) -> tutti_codec::Result<Landed> {
        self.file.commit()?;
        Ok(Landed {
            clipped: self.clipped,
            frames: self.frames,
        })
    }
}

/// A file that was put in place, the samples that clipped on the way and the
/// frames it holds.
pub(crate) struct Landed {
    pub(crate) clipped: u64,
    pub(crate) frames: Samples,
}

impl<F: FftFloat> Chain<F> {
    /// Flushes the converter and closes the encoder's file beside its path:
    /// everything that can fail for the file's own sake, short of moving it.
    pub(crate) fn finalize(mut self) -> Result<Closed> {
        self.flush()?;
        let (encoder, clipped, frames) = self.into_encoder();
        Ok(Closed {
            file: encoder.finalize()?,
            clipped,
            frames,
        })
    }
}

/// What a landed file is, for the caller: its size on disk, `0` if it cannot
/// be stat'd.
pub(crate) fn written(
    path: &Path,
    closed: Landed,
    dropped_events: u64,
    placement: tutti_buffers::Placement,
) -> Written {
    Written {
        path: path.to_path_buf(),
        bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        frames: closed.frames,
        dropped_events,
        clipped_samples: closed.clipped,
        gain: None,
        measured: None,
        placement,
        spill_left: None,
    }
}

/// A converter and the planes it takes from and gives to.
pub(crate) type Resampler<F> = (StreamingResampler<F>, Vec<Vec<F>>, Vec<Vec<F>>);

/// The converter `target` calls for from frames at `source_rate` of `ch`
/// channels, or `None` when the file is at the rate the frames are.
pub(crate) fn resampler_for<F: FftFloat>(
    target: &FileTarget,
    source_rate: SampleRate,
    layout: ChannelLayout,
) -> Result<Option<Resampler<F>>> {
    // Whether to convert at all is the converter's to say
    // (`tutti_dsp::needs_resampling`), not a float comparison here.
    //
    // `source_rate` is the PARAMETER, not a rate of the target's.
    // Frames handed to `write_buffers` already exist and carry their own
    // rate, which may not be the one a config was rendered at; reading
    // it from a config resamples from a rate the samples were never at.
    Ok(match target.resample {
        Some(r) if needs_resampling(source_rate, r.target_rate) => {
            let ch = usize::from(layout.count());
            Some((
                StreamingResampler::try_from(
                    StreamingResamplerConfig::new(layout, source_rate, r.target_rate)
                        .chunk(r.chunk),
                )?,
                vec![Vec::<F>::new(); ch],
                vec![Vec::<F>::new(); ch],
            ))
        }
        // A resample to the rate the frames are already at is not a resample.
        _ => None,
    })
}

/// The resample and quantize between the gate and the encoder, one block at a
/// time.
///
/// Every export goes through this. That is the point: this is where the
/// resample lives (the gate is `drive`'s), so a file is never written
/// un-resampled under a resampled header.
///
/// Order matters: resample first, quantize second. Dither's noise is scaled to
/// one LSB at the *output* depth, so dithering before a rate conversion would
/// filter that noise along with the signal and land it somewhere other than one
/// LSB. The quantize stage is the encoder's own: an integer file is dithered as its
/// `tutti_codec::IntPcm` says, and a float file takes the block as it is.
///
/// `F` is the format the blocks arrive in and the converter runs in.
pub(crate) struct Chain<F: FftFloat> {
    layout: ChannelLayout,
    sink: EncoderSink<F>,
    /// Interleaved, so it is a flat sample buffer with `ch` samples per frame.
    staging: Vec<F>,
    /// A gain applied after the converter and before the quantize: the
    /// normalize of a file, which was measured at the file's rate.
    gain: Option<Amplitude>,
    /// The converter, and the planes it takes from and gives to.
    resampler: Option<Resampler<F>>,
}

impl<F: FftFloat> Chain<F> {
    /// The chain for `target`, from frames at `source_rate`, ending in
    /// `encoder`.
    fn new(target: &FileTarget, source_rate: SampleRate, encoder: FileEncoder) -> Result<Self> {
        // Once, at entry — never inside a per-block or per-frame loop.
        let layout = target.channels();
        let ch = usize::from(layout.count());
        if ch == 0 {
            return Err(crate::RenderError::UnsupportedChannels);
        }
        let resampler = resampler_for(target, source_rate, layout)?;
        Ok(Self {
            layout,
            sink: EncoderSink::new(encoder),
            staging: Vec::new(),
            gain: None,
            resampler,
        })
    }

    /// Scales everything from here on by `gain` after the converter and
    /// before the quantize (the gain of a normalize, measured at the file's
    /// rate).
    pub(crate) fn set_gain(&mut self, gain: tutti_units::Db) {
        self.gain = Some(gain.to_amplitude());
    }

    /// One gated block through the converter, the gain and the quantize into
    /// the encoder.
    pub(crate) fn block<B: Sample>(&mut self, block: Interleaved<'_, B>) -> Result<()> {
        let gain = self.gain;
        if let Some((rs, planes, out)) = self.resampler.as_mut() {
            crate::planar::split(block, planes)?;
            rs.push(Planes::from_vecs(planes)?, out)?;
            emit_planes(out, &mut self.staging, &mut self.sink, self.layout, gain)
        } else {
            // Into the chain's own format, then the gain: the encoder (or the
            // quantize stage) takes `F`.
            self.staging.clear();
            self.staging.resize(block.samples().len(), F::SILENCE);
            convert(block, InterleavedMut::new(&mut self.staging, self.layout)?)?;
            if let Some(gain) = gain {
                let gain = <F as Sample>::from_f64(gain.get());
                for s in &mut self.staging {
                    *s *= gain;
                }
            }
            let staged = Interleaved::new(&self.staging, self.layout)?;
            self.sink.write(staged).map_err(|_| self.failure())
        }
    }

    /// What the converter still holds, through the quantize into the encoder; a
    /// chain with no converter holds nothing.
    pub(crate) fn flush(&mut self) -> Result<()> {
        let gain = self.gain;
        let Some((rs, _, out)) = self.resampler.as_mut() else {
            return Ok(());
        };
        rs.finish(out)?;
        emit_planes(out, &mut self.staging, &mut self.sink, self.layout, gain)
    }

    /// The error the encoder stashed when it refused a write.
    fn failure(&mut self) -> RenderError {
        failure_of(&mut self.sink)
    }

    /// The encoder, to finish it, how many values clipped on the way and how
    /// many frames it was given.
    fn into_encoder(self) -> (FileEncoder, u64, Samples) {
        let frames = self.sink.frames();
        let encoder = self.sink.into_encoder();
        let clipped = encoder.clipped();
        (encoder, clipped, frames)
    }
}

fn failure_of<F: Float>(sink: &mut EncoderSink<F>) -> RenderError {
    sink.take_error().map_or_else(
        || RenderError::from(std::io::Error::other("the encoder refused a write")),
        RenderError::from,
    )
}

/// Interleave the resampler's output planes, apply the gain, hand them on, and
/// clear the planes for the next push.
fn emit_planes<F: Float>(
    out: &mut [Vec<F>],
    staging: &mut Vec<F>,
    sink: &mut EncoderSink<F>,
    layout: ChannelLayout,
    gain: Option<Amplitude>,
) -> Result<()> {
    crate::planar::join(out, staging, layout)?;
    if let Some(gain) = gain {
        let gain = <F as Sample>::from_f64(gain.get());
        for s in staging.iter_mut() {
            *s *= gain;
        }
    }
    let r = Interleaved::new(staging, layout)
        .map_err(RenderError::from)
        .and_then(|block| sink.write(block).map_err(|_| failure_of(sink)));
    for p in out.iter_mut() {
        p.clear();
    }
    r
}

/// Reads `replay` back through `chain`: the write pass of a normalized output
/// and of already-rendered planes, which share every codec path with a streamed
/// render (a second writer per format is how a crate ends up with a
/// whole-signal encoder that works and a streaming one that does not).
///
/// `block_frames` is how many frames one read takes, and `done` of `total`
/// frames of `stage` were already spent in the pass (by the outputs before
/// this one). The token is checked before every block. A cancelled pass is
/// [`Outcome::Cancelled`].
pub(crate) fn replay_through<F: FftFloat>(
    replay: &mut dyn Replay<F>,
    chain: &mut Chain<F>,
    block_frames: usize,
    stage: Stage,
    done: Samples,
    total: Samples,
    control: &mut Control<'_>,
) -> Result<Outcome<Samples>> {
    let layout = chain.layout;
    let mut block = vec![F::ZERO; layout.interleaved_len(Samples::new(block_frames.max(1)))];
    let mut block = InterleavedMut::new(&mut block, layout)?;
    let mut at = done.get();
    loop {
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let n = replay.fill(block.reborrow())?;
        if n == 0 {
            break;
        }
        chain.block(block.as_interleaved().slice(Samples::ZERO..Samples::new(n)))?;
        at += n;
        control.advance(stage, Samples::new(at), total);
    }
    Ok(Outcome::Finished(Samples::new(at)))
}

/// Replays `rendered` into a chain for `target` (the converter at the planes'
/// own rate, then the dither and the encoder): the planes are folded onto the
/// target's width the way a graph's outputs are, by the same rule (the
/// target's `topology` included), and a width it cannot place is an error,
/// never a dropped channel.
///
/// Returns the chain with every frame written, for the finalize.
pub(crate) fn write_planes<F: FftFloat>(
    rendered: &crate::Rendered<F>,
    target: &FileTarget,
    control: &mut Control<'_>,
) -> Result<Outcome<Chain<F>>> {
    let frames = rendered.frames();
    let fold = if rendered.planes().is_empty() || rendered.layout() == target.channels() {
        None
    } else {
        Some(
            crate::render::FileChannels {
                channels: target.channel_set(),
            }
            .fold_from(
                rendered.channels(),
                &ChannelSet::default_for(rendered.layout()),
            )?,
        )
    };
    let plan = RenderPlan::of_frames(frames);
    // The frames' OWN rate: these planes may have been rendered under a
    // different config entirely.
    let mut chain = open_file(target, &plan, rendered.sample_rate(), SplitMix64::mix(0))?;
    let mut src = PlaneSource::new(rendered.planes(), fold);
    control.begin(Stage::Writing, Some(frames));
    let written = replay_through(
        &mut src,
        &mut chain,
        REPLAY_BLOCK,
        Stage::Writing,
        Samples::new(0),
        frames,
        control,
    )?;
    Ok(written.map(|_| chain))
}

#[cfg(test)]
mod tests;
