//! How an output stream runs on a sound card.
//!
//! The block seam ([`StreamDriver`], [`OutputBlock`], [`OutputSpec`]) is
//! `tutti-engine`'s; [`CpalDriver`] is the driver that runs it on CPAL's thread.
//! Its closure body is one call to [`OutputBlock::render_delayed_with`], the
//! method a manual stream's callback calls too, so a manual stream runs exactly
//! the callback CPAL runs.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, StreamTrait};
use std::time::Duration;

use tutti_buffers::{ChannelLayout, MAX_CHANNELS, MaxBlock};
use tutti_buffers::{Float, Sample, SampleFormat};
use tutti_engine::{
    MAX_FRAMES, OutputBlock, OutputSpec, RunningStream, StreamDriver, StreamFaults,
};
use tutti_units::{SampleRate, Samples};

use crate::edge::{self, OffsetBinary};
use crate::error::{DeviceError, Result};
use crate::owner::Owned;

/// The rate a device reports, or [`DeviceError::InvalidDevice`] when it reports 0 Hz.
pub(crate) fn device_rate(rate: cpal::SampleRate) -> Result<SampleRate> {
    SampleRate::from_hz(rate).ok_or_else(|| {
        DeviceError::InvalidDevice("the device reports a sample rate of 0 Hz".into())
    })
}

/// The callback size asked of a device that offers a range: 512 frames
/// (10.7 ms at 48 kHz), a common DAW default, clamped into what the device
/// supports.
pub const PREFERRED_QUANTUM: MaxBlock = MaxBlock::of(Samples::new(512));

/// The [`SampleFormat`] a device's `format` reports in an [`OutputSpec`], or
/// [`DeviceError::InvalidConfig`] for a type the driver does not build. `I8`, `I16`,
/// `I32`, `F32` and `F64` are themselves; `U8`, `U16` and `U32` are offset
/// binary and report the signed type of their width.
pub(crate) fn spec_format(format: cpal::SampleFormat) -> Result<SampleFormat> {
    match format {
        cpal::SampleFormat::I8 | cpal::SampleFormat::U8 => Ok(SampleFormat::I8),
        cpal::SampleFormat::I16 | cpal::SampleFormat::U16 => Ok(SampleFormat::I16),
        cpal::SampleFormat::I32 | cpal::SampleFormat::U32 => Ok(SampleFormat::I32),
        cpal::SampleFormat::F32 => Ok(SampleFormat::F32),
        cpal::SampleFormat::F64 => Ok(SampleFormat::F64),
        other => Err(DeviceError::InvalidConfig(format!(
            "Unsupported sample format: {other:?}"
        ))),
    }
}

/// How a device's `offered` format stands to the format `wanted`, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Fit {
    /// The same type: signed or float.
    Exact,
    /// The offset-binary type of the same width, opened through the signed
    /// format with the sign bit flipped.
    OffsetBinary,
}

/// Whether a device format `offered` opens as the app's `wanted`, and how.
/// `None` for a format of another kind or width.
pub(crate) fn fit(offered: cpal::SampleFormat, wanted: SampleFormat) -> Option<Fit> {
    if spec_format(offered).ok()? != wanted {
        return None;
    }
    Some(match offered {
        cpal::SampleFormat::U8 | cpal::SampleFormat::U16 | cpal::SampleFormat::U32 => {
            Fit::OffsetBinary
        }
        _ => Fit::Exact,
    })
}

/// The output config to open: the device's `default` when the app chose no
/// format, else the supported range at the default's rate and width that
/// opens as `wanted` (a signed type before its offset-binary twin).
///
/// Pure, so it is testable with no device.
///
/// # Errors
/// [`DeviceError::OutputFormatUnavailable`] when no range offers `wanted`.
pub(crate) fn choose_output_config(
    supported: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    default: cpal::SupportedStreamConfig,
    wanted: Option<SampleFormat>,
    device_name: &str,
) -> Result<cpal::SupportedStreamConfig> {
    let Some(wanted) = wanted else {
        return Ok(default);
    };
    let rate = default.sample_rate();
    supported
        .filter(|r| {
            r.channels() == default.channels()
                && r.min_sample_rate() <= rate
                && rate <= r.max_sample_rate()
        })
        .filter_map(|r| fit(r.sample_format(), wanted).map(|f| (f, r)))
        .min_by_key(|(f, _)| *f)
        .map(|(_, r)| r.with_sample_rate(rate))
        .ok_or_else(|| DeviceError::OutputFormatUnavailable {
            device_name: device_name.to_owned(),
            format: wanted,
        })
}

