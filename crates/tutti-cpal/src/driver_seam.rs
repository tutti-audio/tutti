//! How an output stream runs on a sound card.
//!
//! The block seam ([`StreamDriver`], [`OutputBlock`], [`OutputSpec`]) is
//! `tutti-engine`'s; [`CpalDriver`] is the driver that runs it on CPAL's thread.
//! Its closure body is one call to [`OutputBlock::render_delayed_with`], the
//! method a manual stream's callback calls too, so a manual stream runs exactly
//! the callback CPAL runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use cpal::traits::{DeviceTrait, StreamTrait};
use std::time::Duration;

use tutti_buffers::{ChannelLayout, MAX_CHANNELS};
use tutti_buffers::{Float, Sample, SampleFormat};
use tutti_engine::{
    MAX_FRAMES, OpenedOutput, OutputBlock, OutputQuantum, OutputSpec, RunningStream, StreamDriver,
    StreamFaults,
};
use tutti_platform::Executor;
use tutti_rt::Mutex;
use tutti_units::SampleRate;

use crate::edge::{self, OffsetBinary};
use crate::error::{DeviceError, Result};
use crate::owner::Owned;

/// The rate a device reports, or [`DeviceError::InvalidDevice`] when it reports 0 Hz.
pub(crate) fn device_rate(rate: cpal::SampleRate) -> Result<SampleRate> {
    SampleRate::from_hz(rate).ok_or_else(|| {
        DeviceError::InvalidDevice("the device reports a sample rate of 0 Hz".into())
    })
}

/// The callback size asked of a device that offers a range when none was
/// asked for ([`OutputConfig::quantum`](crate::OutputConfig::quantum)): 512
/// frames (10.7 ms at 48 kHz), a common DAW default, clamped into what the
/// device supports.
pub const PREFERRED_QUANTUM: OutputQuantum = OutputQuantum::of(512);

/// The sample formats the driver builds: the device's own type, parsed once
/// from `cpal::SampleFormat` (which is `non_exhaustive`) at the edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceFormat {
    I8,
    I16,
    I32,
    F32,
    F64,
    /// Offset binary: the signed type of the same width with the sign bit flipped.
    U8,
    U16,
    U32,
}

impl TryFrom<cpal::SampleFormat> for DeviceFormat {
    type Error = DeviceError;

    /// # Errors
    /// [`DeviceError::InvalidConfig`] for a type the driver does not build.
    fn try_from(format: cpal::SampleFormat) -> Result<Self> {
        match format {
            cpal::SampleFormat::I8 => Ok(Self::I8),
            cpal::SampleFormat::I16 => Ok(Self::I16),
            cpal::SampleFormat::I32 => Ok(Self::I32),
            cpal::SampleFormat::F32 => Ok(Self::F32),
            cpal::SampleFormat::F64 => Ok(Self::F64),
            cpal::SampleFormat::U8 => Ok(Self::U8),
            cpal::SampleFormat::U16 => Ok(Self::U16),
            cpal::SampleFormat::U32 => Ok(Self::U32),
            other => Err(DeviceError::InvalidConfig(format!(
                "Unsupported sample format: {other:?}"
            ))),
        }
    }
}

impl DeviceFormat {
    /// The [`SampleFormat`] this reports in an [`OutputSpec`]: itself, and for
    /// an offset-binary format the signed type of its width.
    pub(crate) const fn spec(self) -> SampleFormat {
        match self {
            Self::I8 | Self::U8 => SampleFormat::I8,
            Self::I16 | Self::U16 => SampleFormat::I16,
            Self::I32 | Self::U32 => SampleFormat::I32,
            Self::F32 => SampleFormat::F32,
            Self::F64 => SampleFormat::F64,
        }
    }

