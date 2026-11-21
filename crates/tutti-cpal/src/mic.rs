//! Microphone capture as a [`Source`] — the input-device twin of the output
//! stream in [`output`](super::output).
//!
//! This is the device crate, so opening the `cpal` stream belongs here and
//! nowhere lower. Everything the mic feeds is device-free and lives lower: the
//! inlet node that copies the monitor ring into the graph (`tutti-graph`'s
//! `InletNode`, opened by `tutti-session`'s `Graph::add_inlet`), the
//! `tutti_codec::FileOut` a take records into, and the pump that moves
//! frames between them (`tutti-session`'s take). What this crate contributes is the one live
//! [`Source`] that touches hardware.
//!
//! # Threading
//!
//! `cpal` runs the input callback on its own real-time thread (the producer); it
//! only ever pushes whole interleaved frames into lock-free SPSC rings (one lane
//! a channel, [`tutti_graph::TapWriter`]), never allocating or blocking. A background pump thread owns the [`MicIn`] (the
//! consumer) and drains the recording ring via [`pull`](Source::pull).
//! If the consumer falls behind, the ring fills and the callback drops the newest
//! frames rather than block the audio thread — an overrun, surfaced as a gap,
//! never a glitch on the output stream.
//!
//! # Live monitoring
//!
//! [`open_with_monitor`](MicIn::open_with_monitor) tees the same capture
//! callback into a *second*, shallow ring and hands back its reader. Give it to
//! `tutti-session`'s `Graph::add_inlet`, which reads the rate off the ring and
//! refuses one that is not the graph's; the inlet node it makes copies the
//! ring into the graph. (`tutti-session`'s `Session::add_input` does both
//! halves, and reopens them after a restart.) Wire that node through effects if you like to hear the
//! mic live while recording the same input. The two rings are independent: the
//! recording ring is deep (dropout-resistant, latency irrelevant to a file); the
//! monitor ring is shallow (low-latency, so you don't hear yourself slapped-back).
//!
//! # Which inputs
//!
//! A device's inputs are numbered, not placed: cpal reports a count and no
//! channel map, so they are [`Speaker::Unassigned`](tutti_buffers::Speaker::Unassigned)
//! and never go through a surround matrix (a 6-input interface is six
//! microphones, not 5.1). The mic delivers the inputs it was opened with:
//! [`MicConfig::inputs`] defaults to the first two (a mono device's one input on
//! both sides) and takes any list of them, so "inputs 3 and 4 of 16" is
//! `Some(vec![2, 3])`. The selection is
//! [`Fold::select`](tutti_buffers::Fold::select), applied in the capture callback.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, StreamTrait};

use tutti_buffers::{ChannelLayout, ChannelTopology, SampleFormat};
use tutti_buffers::{Float, Fold, Interleaved, InterleavedMut, Pull, Source};
use tutti_codec::{FileOut, Format, Spec};
use tutti_graph::{InletRing, TapReader, TapWriter};
use tutti_rt::Drained;
use tutti_units::Samples;
use tutti_units::{SampleRate, Seconds};

use crate::driver_seam::{Fit, device_rate, fit};
use crate::edge;
use crate::error::{DeviceError, Result};
use crate::host::{AudioHost, DeviceHost, DeviceSelector, device_name};
use crate::owner::Owned;
use crate::roster::DeviceDirection;
use tutti_engine::{StreamFaults, StreamLatency};
use tutti_platform::Executor;

/// Capture-ring capacity: one second, held whole at the stream's rate. Large
/// enough that a briefly descheduled pump thread doesn't overrun, small enough
/// to bound latency/RAM. This is the *recording* ring; the pump thread drains
/// it, so a deep buffer trades latency (irrelevant to a file) for dropout
/// resistance.
const CAPTURE_RING: Seconds = Seconds::new(1.0);

/// Monitor-ring depth, 10 ms, at the stream's rate. The audio callback
/// drains this one *per block*, so it needs only enough slack to bridge one
/// buffer's jitter. It is deliberately SHALLOW: a deep monitor ring would be
/// heard as latency (you'd hear yourself slapped-back), and a full ring just
/// drops the oldest-unread frames, which for live monitoring is the right
/// failure — always hear "now", never a growing delay.
pub const MONITOR_RING: Seconds = Seconds::new(0.01);

