//! How an output stream runs on a sound card.
//!
//! The block seam ([`StreamDriver`], [`OutputBlock`], [`OutputSpec`]) is
//! `tutti-engine`'s; [`CpalDriver`] is the driver that runs it on CPAL's thread.
//! Its closure body is one call to [`OutputBlock::render_delayed_with`], the
//! method a manual stream's callback calls too, so a manual stream runs exactly
//! the callback CPAL runs.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, StreamTrait};
use tutti_buffers::{Float, SampleFormat};
use tutti_engine::{
    MAX_FRAMES, OutputBlock, OutputSpec, RunningStream, StreamDriver, StreamFaults,
};
use tutti_types::{ChannelLayout, MAX_CHANNELS, MaxBlock};
use tutti_units::{SampleRate, Samples};

use crate::edge;
use crate::error::{Error, Result};
use crate::owner::Owned;

/// The rate a device reports, or [`Error::InvalidDevice`] when it reports 0 Hz.
pub(crate) fn device_rate(rate: cpal::SampleRate) -> Result<SampleRate> {
    SampleRate::from_hz(rate)
        .ok_or_else(|| Error::InvalidDevice("the device reports a sample rate of 0 Hz".into()))
}

/// The callback size asked of a device that offers a range: 512 frames
/// (10.7 ms at 48 kHz), a common DAW default, clamped into what the device
/// supports.
pub const PREFERRED_QUANTUM: MaxBlock = MaxBlock::of(Samples(512));

/// The [`SampleFormat`] a device's `format` reports in an [`OutputSpec`], or
/// [`Error::InvalidConfig`] for a type the driver does not build. `I8`, `I16`,
/// `I32`, `F32` and `F64` are themselves; `U8`, `U16` and `U32` are offset
/// binary and report the signed type of their width.
fn spec_format(format: cpal::SampleFormat) -> Result<SampleFormat> {
    match format {
        cpal::SampleFormat::I8 | cpal::SampleFormat::U8 => Ok(SampleFormat::I8),
        cpal::SampleFormat::I16 | cpal::SampleFormat::U16 => Ok(SampleFormat::I16),
        cpal::SampleFormat::I32 | cpal::SampleFormat::U32 => Ok(SampleFormat::I32),
        cpal::SampleFormat::F32 => Ok(SampleFormat::F32),
        cpal::SampleFormat::F64 => Ok(SampleFormat::F64),
        other => Err(Error::InvalidConfig(format!(
            "Unsupported sample format: {other:?}"
        ))),
    }
}

/// A device's default output config, as the spec a stream opens at and the
/// exact sample type its callback converts to.
pub(crate) fn spec_from_supported(
    c: &cpal::SupportedStreamConfig,
) -> Result<(OutputSpec, cpal::SampleFormat)> {
    if usize::from(c.channels()) > MAX_CHANNELS {
        return Err(Error::InvalidConfig(format!(
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
            (min <= max).then(|| MaxBlock::of(Samples(PREFERRED_QUANTUM.get().clamp(min, max))))
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
    /// [`Error::InvalidConfig`] for a sample type the driver does not build.
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

/// The graph's float formats the device edge converts from: cpal's sample types
/// convert from `f32` and `f64` (`cpal::FromSample`).
macro_rules! stream_driver_for {
    ($($float:ty),+) => {$(
        impl StreamDriver<$float> for CpalDriver {
            type Running = CpalStream;
            type Error = Error;

            fn open(
                self,
                spec: &OutputSpec,
                block: OutputBlock<$float>,
                faults: Arc<StreamFaults>,
            ) -> Result<CpalStream> {
                self.open_stream(spec, block, faults)
            }
        }
    )+};
}

stream_driver_for!(f32, f64);

impl CpalDriver {
    /// Opens `block` on the device in the device's own sample type, converting
    /// from the graph's `F` per sample at the edge.
    fn open_stream<F>(
        self,
        spec: &OutputSpec,
        block: OutputBlock<F>,
        faults: Arc<StreamFaults>,
    ) -> Result<CpalStream>
    where
        F: Float,
        i8: cpal::FromSample<F>,
        i16: cpal::FromSample<F>,
        i32: cpal::FromSample<F>,
        u8: cpal::FromSample<F>,
        u16: cpal::FromSample<F>,
        u32: cpal::FromSample<F>,
        f32: cpal::FromSample<F>,
        f64: cpal::FromSample<F>,
    {
        let config = stream_config(spec);
        let format = self.format;
        Owned::spawn("tutti-output-stream", move || {
            let stream = match format {
                cpal::SampleFormat::I8 => self.build::<i8, F>(&config, block, faults)?,
                cpal::SampleFormat::I16 => self.build::<i16, F>(&config, block, faults)?,
                cpal::SampleFormat::I32 => self.build::<i32, F>(&config, block, faults)?,
                cpal::SampleFormat::U8 => self.build::<u8, F>(&config, block, faults)?,
                cpal::SampleFormat::U16 => self.build::<u16, F>(&config, block, faults)?,
                cpal::SampleFormat::U32 => self.build::<u32, F>(&config, block, faults)?,
                cpal::SampleFormat::F32 => self.build::<f32, F>(&config, block, faults)?,
                cpal::SampleFormat::F64 => self.build::<f64, F>(&config, block, faults)?,
                // Checked by `CpalDriver::new`.
                _ => unreachable!("sample format checked at construction"),
            };
            stream.play().map_err(Error::PlayStream)?;
            Ok(stream)
        })
        .map(CpalStream)
    }

    fn build<T, F: Float>(
        &self,
        config: &cpal::StreamConfig,
        mut block: OutputBlock<F>,
        faults: Arc<StreamFaults>,
    ) -> Result<cpal::Stream>
    where
        T: cpal::SizedSample + cpal::FromSample<F>,
    {
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
                    block.render_delayed_with(
                        data,
                        edge::output_delay(&info.timestamp()),
                        T::from_sample,
                    );
                    delivering.delivered();
                },
                move |err| edge::record(&faults, &err),
                None,
            )
            .map_err(Error::BuildStream)
    }
}

#[cfg(test)]
mod quantum_tests;