    /// Whether the format is offset binary.
    pub(crate) const fn is_offset_binary(self) -> bool {
        matches!(self, Self::U8 | Self::U16 | Self::U32)
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
    let offered = DeviceFormat::try_from(offered).ok()?;
    if offered.spec() != wanted {
        return None;
    }
    Some(if offered.is_offset_binary() {
        Fit::OffsetBinary
    } else {
        Fit::Exact
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

/// A device's output config, as the spec a stream opens at and the exact
/// sample type its callback converts to. `wanted` is the callback size the
/// caller asked for: opened at exactly it or refused, and
/// [`PREFERRED_QUANTUM`] fitted into the device's range when none was.
pub(crate) fn spec_from_supported(
    c: &cpal::SupportedStreamConfig,
    device_name: &str,
    wanted: Option<OutputQuantum>,
) -> Result<(OutputSpec, DeviceFormat)> {
    if usize::from(c.channels()) > MAX_CHANNELS {
        return Err(DeviceError::InvalidConfig(format!(
            "the device has {} channels; at most {MAX_CHANNELS} are supported",
            c.channels()
        )));
    }
    let rate = device_rate(c.sample_rate())?;
    let format = DeviceFormat::try_from(c.sample_format())?;
    let spec = OutputSpec::new(
        rate,
        ChannelLayout::from(c.channels().max(1)),
        format.spec(),
    );
    let quantum = match wanted {
        Some(wanted) => Some(offered(c.buffer_size(), wanted, device_name)?),
        None => quantum_for(c.buffer_size()),
    };
    let spec = match quantum {
        Some(quantum) => spec.with_quantum(quantum),
        None => spec,
    };
    Ok((spec, format))
}

fn stream_config(spec: &OutputSpec) -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels: spec.channels.count(),
        sample_rate: spec.sample_rate.get().round() as cpal::SampleRate,
        buffer_size: match spec.quantum {
            Some(q) => cpal::BufferSize::Fixed(q.max_block().get_u32()),
            None => cpal::BufferSize::Default,
        },
    }
}

/// What a refused stream build does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fallback {
    /// Build once more with the backend's default buffer.
    RetryOnDefault,
    /// Report the refusal.
    GiveUp,
}

/// What follows a build that asked for `asked` and failed with `kind`: a fixed
/// buffer size the backend refuses (`UnsupportedConfig`) is retried on the
/// default buffer; any other failure, and a default buffer's own refusal
/// (nothing is left to fall back to), is reported.
pub(crate) fn on_build_failure(asked: cpal::BufferSize, kind: cpal::ErrorKind) -> Fallback {
    match (asked, kind) {
        (cpal::BufferSize::Fixed(_), cpal::ErrorKind::UnsupportedConfig) => {
            Fallback::RetryOnDefault
        }
        _ => Fallback::GiveUp,
    }
}

/// The callback size a stream reports (`frames`) as a quantum: `None` for a
/// count [`OutputQuantum`] refuses (zero, or past [`MAX_FRAMES`]).
pub(crate) fn opened_quantum(frames: cpal::FrameCount) -> Option<OutputQuantum> {
    OutputQuantum::new(usize::try_from(frames).ok()?)
}

/// `wanted` as the callback size to open a device with, when the device
/// offers it: a size outside the range the device reports is refused by name,
/// never clamped into it. A device that reports no range is asked for `wanted`
/// as it is, and CPAL refuses it at the stream's build if it cannot.
fn offered(
    range: &cpal::SupportedBufferSize,
    wanted: OutputQuantum,
    device_name: &str,
) -> Result<OutputQuantum> {
    if let cpal::SupportedBufferSize::Range { min, max } = *range {
        let frames = wanted.max_block().get_u32();
        if frames < min || frames > max {
            return Err(DeviceError::QuantumUnavailable {
                device_name: device_name.to_owned(),
                quantum: wanted,
                min,
                max,
            });
        }
    }
    Ok(wanted)
}

/// The fixed callback size to open a device with when none was asked for:
/// [`PREFERRED_QUANTUM`] clamped into the range it reports, and never past
/// [`MAX_FRAMES`] (the callback's own buffers). `None` for a device that
/// reports no range, which is opened with its default buffer.
fn quantum_for(range: &cpal::SupportedBufferSize) -> Option<OutputQuantum> {
    match *range {
        cpal::SupportedBufferSize::Range { min, max } => {
            // A device reporting a range with no usable size (a zero `max`)
            // has no quantum to open with.
            let (min, max) = ((min as usize).max(1), (max as usize).min(MAX_FRAMES));
            (min <= max).then(|| OutputQuantum::of(PREFERRED_QUANTUM.get().clamp(min, max)))
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
    /// for the offset-binary formats.
    format: DeviceFormat,
    /// The executor whose dedicated thread owns the stream.
    executor: Executor,
}

impl std::fmt::Debug for CpalDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpalDriver").finish_non_exhaustive()
    }
}

impl CpalDriver {
    /// A driver over `device` at the sample type `format` of its config.
    pub(crate) fn new(device: cpal::Device, format: DeviceFormat, executor: Executor) -> Self {
        Self {
            device,
            format,
            executor,
        }
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
    ///
    /// The quantum answered is what the built stream's `buffer_size` reports:
    /// `None` when the backend cannot say or says a size [`OutputQuantum`]
    /// refuses. A fixed buffer size the backend refuses is opened again on
    /// its default buffer.
    fn open(
        self,
        spec: &OutputSpec,
        block: OutputBlock<F>,
        faults: Arc<StreamFaults>,
    ) -> Result<OpenedOutput<CpalStream>> {
        let config = stream_config(spec);
        let format = self.format;
        let executor = self.executor.clone();
        // The owner thread's report of the opened frame count (0: unknown),
        // read once the thread has answered that the stream is built.
        let reported = Arc::new(AtomicU32::new(0));
        let report = Arc::clone(&reported);
        let owned = Owned::spawn(&executor, "tutti-output-stream", move || {
            let (stream, quantum) = match format {
                DeviceFormat::I8 => self.build(&config, block, faults, native::<F, i8>)?,
                DeviceFormat::I16 => self.build(&config, block, faults, native::<F, i16>)?,
                DeviceFormat::I32 => self.build(&config, block, faults, native::<F, i32>)?,
                DeviceFormat::U8 => self.build(&config, block, faults, offset::<F, u8>)?,
                DeviceFormat::U16 => self.build(&config, block, faults, offset::<F, u16>)?,
                DeviceFormat::U32 => self.build(&config, block, faults, offset::<F, u32>)?,
                DeviceFormat::F32 => self.build(&config, block, faults, native::<F, f32>)?,
                DeviceFormat::F64 => self.build(&config, block, faults, native::<F, f64>)?,
            };
            stream.play().map_err(DeviceError::PlayStream)?;
            report.store(
                quantum.map_or(0, |q| q.max_block().get_u32()),
                Ordering::Release,
            );
            Ok(stream)
        })?;
        Ok(OpenedOutput {
            stream: CpalStream(owned),
            quantum: opened_quantum(reported.load(Ordering::Acquire)),
        })
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

/// The block an attempt's callback renders with. A stream that is not built
/// drops its callback, and the block goes back to `back` so the next attempt
/// can use it; once the stream is `kept` the block is dropped with the stream.
struct Hold<F: Float> {
    block: Option<OutputBlock<F>>,
    back: Arc<Mutex<Option<OutputBlock<F>>>>,
    kept: Arc<AtomicBool>,
}

impl<F: Float> Drop for Hold<F> {
    fn drop(&mut self) {
        if !self.kept.load(Ordering::Acquire) {
            *self.back.lock() = self.block.take();
        }
    }
}

impl CpalDriver {
    /// Builds the stream, and the callback size it reports. A fixed buffer size
    /// the backend refuses ([`on_build_failure`]) is built once more on the
    /// default buffer, with the same block.
    fn build<T: cpal::SizedSample + 'static, F: Float>(
        &self,
        config: &cpal::StreamConfig,
        block: OutputBlock<F>,
        faults: Arc<StreamFaults>,
        render: Render<F, T>,
    ) -> Result<(cpal::Stream, Option<OutputQuantum>)> {
        let back = Arc::new(Mutex::new(None));
        let kept = Arc::new(AtomicBool::new(false));
        let channels = block.channels();
        let attempt = |config: &cpal::StreamConfig, block| {
            let hold = Hold {
                block: Some(block),
                back: Arc::clone(&back),
                kept: Arc::clone(&kept),
            };
            self.build_once(config, channels, hold, Arc::clone(&faults), render)
        };
        let stream = match attempt(config, block) {
            Err(DeviceError::BuildStream(refused))
                if on_build_failure(config.buffer_size, refused.kind())
                    == Fallback::RetryOnDefault =>
            {
                let recovered = back.lock().take();
                match recovered {
                    Some(block) => {
                        let default = cpal::StreamConfig {
                            buffer_size: cpal::BufferSize::Default,
                            ..*config
                        };
                        attempt(&default, block)?
                    }
                    None => return Err(DeviceError::BuildStream(refused)),
                }
            }
            built => built?,
        };
        kept.store(true, Ordering::Release);
        let quantum = stream.buffer_size().ok().and_then(opened_quantum);
        Ok((stream, quantum))
    }

    fn build_once<T: cpal::SizedSample + 'static, F: Float>(
        &self,
        config: &cpal::StreamConfig,
        channels: usize,
        mut hold: Hold<F>,
        faults: Arc<StreamFaults>,
        render: Render<F, T>,
    ) -> Result<cpal::Stream> {
        let delivering = Arc::clone(&faults);
        self.device
            .build_output_stream(
                *config,
                move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                    // The CPAL contract: a callback no larger than MAX_FRAMES.
                    // The assertion lives HERE, at the boundary CPAL crosses, and
                    // not inside `render_with`: it is a claim about CPAL, not about
                    // the block, and keeping it out is what lets a debug-build
                    // test observe the count instead of panicking before it.
                    debug_assert!(
                        data.len() / channels <= MAX_FRAMES,
                        "CPAL callback frames {} exceeds MAX_FRAMES {MAX_FRAMES}",
                        data.len() / channels
                    );
                    let Some(block) = hold.block.as_mut() else {
                        return;
                    };
                    render(block, data, edge::output_delay(&info.timestamp()));
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