/// Device frames the capture callback selects and pushes at a time. The
/// callback's scratch is this many frames of the selection, allocated once, so
/// a callback of any size runs in chunks of it.
const CHUNK_FRAMES: usize = 64;

/// Keeps the `cpal` input stream alive on its own thread. The callback runs for
/// as long as this value exists; dropping it stops capture. The field is never
/// read — ownership *is* the API, as with the output side's `CpalStream`.
#[derive(Debug)]
struct StreamHandle(
    #[allow(dead_code, reason = "ownership is the API — held for Drop, never read")] Owned,
);

/// A live microphone as a [`Source`]. Requires the `capture` feature.
///
/// The CPAL input callback selects the inputs the mic was opened with from each
/// device frame and pushes them into a lock-free ring of about one second; this
/// value is the consumer end. Pull from it with [`pull`](Source::pull),
/// or hand it to a `tutti_session::PumpLoop` with a sink from
/// [`matching_sink`](Self::matching_sink) to record. If the consumer falls
/// behind, the callback drops the newest frames rather than block. Capture
/// stops when this value is dropped.
///
/// `F` is the graph's float: the callback decodes each device sample into it by
/// the rule in `Sample`, so an `i32` device keeps its bits in an `f64` graph.
///
/// Its [`layout`](Source::layout) is the number of inputs selected, and its
/// [`topology`](Self::topology) says they feed no speaker.
///
/// [`open_with_monitor`](Self::open_with_monitor) also returns the reader of a
/// second, shallow (about 10 ms) ring fed from the same callback, for an inlet
/// that makes the mic heard live through the graph while it is recorded.
pub struct MicIn<F: Float = f32> {
    tap: TapReader<F>,
    sample_rate: SampleRate,
    /// How many inputs the device has.
    device_inputs: ChannelLayout,
    /// What the input stream's error callback reports into.
    faults: Arc<StreamFaults>,
    /// The delay between a frame's capture and its callback, as the callback
    /// last measured it.
    latency: Arc<StreamLatency>,
    // Held to keep the input stream running; dropped with the source.
    _stream: StreamHandle,
}

impl<F: Float> std::fmt::Debug for MicIn<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicIn")
            .field("sample_rate", &self.sample_rate)
            .field("inputs", &self.tap.layout())
            .field("faults", &self.faults.count())
            .field("_stream", &self._stream)
            .finish_non_exhaustive()
    }
}

/// The inputs [`MicIn::open`] takes of a device of `width` inputs: the first
/// two, or a lone input on both sides.
fn default_inputs(width: ChannelLayout) -> [usize; 2] {
    [0, usize::from(width.count() > 1)]
}

/// What [`MicIn`] opens: the host and device, which inputs, the sample format,
/// and where the capture delay is reported. `Default` is the default host's
/// default input device, its first two inputs, the device's float format when
/// it has one, and a delay cell of the mic's own; build one by struct update or
/// by the chained [`sample_format`](Self::sample_format).
///
/// ```
/// use tutti_cpal::{AudioHost, DeviceSelector, MicConfig};
///
/// let config = MicConfig {
///     host: AudioHost::Jack,
///     device: DeviceSelector::named("system"),
///     inputs: Some(vec![2, 3]), // inputs 3 and 4 of the device
///     ..MicConfig::default()
/// };
/// assert_eq!(config.inputs.as_deref(), Some(&[2, 3][..]));
/// ```
#[derive(Debug, Clone)]
pub struct MicConfig {
    /// The platform host the device is opened through.
    pub host: AudioHost,
    /// The input device on it.
    pub device: DeviceSelector,
    /// The device inputs to capture, counted from 0: destination channel `i` is
    /// the device's input `inputs[i]`. `None`: the first two (a mono device's
    /// one input on both sides).
    pub inputs: Option<Vec<usize>>,
    /// The sample format to open the device at, at the graph's rate. `None`:
    /// `F32` when the device offers it, else its first format. A format the
    /// device does not offer is [`DeviceError::InputFormatUnavailable`], never
    /// another format. An unsigned device format opens as the signed format of
    /// its width (a `U16` device offers `I16`).
    pub sample_format: Option<SampleFormat>,
    /// Where the capture callback reports the delay between a frame's capture
    /// and its callback (`None`: a cell of the mic's own,
    /// [`MicIn::latency`]). A host that reopens its mic hands the same cell to
    /// each, so one reader follows them all.
    pub latency: Option<Arc<StreamLatency>>,
    /// The depth of the monitor ring [`MicIn::open_with_monitor`] makes, at the
    /// stream's rate: shallow on purpose, so a monitored input is heard now and
    /// not late. [`MONITOR_RING`] (10 ms) by default.
    pub monitor_depth: Seconds,
}