/// A device's default output config, as the spec a stream opens at and the
/// exact sample type its callback converts to.
pub(crate) fn spec_from_supported(
    c: &cpal::SupportedStreamConfig,
) -> Result<(OutputSpec, cpal::SampleFormat)> {
    if usize::from(c.channels()) > MAX_CHANNELS {
        return Err(DeviceError::InvalidConfig(format!(
            "the device has {} channels; at most {MAX_CHANNELS} are supported",
            c.channels()
        )));
    }
    let spec = OutputSpec::new(
        device_rate(c.sample_rate())?,
        ChannelLayout::from(c.channels().max(1)),
        spec_format(c.sample_format())?,
    );
    let spec = match quantum_for(c.buffer_size()) {
        Some(quantum) => spec.with_quantum(quantum),
        None => spec,
    };
    Ok((spec, c.sample_format()))
}

fn stream_config(spec: &OutputSpec) -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels: spec.channels.count(),
        sample_rate: spec.sample_rate.get().round() as cpal::SampleRate,
        buffer_size: match spec.quantum {
            Some(q) => cpal::BufferSize::Fixed(q.get_u32()),
            None => cpal::BufferSize::Default,
        },
    }
}

/// The fixed callback size to open a device with: [`PREFERRED_QUANTUM`]
/// clamped into the range it reports, and never past [`MAX_FRAMES`] (the
/// callback's own buffers). `None` for a device that reports no range, which
/// is opened with its default buffer.
fn quantum_for(range: &cpal::SupportedBufferSize) -> Option<MaxBlock> {
    match *range {
        cpal::SupportedBufferSize::Range { min, max } => {
            // A device reporting a range with no usable size (a zero `max`)
            // has no quantum to open with.
            let (min, max) = ((min as usize).max(1), (max as usize).min(MAX_FRAMES));
            (min <= max)
                .then(|| MaxBlock::of(Samples::new(PREFERRED_QUANTUM.get().clamp(min, max))))
        }
        cpal::SupportedBufferSize::Unknown => None,
    }
}

/// The device driver: runs the callback on a real `cpal::Stream`.
///
/// Created internally by [`AudioEngine::start`](crate::AudioEngine::start) and
/// [`TuttiDriver::restart`](crate::TuttiDriver::restart) from the selected
/// device.
pub struct CpalDriver {
    device: cpal::Device,
    /// The device's exact sample type, which `spec.sample_format` cannot say
    /// for the offset-binary formats; one [`spec_format`] accepts.
    format: cpal::SampleFormat,
}

impl std::fmt::Debug for CpalDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpalDriver").finish_non_exhaustive()
    }
}

impl CpalDriver {
    /// A driver over `device` at the sample type `format` of its config.
    ///
    /// # Errors
    /// [`DeviceError::InvalidConfig`] for a sample type the driver does not build.
    pub(crate) fn new(device: cpal::Device, format: cpal::SampleFormat) -> Result<Self> {
        spec_format(format)?;
        Ok(Self { device, format })
    }
}

/// The running handle of a [`CpalDriver`] stream. CPAL runs the callback for
/// as long as this value exists; dropping it stops the stream.
///
/// The `cpal::Stream` is created, played and dropped on a thread of its own
/// (some backends tie a stream to its creating thread); this handle is `Send`
/// and dropping it, from any thread, drops the stream there and waits for it.
#[derive(Debug)]
pub struct CpalStream(
    #[allow(dead_code, reason = "ownership is the API — held for Drop, never read")] Owned,
);