impl Default for MicConfig {
    fn default() -> Self {
        Self {
            host: AudioHost::Default,
            device: DeviceSelector::Default,
            inputs: None,
            sample_format: None,
            latency: None,
            monitor_depth: MONITOR_RING,
        }
    }
}

impl MicConfig {
    /// Opens the device at `format` ([`sample_format`](Self::sample_format)).
    #[must_use]
    pub fn sample_format(mut self, format: SampleFormat) -> Self {
        self.sample_format = Some(format);
        self
    }
}

impl<F: Float> MicIn<F> {
    /// Opens the input device `config` names at the graph's sample rate and
    /// starts capturing its inputs on a dedicated thread of `executor`. Returns once the
    /// stream is live.
    ///
    /// The rate is a parameter rather than read off the device because nothing
    /// downstream resamples: an inlet renders the mic into the graph
    /// as-is, so the device must run at `graph_rate`. A device whose supported
    /// range covers `graph_rate` is opened at it, at the format
    /// [`MicConfig::sample_format`] chose (preferring `f32` samples when none
    /// is chosen); otherwise its default config is used if it already matches.
    ///
    /// Asking for a rate does not prove a device produces it natively: ALSA
    /// plug devices, for one, advertise wide ranges and resample internally.
    ///
    /// # Errors
    /// [`DeviceError::HostUnavailable`] if the host cannot be opened;
    /// [`DeviceError::SampleRateMismatch`] if the device cannot run at
    /// `graph_rate`; [`DeviceError::InputFormatUnavailable`] if it runs at
    /// `graph_rate` but not at the chosen format; [`DeviceError::InvalidDevice`] if the device does not resolve or
    /// its configs cannot be queried; [`DeviceError::DeviceNotAvailable`] if it has
    /// no default input config; [`DeviceError::InvalidConfig`] for an unsupported
    /// sample format; [`DeviceError::BuildStream`] / [`DeviceError::PlayStream`] from
    /// CPAL; [`DeviceError::Inputs`] for an empty list of inputs or one the device
    /// does not have.
    pub fn open(executor: &Executor, config: &MicConfig, graph_rate: SampleRate) -> Result<Self> {
        Self::open_inner(executor, config, graph_rate, false).map(|(mic, _)| mic)
    }

    /// Opens the mic like [`open`](Self::open), and also returns the reader of
    /// a live-monitor ring fed by the same capture callback (one lane a
    /// selected input).
    ///
    /// Each captured frame goes into both the recording ring (drained by
    /// [`pull`](Source::pull)) and a shallow monitor ring. Give the
    /// monitor reader to `Graph::add_inlet` (`tutti-session`), with this mic's
    /// [`sample_rate`](Self::sample_rate), to hear the mic live, through
    /// effects if wanted, while recording the same input. The rings are
    /// independent: when one is full it drops frames without stalling the
    /// other or the capture thread.
    ///
    /// # Errors
    /// As [`open`](Self::open).
    pub fn open_with_monitor(
        executor: &Executor,
        config: &MicConfig,
        graph_rate: SampleRate,
    ) -> Result<(Self, InletRing<F>)> {
        Self::open_inner(executor, config, graph_rate, true).map(Self::with_ring)
    }

    fn with_ring((source, ring): (Self, Option<InletRing<F>>)) -> (Self, InletRing<F>) {
        (source, ring.expect("a monitor was asked for"))
    }

    fn open_inner(
        executor: &Executor,
        mic: &MicConfig,
        graph_rate: SampleRate,
        monitor: bool,
    ) -> Result<(Self, Option<InletRing<F>>)> {
        let host = DeviceHost::open(mic.host)?;
        let device = host.device(DeviceDirection::Input, &mic.device)?;
        let inputs = mic.inputs.as_deref();
        let latency = mic.latency.clone().unwrap_or_default();
        let name = device_name(&device).unwrap_or_default();
        let config = choose_input_config(
            device.supported_input_configs().map_err(|e| {
                DeviceError::InvalidDevice(format!("cannot query {name:?} input configs: {e}"))
            })?,
            device
                .default_input_config()
                .map_err(DeviceError::DeviceNotAvailable)?,
            graph_rate,
            mic.sample_format,
            &name,
        )?;
        let sample_rate = device_rate(config.sample_rate())?;
        let device_inputs = ChannelLayout::from(config.channels());

        // Checked before the stream exists: an input the device lacks is an
        // error here, not silence in the callback.
        let select = match inputs {
            Some(list) => Fold::select(device_inputs, list),
            None => Fold::select(device_inputs, &default_inputs(device_inputs)),
        }?;
        let width = select.dst_layout();

        let faults = Arc::new(StreamFaults::new());
        let (reader, rec) = TapReader::pair(
            usize::from(width.count()),
            CAPTURE_RING.to_samples_ceil(sample_rate),
        );
        let (ring, mon) = if monitor {
            // The ring carries the rate the stream runs at, so whoever takes it
            // into a graph can check it there.
            let (ring, writer) = InletRing::pair(
                width,
                mic.monitor_depth.to_samples_ceil(sample_rate),
                sample_rate,
            );
            (Some(ring), Some(writer))
        } else {
            (None, None)
        };
        let capture = Capture::new(select, device_inputs, rec, mon);

        let sample_format = config.sample_format();
        let cfg: cpal::StreamConfig = config.into();
        if !matches!(
            sample_format,
            cpal::SampleFormat::I8
                | cpal::SampleFormat::I16
                | cpal::SampleFormat::I32
                | cpal::SampleFormat::U8
                | cpal::SampleFormat::U16
                | cpal::SampleFormat::U32
                | cpal::SampleFormat::F32
                | cpal::SampleFormat::F64
        ) {
            return Err(DeviceError::InvalidConfig(format!(
                "Unsupported input sample format: {sample_format:?}"
            )));
        }
        let cb_faults = Arc::clone(&faults);
        let cb_latency = Arc::clone(&latency);
        let owned = Owned::spawn(executor, "tutti-input-stream", move || {
            let (d, c) = (&device, &cfg);
            let stream = match sample_format {
                cpal::SampleFormat::I8 => build_input(
                    d,
                    c,
                    capture,
                    cb_faults,
                    cb_latency,
                    edge::from_native::<i8, F>,
                )?,
                cpal::SampleFormat::I16 => build_input(
                    d,
                    c,
                    capture,
                    cb_faults,
                    cb_latency,
                    edge::from_native::<i16, F>,
                )?,
                cpal::SampleFormat::I32 => build_input(
                    d,
                    c,
                    capture,
                    cb_faults,
                    cb_latency,
                    edge::from_native::<i32, F>,
                )?,
                cpal::SampleFormat::U8 => build_input(
                    d,
                    c,
                    capture,
                    cb_faults,
                    cb_latency,
                    edge::from_offset::<u8, F>,
                )?,
                cpal::SampleFormat::U16 => build_input(
                    d,
                    c,
                    capture,
                    cb_faults,
                    cb_latency,
                    edge::from_offset::<u16, F>,
                )?,
                cpal::SampleFormat::U32 => build_input(
                    d,
                    c,
                    capture,
                    cb_faults,
                    cb_latency,
                    edge::from_offset::<u32, F>,
                )?,
                cpal::SampleFormat::F32 => build_input(
                    d,
                    c,
                    capture,
                    cb_faults,
                    cb_latency,
                    edge::from_native::<f32, F>,
                )?,
                cpal::SampleFormat::F64 => build_input(
                    d,
                    c,
                    capture,
                    cb_faults,
                    cb_latency,
                    edge::from_native::<f64, F>,
                )?,
                // Checked above, before the thread is spawned.
                _ => unreachable!("sample format checked before spawn"),
            };
            stream.play().map_err(DeviceError::PlayStream)?;
            Ok(stream)
        })?;

        let mic = Self {
            tap: reader,
            sample_rate,
            device_inputs,
            faults,
            latency,
            _stream: StreamHandle(owned),
        };
        Ok((mic, ring))
    }