impl RunningStream for CpalStream {
    fn stop(self: Box<Self>) {
        // Dropping is the stop.
    }
}

impl<F: Float> StreamDriver<F> for CpalDriver {
    type Running = CpalStream;
    type Error = DeviceError;

    /// Opens `block` on the device in the device's own sample type. The
    /// graph's `F` converts at the edge by the rule in `Sample`: a signed or
    /// float format directly, an offset-binary one through the signed format of
    /// its width with the sign bit flipped.
    fn open(
        self,
        spec: &OutputSpec,
        block: OutputBlock<F>,
        faults: Arc<StreamFaults>,
    ) -> Result<CpalStream> {
        let config = stream_config(spec);
        let format = self.format;
        Owned::spawn("tutti-output-stream", move || {
            let stream = match format {
                cpal::SampleFormat::I8 => self.build(&config, block, faults, native::<F, i8>)?,
                cpal::SampleFormat::I16 => self.build(&config, block, faults, native::<F, i16>)?,
                cpal::SampleFormat::I32 => self.build(&config, block, faults, native::<F, i32>)?,
                cpal::SampleFormat::U8 => self.build(&config, block, faults, offset::<F, u8>)?,
                cpal::SampleFormat::U16 => self.build(&config, block, faults, offset::<F, u16>)?,
                cpal::SampleFormat::U32 => self.build(&config, block, faults, offset::<F, u32>)?,
                cpal::SampleFormat::F32 => self.build(&config, block, faults, native::<F, f32>)?,
                cpal::SampleFormat::F64 => self.build(&config, block, faults, native::<F, f64>)?,
                // Checked by `CpalDriver::new`.
                _ => unreachable!("sample format checked at construction"),
            };
            stream.play().map_err(DeviceError::PlayStream)?;
            Ok(stream)
        })
        .map(CpalStream)
    }
}

/// What one callback does with `block`: renders into the device's `T` buffer,
/// recording `delay`.
type Render<F, T> = fn(&mut OutputBlock<F>, &mut [T], Option<Duration>);

/// A device format that is a [`Sample`]: converted by
/// its rule.
fn native<F: Float, S: Sample>(
    block: &mut OutputBlock<F>,
    data: &mut [S],
    delay: Option<Duration>,
) {
    block.render_delayed(data, delay);
}

/// An offset-binary device format: the same rule on the signed format of its
/// width, then the sign bit flipped (`Sample` covers no unsigned format).
fn offset<F: Float, U: OffsetBinary>(
    block: &mut OutputBlock<F>,
    data: &mut [U],
    delay: Option<Duration>,
) {
    block.render_delayed_with(data, delay, edge::to_offset::<F, U>);
}

impl CpalDriver {
    fn build<T: cpal::SizedSample + 'static, F: Float>(
        &self,
        config: &cpal::StreamConfig,
        mut block: OutputBlock<F>,
        faults: Arc<StreamFaults>,
        render: Render<F, T>,
    ) -> Result<cpal::Stream> {
        let channels = block.channels();
        let delivering = Arc::clone(&faults);
        self.device
            .build_output_stream(
                *config,
                move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                    // The CPAL contract: a callback no larger than MAX_FRAMES.
                    // The assertion lives HERE, at the boundary CPAL crosses, and
                    // not inside `render_with`: it is a claim about CPAL, not about
                    // the block, and keeping it out is what lets a debug-build
                    // test observe the clamp instead of panicking before it.
                    debug_assert!(
                        data.len() / channels <= MAX_FRAMES,
                        "CPAL callback frames {} exceeds MAX_FRAMES {MAX_FRAMES}",
                        data.len() / channels
                    );
                    render(&mut block, data, edge::output_delay(&info.timestamp()));
                    delivering.delivered();
                },
                move |err| edge::record(&faults, &err),
                None,
            )
            .map_err(DeviceError::BuildStream)
    }
}

#[cfg(test)]
mod choice_tests;
#[cfg(test)]
mod format_tests;
#[cfg(test)]
mod quantum_tests;