    /// The faults the input stream has reported, shared with its error callback.
    ///
    /// Take it before handing the mic to a `tutti_session::PumpLoop`, which owns the
    /// `MicIn`. A disconnect also ends the take: the pump's
    /// `failed()` turns `true` once the buffered frames are written. Nothing
    /// else observes the mic; a host that wants to report it polls this handle,
    /// as it does the output's.
    #[must_use]
    pub fn faults(&self) -> Arc<StreamFaults> {
        Arc::clone(&self.faults)
    }

    /// The delay between a frame's capture and the callback that delivered it,
    /// as the last callback measured it: the cell a host reads, `None` while
    /// no callback has reported one. It is the one [`MicConfig::latency`]
    /// named if it named one.
    #[must_use]
    pub fn latency(&self) -> Arc<StreamLatency> {
        Arc::clone(&self.latency)
    }

    /// Returns the rate the capture stream runs at (the `graph_rate` it was
    /// opened with). A recorder passes this to its sink so the WAV header
    /// matches the frames it is fed.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// How many inputs the device has, of which this mic delivers
    /// [`layout`](Source::layout)'s.
    #[must_use]
    pub fn device_inputs(&self) -> ChannelLayout {
        self.device_inputs
    }

    /// Which speaker each delivered channel feeds: none, they are numbered
    /// device inputs ([`Speaker::Unassigned`](tutti_buffers::Speaker::Unassigned)).
    /// [`Fold::between`] this to the topology a mic should be placed in.
    #[must_use]
    pub fn topology(&self) -> ChannelTopology {
        ChannelTopology::unassigned(self.tap.layout())
    }

    /// Creates a file sink that matches this mic: its sample rate, its
    /// layout (one channel a selected input), in `format`.
    ///
    /// A source and sink must agree on rate and channel count, and nothing
    /// downstream can check the rate: [`Source`] carries none, so an 8 kHz
    /// sink fed by a 48 kHz mic writes a valid WAV that plays six times too
    /// slow. Building the sink here rules that out.
    ///
    /// # Errors
    ///
    /// Whatever [`FileOut::create`] reports: the file could not be created, the
    /// format cannot be written at this width, or the header not written.
    pub fn matching_sink(
        &self,
        path: impl AsRef<std::path::Path>,
        format: Format,
    ) -> tutti_codec::Result<FileOut> {
        // Read off `Source::layout` rather than hard-coded, so this pairing
        // cannot drift from what `pull` actually hands back; a sink built
        // from a *different* width than the source reports is exactly what
        // `PumpLoop::new` refuses, and this helper exists to be the pairing
        // that always passes.
        FileOut::create(
            &Spec::of_width(format, Source::layout(self), self.sample_rate),
            path,
        )
    }
}

impl<F: Float> Source for MicIn<F> {
    type Sample = F;

    /// One channel a selected input (two for [`MicIn::open`]), whatever the
    /// device's own width is.
    fn layout(&self) -> ChannelLayout {
        self.tap.layout()
    }

    /// Pops up to `out.len()` FRAMES the callback has pushed.
    ///
    /// A live capture device: an empty ring means the callback has not pushed
    /// since the last pull, so [`Starved`](Pull::Starved), never
    /// [`Ended`](Pull::Ended): a consumer that stopped there would end a
    /// recording within milliseconds of starting it.
    ///
    /// [`Failed`](Pull::Failed) means the stream is gone (the backend reported
    /// the device disconnected, or the capture callback panicked; any other
    /// backend error may be transient and does not fail the source) **and** the
    /// ring is empty. The fault flag is read before the ring, so frames the
    /// callback pushed before the fault are never hidden behind it.
    fn pull(&mut self, mut out: InterleavedMut<'_, F>) -> Pull {
        if out.layout() != self.tap.layout() {
            return Pull::Failed;
        }
        let tap = &mut self.tap;
        match self.faults.drain(|| {
            // The width was checked above, so the pop is refused never.
            let n = tap.pop_into(out.reborrow()).ok()?;
            (!n.is_zero()).then_some(n)
        }) {
            Drained::Item(n) => Pull::Frames(n),
            Drained::Empty => Pull::Starved,
            Drained::Failed(()) => Pull::Failed,
        }
    }

    /// The frames the capture callback dropped on a full ring since the last
    /// call: the recording closes up over them. (Counted in the callback by the
    /// ring's writer.)
    fn take_lost(&mut self) -> Samples {
        TapReader::take_lost(&self.tap)
    }
}

/// The capture callback's state: what it selects, and where it writes.
///
/// Built on the control side, so everything the callback touches is allocated
/// before the stream runs: it only reads, folds and pushes.
struct Capture<F: Float> {
    /// The selection: destination channel `i` is device input `inputs[i]`.
    select: Fold<F>,
    /// A chunk of device frames in the graph's float, [`CHUNK_FRAMES`] of them.
    device: Vec<F>,
    /// A chunk of selected frames, [`CHUNK_FRAMES`] of them.
    chunk: Vec<F>,
    /// The recording ring's writer.
    rec: TapWriter<F>,
    /// The monitor ring's writer, when monitoring.
    mon: Option<TapWriter<F>>,
}

impl<F: Float> Capture<F> {
    fn new(
        select: Fold<F>,
        device: ChannelLayout,
        rec: TapWriter<F>,
        mon: Option<TapWriter<F>>,
    ) -> Self {
        let frames = Samples::new(CHUNK_FRAMES);
        let selected = select.dst_layout();
        Self {
            select,
            device: vec![F::ZERO; device.interleaved_len(frames)],
            chunk: vec![F::ZERO; selected.interleaved_len(frames)],
            rec,
            mon,
        }
    }

    /// Selects the inputs of every whole device frame in `data` and pushes the
    /// result to the rings, a chunk of frames at a time. A full ring refuses
    /// whole frames and counts them lost (the writer does); neither ring can
    /// stall the other or the caller. Allocation-free.
    fn block<T: Copy>(&mut self, data: &[T], convert: fn(T) -> F) {
        let device = self.select.src_layout();
        let selected = self.select.dst_layout();
        let whole = device.interleaved_len(device.from_interleaved_len(data.len()));
        let run_len = device.interleaved_len(Samples::new(CHUNK_FRAMES)).max(1);
        for run in data[..whole].chunks(run_len) {
            let frames = device.from_interleaved_len(run.len());
            let held = &mut self.device[..run.len()];
            for (slot, s) in held.iter_mut().zip(run) {
                *slot = convert(*s);
            }
            let pushed = &mut self.chunk[..selected.interleaved_len(frames)];
            // The chunks are sized to the run, so the views agree in width and frames.
            let folded = Interleaved::new(held, device).and_then(|src| {
                let dst = InterleavedMut::new(&mut *pushed, selected)?;
                self.select.fold(src, dst)
            });
            debug_assert!(folded.is_ok(), "the capture chunks are sized to the run");
            let Ok(pushed) = Interleaved::new(pushed, selected) else {
                continue;
            };
            self.rec.push_interleaved(pushed);
            // The monitor ring is shallow (~10ms) so it never builds a backlog
            // of latency, and a full one drops this chunk (the graph consumer
            // briefly fell behind); the node reads silence for those.
            if let Some(mon) = &mut self.mon {
                mon.push_interleaved(pushed);
            }
        }
    }
}

/// Build the `cpal` input stream: the callback runs [`Capture::block`] over
/// each device buffer. Alloc-free and non-blocking — a full ring drops the
/// frames (overrun) rather than stall, so neither tap can ever block the RT
/// input thread or the other tap.
///
/// # Selecting, not truncating or folding
///
/// A device's inputs are numbered, not placed, so a wide capture is not
/// matrixed (a six-input interface is not 5.1) and its leading channels are
/// not all there is: the mic delivers exactly the inputs it was opened with,
/// whatever their number, by [`Fold::select`].
///
/// # Why the scratch is allocated before the stream runs
///
/// `data: &[T]` is generic over [`cpal::SizedSample`], so the samples must be
/// converted to the graph's float (by the rule in `Sample`, through the signed format of an offset-binary one) before the selection can see them — a per-frame convert
/// is unavoidable. **This is the RT input callback, so it must not allocate**:
/// the device frame and the chunk are `Vec`s sized once in [`Capture::new`] to
/// the device's width and the selection's, and never grown.
fn build_input<T: cpal::SizedSample + 'static, F: Float>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut capture: Capture<F>,
    faults: Arc<StreamFaults>,
    latency: Arc<StreamLatency>,
    convert: fn(T) -> F,
) -> Result<cpal::Stream> {
    let delivering = Arc::clone(&faults);
    // Set by a panic in `capture.block`, whose state may be torn: the capture
    // then does nothing, and its source answers `Failed` (the faults' latch).
    let mut failed = false;
    let stream = device
        .build_input_stream(
            *config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                if failed {
                    return;
                }
                delivering.delivered();
                latency.record(edge::input_delay(&info.timestamp()));
                // A panic must not unwind out of the device's C frame.
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    capture.block(data, convert);
                }));
                if let Err(payload) = ran {
                    failed = true;
                    delivering.record_panic("the capture callback panicked");
                    // Dropping the payload would free on the audio thread.
                    std::mem::forget(payload);
                }
            },
            // Not exercised without a device: what it calls, `StreamFaults::record`
            // and `MicIn::pull`'s verdict, is tested on its own.
            move |err| edge::record(&faults, &err),
            None,
        )
        .map_err(DeviceError::BuildStream)?;
    Ok(stream)
}

/// Choose an input configuration that runs at `graph_rate`, or say why none
/// can.
///
/// Free and pure so it is testable with **no device**: cpal's
/// `SupportedStreamConfigRange::new` is public, so a fixture can state a
/// device's capabilities directly.
///
/// The rules, in order:
/// 1. a supported range whose `[min, max]` contains `graph_rate` and whose
///    format opens as `wanted` (a signed type before its offset-binary twin);
///    with no `wanted`, `F32`, the float format, which needs no integer
///    scaling, else the first such range;
/// 2. otherwise the device's own default, if it matches the rate and the
///    format;
/// 3. otherwise [`DeviceError::InputFormatUnavailable`] when a range covers the rate
///    but none offers `wanted`, else [`DeviceError::SampleRateMismatch`], naming
///    both rates.
pub(crate) fn choose_input_config(
    supported: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    fallback: cpal::SupportedStreamConfig,
    graph_rate: SampleRate,
    wanted: Option<SampleFormat>,
    device_name: &str,
) -> Result<cpal::SupportedStreamConfig> {
    let rate = graph_rate.get().round() as cpal::SampleRate;

    let mut best: Option<(Fit, cpal::SupportedStreamConfig)> = None;
    let mut covered = false;
    for range in supported {
        if range.min_sample_rate() > rate || range.max_sample_rate() < rate {
            continue;
        }
        covered = true;
        let candidate = range.with_sample_rate(rate);
        let Some(by) = (match wanted {
            Some(format) => fit(candidate.sample_format(), format),
            // Prefer F32: `build_input` scales every integer format per
            // sample, and this one is already a float.
            None if candidate.sample_format() == cpal::SampleFormat::F32 => {
                return Ok(candidate);
            }
            None => Some(Fit::Exact),
        }) else {
            continue;
        };
        if best.as_ref().is_none_or(|(held, _)| by < *held) {
            best = Some((by, candidate));
        }
    }
    if let Some((_, c)) = best {
        return Ok(c);
    }

    if fallback.sample_rate() == rate
        && wanted.is_none_or(|w| fit(fallback.sample_format(), w).is_some())
    {
        return Ok(fallback);
    }

    match wanted {
        Some(format) if covered || fallback.sample_rate() == rate => {
            Err(DeviceError::InputFormatUnavailable {
                device_name: device_name.to_string(),
                format,
                rate: graph_rate,
            })
        }
        _ => Err(DeviceError::SampleRateMismatch {
            device_name: device_name.to_string(),
            device: device_rate(fallback.sample_rate())?,
            graph: graph_rate,
        }),
    }
}

#[cfg(test)]
mod tests;
