//! [`SessionBuilder`]: one ordered, fallible build of a graph, its engine and
//! the nodes the engine inserts, offline or on a device.

use std::num::NonZeroUsize;
use std::sync::Arc;

use tutti_buffers::MAX_CHANNELS;
use tutti_buffers::{ChannelLayout, ChannelTopology, MaxBlock};
use tutti_graph::NodeKey;
use tutti_nodes::{ClickNode, ClickNodeConfig, ClickSettings};
use tutti_timeline::Transport;
use tutti_units::SampleRate;

use crate::graph::{Graph, LIVE_MAX_BLOCK};
use tutti_buffers::Float;

use crate::{Clock, Offline, OfflineSession, Result, Session, SessionFloat, SystemClock};
use tutti_engine::BlockDates;

/// Builds a [`Session`], declaratively: chain the options, then
/// [`build`](SessionBuilder::build).
///
/// The builder's type parameter is the session's [`Clock`], what runs its
/// blocks, and a setter that only one clock can honour exists only on that
/// one, so the others do not compile.
///
/// - [`Offline`] (what [`Session::builder`] returns): the host pulls blocks
///   itself with [`Session::render`], from a game's or a plugin's own
///   real-time callback or from a loop that renders faster than real time.
///   It sets the [`sample_rate`](SessionBuilder::sample_rate) (default
///   48 kHz), the [`outputs`](SessionBuilder::outputs) (default stereo) and the
///   [`max_block`](SessionBuilder::max_block) (default
///   [`LIVE_MAX_BLOCK`], 1024 frames), and `build` returns an
///   [`OfflineSession`].
/// - [`SystemClock`] (after [`system_clock`](SessionBuilder::system_clock)): an
///   OS timer thread steps the blocks, with no sound card, for an app that
///   plays MIDI. It takes the same three setters as `Offline`, and its
///   [`max_block`](SessionBuilder::max_block) is the timer's block. `build`
///   returns a [`SystemSession`](crate::SystemSession) with the timer running.
/// - `DeviceClock` (after `device`, feature `device`): the sound card sets the
///   rate, the width and the callback, so none of those is a setter, and
///   `build` returns a `LiveSession` with the stream running.
///
/// Everything else (workers, MIDI, plugins, the root topology) is
/// set the same way on every clock.
///
/// ```
/// use tutti_units::{SampleRate, Samples};
/// use tutti_buffers::{ChannelLayout, MaxBlock};
/// use tutti_session::{Session, Workers};
///
/// let session = Session::builder()
///     .sample_rate(SampleRate::SR_44K1)
///     .max_block(MaxBlock::of(Samples::new(512)))
///     .outputs_floor(ChannelLayout::from_count(6)) // a 5.1 project renders six, whatever the device
///     .render_workers(Workers::SERIAL)
///     .build()
///     .unwrap();
/// assert_eq!(session.graph_ref().outputs(), 6);
/// ```
pub struct SessionBuilder<C: Clock = Offline> {
    opts: Options,
    config: C::Config,
    dates: C::Dates,
}

impl<C: Clock> std::fmt::Debug for SessionBuilder<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionBuilder")
            .field("opts", &self.opts)
            .field("config", &self.config)
            .field("dates", &self.dates)
            .finish()
    }
}

/// The size of the blocks a clock steps, which a session that is not on a
/// device sets itself: [`Offline`] and [`SystemClock`] share it (their
/// [`Clock::Config`]).
#[derive(Clone, Copy, Debug)]
pub struct BlockConfig {
    rate: SampleRate,
    outputs: ChannelLayout,
    max_block: MaxBlock,
}

impl Default for BlockConfig {
    fn default() -> Self {
        Self {
            rate: SampleRate::SR_48K,
            outputs: ChannelLayout::STEREO,
            max_block: LIVE_MAX_BLOCK,
        }
    }
}

/// What every build shares, whichever state the builder is in.
#[derive(Debug)]
pub(crate) struct Options {
    inputs: ChannelLayout,
    outputs_floor: ChannelLayout,
    root: Option<ChannelTopology>,
    /// What the output device's channels are, when the host states it
    /// ([`device_topology`](SessionBuilder::device_topology)).
    device_topology: Option<ChannelTopology>,
    render_workers: Workers,
    /// The platform host the output and every input the session opens use.
    #[cfg(feature = "device")]
    pub(crate) host: tutti_cpal::AudioHost,
    /// Where the session's inputs open, if not the sound card
    /// ([`input_opener`](Self::input_opener)).
    #[cfg(feature = "capture")]
    pub(crate) input_opener: Option<Box<dyn crate::input::Openers>>,
    render_worker_start: Option<crate::graph::RenderWorkerStart>,
    /// The OS MIDI layer's options when it is on
    /// ([`midi_hardware`](Self::midi_hardware)); `None` is no OS MIDI layer.
    #[cfg(feature = "midi-hardware")]
    midi_hardware: Option<tutti_midi_hardware::MidiHardwareConfig>,
    /// The backend the OS MIDI layer runs over, if not the platform's.
    #[cfg(feature = "midi-hardware")]
    midi_backend: Option<MidiBackend>,
    #[cfg(feature = "midi")]
    pub(crate) mpe_mode: tutti_midi_protocol::MpeMode,
    #[cfg(feature = "midi")]
    midi_ports: tutti_midi_runtime::MidiInputPorts,
    unresolved_warning: std::time::Duration,
    pub(crate) plugins: crate::session::parts::PluginOptions,
}

/// A MIDI backend a host supplies, held until the session takes it.
#[cfg(feature = "midi-hardware")]
struct MidiBackend(Box<dyn tutti_midi_hardware::MidiEndpoints>);

#[cfg(feature = "midi-hardware")]
impl std::fmt::Debug for MidiBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiBackend").finish_non_exhaustive()
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            inputs: ChannelLayout::EMPTY,
            outputs_floor: ChannelLayout::STEREO,
            root: None,
            device_topology: None,
            render_workers: Workers::SERIAL,
            #[cfg(feature = "device")]
            host: tutti_cpal::AudioHost::Default,
            #[cfg(feature = "capture")]
            input_opener: None,
            render_worker_start: None,
            #[cfg(feature = "midi-hardware")]
            midi_hardware: None,
            #[cfg(feature = "midi-hardware")]
            midi_backend: None,
            #[cfg(feature = "midi")]
            mpe_mode: tutti_midi_protocol::MpeMode::Disabled,
            #[cfg(feature = "midi")]
            midi_ports: tutti_midi_runtime::MidiInputPorts::FIRST_GROUP,
            unresolved_warning: crate::modulation::UNRESOLVED_WARNING,
            plugins: Default::default(),
        }
    }
}

/// How many threads render a block, the caller's included
/// ([`SessionBuilder::render_workers`]).
///
/// Serial is [`SERIAL`](Self::SERIAL), a constant and the default: there is
/// no separate variant for it, so there is one spelling of "the callback
/// alone" (`Exactly(1)` *is* it). A pool of more threads renders every block
/// bit-identically to the serial one; it pays when the graph is wide and its
/// nodes heavy.
///
/// ```
/// use tutti_session::Workers;
///
/// assert_eq!(Workers::default(), Workers::SERIAL);
/// assert_eq!(Workers::of(4).threads(), 4);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Workers {
    /// One thread for each core the OS reports (one, when it reports none).
    PerCore,
    /// This many threads. One renders on the caller alone.
    Exactly(NonZeroUsize),
}

impl Workers {
    /// The callback alone: the default.
    pub const SERIAL: Self = Self::Exactly(NonZeroUsize::MIN);

    /// `threads` threads, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `threads` is zero.
    #[must_use]
    pub const fn of(threads: usize) -> Self {
        match NonZeroUsize::new(threads) {
            Some(n) => Self::Exactly(n),
            None => panic!("a render pool has at least one thread: the caller's"),
        }
    }

    /// The thread count this asks for, resolved now (`PerCore` reads the
    /// OS's figure).
    #[must_use]
    pub fn threads(self) -> usize {
        match self {
            Self::PerCore => std::thread::available_parallelism().map_or(1, NonZeroUsize::get),
            Self::Exactly(n) => n.get(),
        }
    }
}

impl Default for Workers {
    fn default() -> Self {
        Self::SERIAL
    }
}

/// What every build path shares: the graph, the transport, the click, the
/// MIDI nodes, and what the engine's callback state holds.
pub(crate) struct Parts<F: Float> {
    /// The session's executor, the one the MIDI hardware opened on: `session()`
    /// clones it into the session's background work, sampler and plugins.
    executor: tutti_platform::Executor,
    /// The thread that drives `executor`; the session's background work takes it over in `session()`.
    background: crate::background::Running,
    graph: Graph<F>,
    transport: Transport,
    click: Option<NodeKey>,
    click_settings: Arc<ClickSettings>,
    #[cfg(feature = "midi")]
    pub(crate) midi: Option<crate::MidiNodes>,
    #[cfg(feature = "midi-hardware")]
    pub(crate) midi_io: Option<tutti_midi_hardware::MidiSession>,
    #[cfg(feature = "midi-hardware")]
    pub(crate) device_events: Option<tutti_midi_hardware::DeviceEvents>,
}

impl<F: SessionFloat> SessionBuilder<Offline<F>> {
    /// The defaults: [`Session::builder`] is the way in.
    pub(crate) fn new() -> Self {
        Self {
            opts: Options::default(),
            config: BlockConfig::default(),
            dates: BlockDates::Os,
        }
    }
}

impl<C: Clock<Config = BlockConfig>> SessionBuilder<C> {
    /// The rate the graph runs at. Default 48 kHz.
    #[must_use]
    pub fn sample_rate(mut self, rate: SampleRate) -> Self {
        self.config.rate = rate;
        self
    }

    /// The width of every block the host pulls with [`Session::render`] (or
    /// the system clock renders). The graph root is
    /// [`root_width`]`(outputs_floor, outputs)` wide. Default stereo.
    #[must_use]
    pub fn outputs(mut self, outputs: ChannelLayout) -> Self {
        self.config.outputs = outputs;
        self
    }

    /// The most frames in one graph block: a longer block the host pulls
    /// renders as consecutive graph blocks. On a [`SystemClock`] it is also
    /// the timer's block: one timer wake-up renders this many frames.
    /// Default [`LIVE_MAX_BLOCK`].
    #[must_use]
    pub fn max_block(mut self, max_block: MaxBlock) -> Self {
        self.config.max_block = max_block;
        self
    }
}

impl<F: SessionFloat> SessionBuilder<Offline<F>> {
    /// The float format of the session's audio, `G` in place of `F`: every
    /// option set so far carries over. Default `f32`.
    ///
    /// It is the one place the choice is made: [`system_clock`](Self::system_clock)
    /// and `device` carry the float, so
    /// `.float::<f64>().device(..)` reaches every clock.
    ///
    /// ```
    /// use tutti_session::Session;
    /// use tutti_units::SampleRate;
    ///
    /// let session: tutti_session::OfflineSession<&str, f64> = Session::builder()
    ///     .float::<f64>()
    ///     .sample_rate(SampleRate::SR_44K1)
    ///     .build()
    ///     .unwrap();
    /// ```
    #[must_use]
    pub fn float<G: SessionFloat>(self) -> SessionBuilder<Offline<G>> {
        SessionBuilder {
            opts: self.opts,
            config: self.config,
            dates: self.dates,
        }
    }

    /// Steps the blocks with an OS timer thread instead: no sound card, for an
    /// app that plays MIDI. The rate, the width and the block carry over, and
    /// every other option with them.
    #[must_use]
    pub fn system_clock(self) -> SessionBuilder<SystemClock<F>> {
        SessionBuilder {
            opts: self.opts,
            config: self.config,
            dates: (),
        }
    }

    /// Plays on the output device `device` selects instead: the device sets
    /// the rate, the width and the callback size, so the builder loses
    /// [`sample_rate`](Self::sample_rate), [`outputs`](Self::outputs) and
    /// [`max_block`](Self::max_block), and every other option carries over.
    ///
    /// ```compile_fail
    /// use tutti_session::Session;
    /// use tutti_units::SampleRate;
    ///
    /// // The device's rate is the session's: there is nothing to set.
    /// let _ = Session::builder()
    ///     .device(tutti_cpal::DeviceSelector::Default)
    ///     .sample_rate(SampleRate::SR_44K1);
    /// ```
    #[cfg(feature = "device")]
    #[must_use]
    pub fn device(
        self,
        device: tutti_cpal::DeviceSelector,
    ) -> SessionBuilder<crate::DeviceClock<F>> {
        SessionBuilder {
            opts: self.opts,
            config: tutti_cpal::OutputConfig::from(device),
            dates: (),
        }
    }
}

impl<F: SessionFloat> SessionBuilder<Offline<F>> {
    /// Dates each block the host renders by `dates` ([`BlockDates`]), where the
    /// default, [`BlockDates::Os`], reads the OS clock. An offline render dated
    /// by [`BlockDates::Frames`] is the same on every run, whatever the host's
    /// wall clock does. The dates belong to the offline builder:
    /// [`system_clock`](Self::system_clock) and `device` sessions are timed by
    /// the OS and take none.
    #[must_use]
    pub fn block_dates(mut self, dates: BlockDates) -> Self {
        self.dates = dates;
        self
    }

    /// An offline session: everything but a stream. The host pulls blocks
    /// with [`Session::render`] (`Engine::process`), each up to
    /// [`max_block`](Self::max_block) frames per graph block (a longer one
    /// renders as consecutive graph blocks),
    /// [`outputs`](Self::outputs) wide; the graph root is
    /// [`root_width`]`(outputs_floor, outputs)`.
    ///
    /// # Errors
    ///
    /// When the engine refuses the graph, `outputs` has no channels or cannot take
    /// the root ([`Error::Output`](crate::Error::Output)),
    /// (`midi-hardware`) the MIDI device watch cannot start, or (`sampler`) the disk streamer cannot start.
    pub fn build(mut self) -> Result<OfflineSession<&'static str, F>> {
        let BlockConfig {
            rate,
            outputs: layout,
            max_block,
        } = self.config;
        let root = self.root_width(layout);
        self.check_placement(root, layout)?;
        let runtime = Self::session_executor()?;
        let mut parts = self.parts(runtime, rate, max_block, None, root)?;
        // The width of every block the host pulls.
        let engine = parts.graph.engine(
            &parts.transport,
            layout,
            self.opts.device_topology.clone(),
            self.dates,
        )?;
        Self::clock_midi(&parts, &engine);
        self.declare(&mut parts.graph)?;
        let taps = engine.heard_taps();
        let readings = (engine.load(), engine.blocks());
        let clock = engine.block_clock();
        let mut session = self.session(parts, crate::clock::OfflineEngine { engine }, true)?;
        session.output.readings = Some(readings);
        session.transport.handle.set_clock(clock);
        taps.notify_with(session.background.bells().handle());
        session.output.taps = Some((taps, layout));
        Ok(session)
    }

    /// A session with no engine: the executor stays on this side, every
    /// commit is applied at once, and `Session::render_frame` renders it
    /// (or `Graph::take_audio_side` hands it to a test). No metronome and
    /// no MIDI nodes are inserted, and no disk streamer is started: the
    /// graph is exactly `outputs_floor` wide (a declared
    /// [`root_topology`](Self::root_topology)'s width, if there is one), empty,
    /// at `rate`, in blocks of [`LIVE_MAX_BLOCK`]. Behind `testing`: what a
    /// host wants is [`build`](Self::build).
    ///
    /// # Errors
    ///
    /// [`Error::Output`](crate::Error::Output) when the declared root topology is empty or wider
    /// than the engine renders; there is no device to place it on.
    #[cfg(feature = "testing")]
    pub fn build_detached(
        mut self,
        rate: SampleRate,
    ) -> Result<Session<&'static str, crate::Detached<F>>> {
        let outputs = self.opts.root.as_ref().map_or_else(
            || usize::from(self.opts.outputs_floor.count()),
            |r| usize::from(r.width().count()),
        );
        let transport = Transport::new(rate);
        let mut graph = Graph::new(
            self.opts.inputs,
            outputs,
            rate,
            LIVE_MAX_BLOCK,
            None,
            transport.settings().tempo_reader(),
        );
        graph.set_render_workers(self.opts.render_workers);
        graph.declare_root(self.opts.root.clone(), true)?;
        let (executor, background) = Self::session_executor()?;
        self.session(
            Parts {
                executor,
                background,
                graph,
                transport,
                click: None,
                click_settings: Arc::new(ClickSettings::new()),
                #[cfg(feature = "midi")]
                midi: None,
                #[cfg(feature = "midi-hardware")]
                midi_io: None,
                #[cfg(feature = "midi-hardware")]
                device_events: None,
            },
            crate::clock::DetachedGraph,
            false,
        )
    }
}

impl<F: SessionFloat> SessionBuilder<SystemClock<F>> {
    /// A session whose blocks an OS timer thread renders, with no sound card:
    /// the graph runs and its MIDI leaves, and nothing plays. The thread is
    /// running on return; the [`SystemSession`](crate::SystemSession) holds it, and dropping the
    /// session stops it.
    ///
    /// The timer renders block `n` one block before it would play, so the
    /// MIDI it makes leaves with a block's lead, and the session reports no
    /// audio output latency. Its block is [`max_block`](Self::max_block).
    ///
    /// # Errors
    ///
    /// When the engine refuses the graph, `outputs` has no channels or cannot
    /// take the root ([`Error::Output`](crate::Error::Output)),
    /// (`midi-hardware`) the MIDI device watch cannot start, (`sampler`) the
    /// disk streamer cannot start, or the OS refuses the timer thread
    /// ([`Error::Clock`](crate::Error::Clock)), or [`max_block`](Self::max_block) is past
    /// the callback ceiling ([`Error::Quantum`](crate::Error::Quantum)). A target that starts no thread (the web)
    /// refuses with [`Error::NeedsThreads`](crate::Error::NeedsThreads).
    pub fn build(mut self) -> Result<crate::SystemSession<&'static str, F>> {
        crate::background::require(crate::BackgroundWork::Timer)?;
        let BlockConfig {
            rate,
            outputs: layout,
            max_block,
        } = self.config;
        let root = self.root_width(layout);
        self.check_placement(root, layout)?;
        let runtime = Self::session_executor()?;
        let timer = tutti_engine::TimerDriver::new(&runtime.0);
        let mut parts = self.parts(runtime, rate, max_block, Some(max_block), root)?;
        let engine = parts.graph.engine(
            &parts.transport,
            layout,
            self.opts.device_topology.clone(),
            BlockDates::Os,
        )?;
        Self::clock_midi(&parts, &engine);
        self.declare(&mut parts.graph)?;
        let readings = (engine.load(), engine.blocks());
        let taps = engine.heard_taps();
        let clock = engine.block_clock();
        let spec = tutti_engine::OutputSpec::new(rate, layout, F::FORMAT)
            .with_quantum(tutti_engine::OutputQuantum::try_from(max_block)?);
        let mut host = tutti_engine::StreamHost::new(
            tutti_engine::OutputStream::new(spec),
            Arc::new(tutti_engine::AudioCallbackState::new(engine)),
        );
        let latency = host.stream().latency();
        host.start_with(timer)?;
        let mut session = self.session(
            parts,
            crate::clock::SystemEngine {
                host: Some(host),
                #[cfg(feature = "device")]
                audio_host: self.opts.host,
            },
            true,
        )?;
        session.output.readings = Some(readings);
        taps.notify_with(session.background.bells().handle());
        session.output.taps = Some((taps, layout));
        // The outputs are timed by the session's one clock reader and the
        // stream's latency, which the timer reports as zero.
        #[cfg(feature = "midi")]
        session
            .midi
            .state
            .set_output_clock(crate::midi::OutputClockSource::new(clock.clone(), latency));
        #[cfg(not(feature = "midi"))]
        let _ = latency;
        session.transport.handle.set_clock(clock);
        Ok(session)
    }
}

#[cfg(feature = "device")]
impl<F: SessionFloat> SessionBuilder<crate::DeviceClock<F>> {
    /// Opens the output device at `format`, at its default rate and width
    /// ([`OutputConfig::sample_format`](tutti_cpal::OutputConfig::sample_format)).
    ///
    /// The choice holds across every restart ([`Session::restart`](crate::Session),
    /// the device watch's restarts, and a restart onto any other device). A
    /// device that lacks the format fails that restart with
    /// [`tutti_cpal::DeviceError::OutputFormatUnavailable`], through the restart's
    /// usual failure path, and never opens at another format. The same error
    /// fails [`build`](Self::build). A switch from the system clock
    /// ([`Session::into_device`](crate::Session)) takes its own
    /// [`OutputConfig`](tutti_cpal::OutputConfig).
    #[must_use]
    pub fn sample_format(mut self, format: tutti_buffers::SampleFormat) -> Self {
        self.config = self.config.sample_format(format);
        self
    }

    /// A session on the selected output device: the device opened first, the
    /// graph built at its rate and callback size, the callback live on
    /// return. The [`LiveSession`](crate::LiveSession) holds the
    /// [`Device`](crate::Device) and stays on this thread: keep it for as long
    /// as the sound should play.
    ///
    /// # Errors
    ///
    /// When the device cannot be opened or started, or the graph cannot be
    /// built over it.
    pub fn build(self) -> Result<crate::LiveSession<&'static str, F>> {
        crate::background::require(crate::BackgroundWork::Device)?;
        let runtime = Self::session_executor()?;
        let host = tutti_cpal::DeviceHost::open(self.opts.host)?;
        let engine = tutti_cpal::AudioEngine::open(&runtime.0, host, self.config.clone())?;
        let device = self.config.device().clone();
        self.build_on(runtime, engine, device, |engine, state| engine.start(state))
    }

    /// [`build`](Self::build) on a stream driver of the
    /// caller's choosing, `spec` standing for the config its device reports:
    /// the device-free build (a `tutti_engine::ManualStreamDriver`, for a test
    /// that renders the callback itself). Behind `testing`: a host builds on a
    /// device.
    ///
    /// # Errors
    ///
    /// When the driver cannot start, or the graph cannot be built.
    #[cfg(feature = "testing")]
    pub fn build_device_on<D>(
        self,
        spec: tutti_engine::OutputSpec,
        driver: D,
    ) -> Result<crate::LiveSession<&'static str, F>>
    where
        D: tutti_engine::StreamDriver<F>,
        D::Error: Into<tutti_cpal::DeviceError>,
    {
        let device = self.config.device().clone();
        let runtime = Self::session_executor()?;
        self.build_on(
            runtime,
            tutti_cpal::AudioEngine::from_spec(spec),
            device,
            |engine, state| engine.start_with(state, driver),
        )
    }

    /// Build over an opened `audio_engine` and start it with `start`.
    ///
    /// **The graph runs at the device's rate.** Every node is prepared at
    /// the graph's rate, so a graph at any other rate would run every
    /// oscillator off pitch.
    #[cfg(feature = "device")]
    fn build_on(
        mut self,
        runtime: (tutti_platform::Executor, crate::background::Running),
        mut audio_engine: tutti_cpal::AudioEngine,
        chosen: tutti_cpal::DeviceSelector,
        start: impl FnOnce(
            &mut tutti_cpal::AudioEngine,
            Arc<tutti_engine::AudioCallbackState<F>>,
        ) -> tutti_cpal::Result<()>,
    ) -> Result<crate::LiveSession<&'static str, F>> {
        crate::background::require(crate::BackgroundWork::Device)?;
        let spec = audio_engine.spec().clone();
        let root = self.root_width(spec.channels);
        self.check_placement(root, spec.channels)?;
        let mut parts = self.parts(
            runtime,
            spec.sample_rate,
            LIVE_MAX_BLOCK,
            spec.quantum.map(tutti_engine::OutputQuantum::max_block),
            root,
        )?;
        let engine = parts.graph.engine(
            &parts.transport,
            spec.channels,
            self.opts.device_topology.clone(),
            BlockDates::Os,
        )?;
        Self::clock_midi(&parts, &engine);
        self.declare(&mut parts.graph)?;
        let mut session: crate::LiveSession<&'static str, F> = self.session(
            parts,
            crate::device::DeviceHandle::vacant(self.opts.host),
            true,
        )?;
        session.output.readings = Some((engine.load(), engine.blocks()));
        let taps = engine.heard_taps();
        taps.notify_with(session.background.bells().handle());
        session.output.taps = Some((taps, spec.channels));
        let clock = engine.block_clock();
        // The outputs are timed by the session's one clock reader and the
        // stream's latency, read on every drain.
        #[cfg(feature = "midi")]
        session
            .midi
            .state
            .set_output_clock(crate::midi::OutputClockSource::new(
                clock.clone(),
                audio_engine.latency(),
            ));
        session.transport.handle.set_clock(clock);
        #[cfg(feature = "capture")]
        session.inputs.set_output_latency(audio_engine.latency());
        let state = Arc::new(tutti_engine::AudioCallbackState::new(engine));
        start(&mut audio_engine, Arc::clone(&state))?;
        #[cfg(feature = "capture")]
        let input_latency = session.inputs.latency();
        #[cfg(not(feature = "capture"))]
        let input_latency = Arc::default();
        let device = crate::Device::new(
            tutti_cpal::TuttiDriver::new(audio_engine, state),
            chosen,
            input_latency,
        );
        session.reports.faults_restarted(device.faults().count());
        session.running.fill(device);
        Ok(session)
    }
}

impl<C: Clock> SessionBuilder<C> {
    /// Global graph inputs (hardware or host input channels). Default none
    /// ([`ChannelLayout::EMPTY`]).
    #[must_use]
    pub fn inputs(mut self, inputs: ChannelLayout) -> Self {
        self.opts.inputs = inputs;
        self
    }

    /// The platform audio host the output device and every input the session
    /// opens (`Session::add_input`, feature `capture`) go through.
    /// Default [`AudioHost::Default`](tutti_cpal::AudioHost::Default); asking for
    /// one this build cannot reach (JACK without the `jack` feature, or with
    /// no server running) is refused when the device opens, as
    /// [`Error::Device`](crate::Error::Device) with
    /// `HostUnavailable`.
    #[cfg(feature = "device")]
    #[must_use]
    pub fn host(mut self, host: tutti_cpal::AudioHost) -> Self {
        self.opts.host = host;
        self
    }

    /// The project's output width: the graph root is at least this wide, and
    /// at least the device's ([`root_width`]). Default stereo.
    #[must_use]
    pub fn outputs_floor(mut self, outputs: ChannelLayout) -> Self {
        self.opts.outputs_floor = outputs;
        self
    }

    /// Declares the graph root's channels from the start
    /// ([`Session::declare_root_topology`]): the root is exactly as wide as
    /// `root`, whatever [`outputs_floor`](Self::outputs_floor) and the device
    /// say, and the build refuses a device that cannot place it
    /// ([`Error::Output`](crate::Error::Output)). Default none: the root is the engine's order for
    /// its width.
    #[must_use]
    pub fn root_topology(mut self, root: ChannelTopology) -> Self {
        self.opts.root = Some(root);
        self
    }

    /// States what the output device's channels are: an interface wired as
    /// 5.1 on its six outputs, say. The graph root is folded into it by
    /// speaker. Without it the device is taken to be the engine's order for
    /// its width ([`ChannelTopology::default_for`]: mono, stereo, quad, 5.1, 7.1,
    /// 7.1.4, and independent channels at any other width) and is read back as
    /// [`Placement::Default`](tutti_buffers::Placement::Default)
    /// ([`Session::output_device`]).
    ///
    /// The statement is the device's width: a build whose device has another
    /// is refused ([`Error::Output`](crate::Error::Output)), and so is a root
    /// the statement cannot place. A restart takes the statement of its own
    /// request (`DeviceRestart::topology`),
    /// since another device is wired another way.
    #[must_use]
    pub fn device_topology(mut self, device: ChannelTopology) -> Self {
        self.opts.device_topology = Some(device);
        self
    }

    /// Threads per block, the callback included ([`Workers`]): the callback
    /// alone by default. Each export gets its own pool of the same size.
    #[must_use]
    pub fn render_workers(mut self, workers: Workers) -> Self {
        self.opts.render_workers = workers;
        self
    }

    /// A hook each helper of the live render pool runs once on its own
    /// thread, with its index (`1..`, the callback is 0), before it takes
    /// work. A host that promotes its audio callback (`SCHED_FIFO`, MMCSS, an
    /// audio workgroup) gives the helpers the same priority here: helpers
    /// below it can stall a block, helpers above it preempt the callback.
    /// Nothing promotes them by default. An export's own pool does not run
    /// it. A hook that panics or blocks behaves as
    /// [`WorkerPoolBuilder::start_handler`](tutti_graph::WorkerPoolBuilder::start_handler)
    /// documents.
    #[must_use]
    pub fn render_worker_start(mut self, hook: impl Fn(usize) + Send + Sync + 'static) -> Self {
        self.opts.render_worker_start = Some(crate::graph::RenderWorkerStart::new(hook));
        self
    }

    /// Opens the session's inputs ([`Session::add_input`]) through `opener`
    /// instead of the sound card: a test's own capture, with no device.
    ///
    /// The opener serves both widths, so it carries over
    /// [`float`](SessionBuilder::float); the session opens its inputs at its own
    /// width, and an opener that refuses that width fails [`Session::add_input`].
    #[cfg(all(feature = "capture", feature = "testing"))]
    #[must_use]
    pub fn input_opener(
        mut self,
        opener: impl crate::InputOpener<f32> + crate::InputOpener<f64> + 'static,
    ) -> Self {
        self.opts.input_opener = Some(Box::new(opener));
        self
    }

    /// Open the OS MIDI layer: its devices are listed by
    /// [`Midi::devices`](crate::Midi::devices); a port is begun by
    /// [`Session::open_midi_input`](crate::Session::open_midi_input) or
    /// [`Session::open_midi_output`](crate::Session::open_midi_output) and installed
    /// by [`Session::apply`](crate::Session::apply),
    /// each input feeding the graph through an input node and an event
    /// inlet of its own, with `config` (where input times come from). Who the
    /// session is on the wire (MIDI-CI, its UMP Endpoint) is the session's
    /// [`MidiPolicy`](crate::MidiPolicy). Default off.
    ///
    /// The layer's background work (an input's pump, the device watch, Active
    /// Sensing, the legacy ports' poll) runs on tutti's own thread, which keeps pace
    /// with the hardware whether or not the app calls [`Session::update`]. A target
    /// that starts no thread (the web) refuses the build with
    /// [`Error::NeedsThreads`](crate::Error::NeedsThreads).
    #[cfg(feature = "midi-hardware")]
    #[must_use]
    pub fn midi_hardware(mut self, config: tutti_midi_hardware::MidiHardwareConfig) -> Self {
        self.opts.midi_hardware = Some(config);
        self
    }

    /// As [`midi_hardware`](Self::midi_hardware), over `backend` instead of the
    /// platform's: a headless test's `FakeBackend`
    /// (`tutti_midi_hardware::test_support`), whose `plug` and `unplug` drive
    /// hot-plug with no device.
    #[cfg(feature = "midi-hardware")]
    #[must_use]
    pub fn midi_hardware_over(
        mut self,
        backend: Box<dyn tutti_midi_hardware::MidiEndpoints>,
        config: tutti_midi_hardware::MidiHardwareConfig,
    ) -> Self {
        self.opts.midi_hardware = Some(config);
        self.opts.midi_backend = Some(MidiBackend(backend));
        self
    }

    /// The session's MPE mode to begin with: what the engine's MIDI input
    /// reads in, and what a synth added through
    /// [`Session::add_instrument`](crate::Session::add_instrument) is built
    /// to follow (writing `session.mpe()` changes it later).
    /// Default `Disabled`.
    #[cfg(feature = "midi")]
    #[must_use]
    pub fn mpe_mode(mut self, mode: tutti_midi_protocol::MpeMode) -> Self {
        self.opts.mpe_mode = mode;
        self
    }

    /// The Groups the engine's MIDI input hears, from the first through
    /// `last`. Default the first Group alone, whose seventeen ports are what a
    /// one-Group device needs; each Group heard adds as many event outputs
    /// ([`tutti_midi_runtime::MidiInputPorts`]). An event of a Group past
    /// `last` is not delivered.
    #[cfg(feature = "midi")]
    #[must_use]
    pub fn midi_groups(mut self, last: tutti_ump::MidiGroup) -> Self {
        self.opts.midi_ports = tutti_midi_runtime::MidiInputPorts::through(last);
        self
    }

    /// How long a modulation target may stay unresolved, measured on
    /// [`PhaseCx::now`](crate::PhaseCx::now), before it is logged and
    /// reported ([`SessionEvent::Modulation`](crate::SessionEvent::Modulation)).
    /// Default 4 s.
    #[must_use]
    pub fn unresolved_warning(mut self, after: std::time::Duration) -> Self {
        self.opts.unresolved_warning = after;
        self
    }

    /// How often a healthy hosted plugin's state is captured, measured on
    /// [`PhaseCx::now`](crate::PhaseCx::now)
    /// (a task on tutti's own thread).
    /// Default 8 s.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn snapshot_interval(mut self, every: std::time::Duration) -> Self {
        self.opts.plugins.snapshot_interval = every;
        self
    }

    /// What a hosted plugin's death or hang does. Default
    /// `RecoveryPolicy::default()`: the plugin reloads at its node with its
    /// last snapshot, within the policy's budget. A hang is detected either
    /// way ([`plugin_hang`](Self::plugin_hang)): the hung server is ended and
    /// its death is what this decides. See
    /// [`no_plugin_recovery`](Self::no_plugin_recovery) for a host that decides itself.
    ///
    /// It covers out-of-process plugins loaded through the session's load
    /// path ([`Session::apply`](crate::Session::apply)). It does not
    /// cover an in-process VST2 plugin (its crash takes the process, and a
    /// hang in it is not observed), nor one a host inserted itself with
    /// `Graph::add_plugin` (the session does not know its path): those are
    /// removed or left as before.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn plugin_recovery(mut self, policy: crate::RecoveryPolicy) -> Self {
        self.opts.plugins.recovery = Some(policy);
        self
    }

    /// A hosted plugin's death or hang is not recovered: a loaded plugin's node
    /// stays silent at its key and its handle reads lost, and a node of a plugin
    /// the host inserted itself is removed
    /// ([`PluginChange::Died`](crate::PluginChange::Died)); the host decides, a
    /// prompt to the user for instance. Replaces
    /// [`plugin_recovery`](Self::plugin_recovery).
    ///
    /// It covers the same plugins as `plugin_recovery`.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn no_plugin_recovery(mut self) -> Self {
        self.opts.plugins.recovery = None;
        self
    }

    /// When a hosted plugin's unanswered blocks are a hang. Default
    /// [`HangPolicy::default`](crate::HangPolicy). The session watches every
    /// hosted plugin for it, whether or not
    /// [`plugin_recovery`](Self::plugin_recovery) is set.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn plugin_hang(mut self, policy: crate::HangPolicy) -> Self {
        self.opts.plugins.hang = policy;
        self
    }

    /// Build the graph at `rate` for blocks of up to `max_block`, with the
    /// device's callback `quantum` when known, `outputs` wide; insert the
    /// metronome and the engine's MIDI nodes.
    fn parts(
        &mut self,
        (executor, background): (tutti_platform::Executor, crate::background::Running),
        rate: SampleRate,
        max_block: MaxBlock,
        quantum: Option<MaxBlock>,
        outputs: usize,
    ) -> Result<Parts<C::Float>> {
        #[cfg(feature = "midi-hardware")]
        if self.opts.midi_hardware.is_some() {
            crate::background::require(crate::BackgroundWork::MidiHardware)?;
        }
        // Built first so its port list can feed the graph's MIDI input.
        #[cfg(feature = "midi-hardware")]
        let backend = self.opts.midi_backend.take();
        #[cfg(feature = "midi-hardware")]
        let midi_io = self.opts.midi_hardware.map(|options| -> Result<_> {
            // One event inlet for each input, which its thread pushes into
            // for the frame the engine's block clock puts its arrival on: a
            // block later, so no event is late (the lead).
            let inlets = tutti_midi_runtime::InputInlets::new(
                crate::midi::MIDI_INPUT_INLET_CAPACITY,
                crate::midi::input_lead(quantum.unwrap_or(max_block).samples(), rate),
            );
            let io = match backend {
                Some(MidiBackend(backend)) => {
                    tutti_midi_hardware::MidiSession::open_over(&executor, backend, inlets, options)
                        .map_err(crate::Error::MidiHardware)?
                }
                None => tutti_midi_hardware::MidiSession::open(&executor, inlets, options)
                    .map_err(crate::Error::MidiHardware)?,
            };
            // Subscribed before anything can plug in unseen: `Phase::Poll`
            // is this subscription's only reader.
            let events = io.devices().subscribe();
            Ok((io, events))
        });
        #[cfg(feature = "midi-hardware")]
        let (midi_io, device_events) = midi_io.transpose()?.unzip();

        let transport = Transport::new(rate);
        let mut graph = Graph::new(
            self.opts.inputs,
            outputs,
            rate,
            max_block,
            quantum,
            transport.settings().tempo_reader(),
        );
        graph.set_render_workers(self.opts.render_workers);
        graph.set_render_worker_start(self.opts.render_worker_start.take());
        let click_settings = Arc::new(ClickSettings::new());

        // The metronome. It reads the beat, the play, record and count-in state
        // and the meter from its block's `Env`.
        // It is not wired to the output: what it feeds is the host's
        // declaration, like every other node's.
        let click = graph.add(ClickNode::from(ClickNodeConfig::new(Arc::clone(
            &click_settings,
        ))));

        // The engine's MIDI as graph nodes: the sync node (sends nothing until a
        // host sets its messages). An input port's input node and an output's
        // node are made when their ports open, and the sync node feeds
        // each output.
        #[cfg(feature = "midi")]
        let midi = {
            let meter = transport.settings().project_meter().clone();
            let sync = graph.add(
                tutti_midi_runtime::SyncOut::new(tutti_ump::MidiGroup::FIRST)
                    .meter(move || meter.reader()),
            );
            let sync_controls = graph
                .controls(&sync)
                .cloned()
                .expect("the sync node was added a moment ago");
            Some(crate::MidiNodes {
                input_ports: self.opts.midi_ports,
                sync: sync.erase(),
                sync_controls,
            })
        };

        Ok(Parts {
            executor,
            background,
            graph,
            transport,
            click: Some(click.erase()),
            click_settings,
            #[cfg(feature = "midi")]
            midi,
            #[cfg(feature = "midi-hardware")]
            midi_io,
            #[cfg(feature = "midi-hardware")]
            device_events,
        })
    }

    /// The session's executor, and the one thread of tutti's that drives it.
    fn session_executor() -> Result<(tutti_platform::Executor, crate::background::Running)> {
        let executor = tutti_platform::Executor::new();
        let background = crate::background::start(&executor)?;
        Ok((executor, background))
    }

    /// The session over `parts`, `engine` its offline engine if any, with
    /// (`sampler`) a disk streamer when `streamer`.
    fn session<N: Clock<Float = C::Float>>(
        &mut self,
        mut parts: Parts<N::Float>,
        running: N::Running,
        streamer: bool,
    ) -> Result<Session<&'static str, N>> {
        let executor = parts.executor.clone();
        let bells = tutti_rt::Bells::new(&executor).map_err(crate::Error::Bells)?;
        parts.graph.notify_with(bells.handle());
        let sampler = crate::session::parts::SamplerPart::new(
            streamer,
            parts.graph.prepared().sample_rate(),
            &executor,
            &bells,
        )?;
        let (plugins, _early_work) =
            crate::session::parts::PluginPart::new(&self.opts.plugins, &executor);
        let table = parts.graph.value_table();
        let click_settings = Arc::clone(&parts.click_settings);
        let session = Session {
            id: crate::apply::SessionId::new(),
            values: crate::value::ValueState::new(Arc::clone(&table)),
            _early_work,
            running,
            resource_ids: crate::peer::ResourceIds::new(),
            transport: crate::session::parts::TransportPart::new(
                parts.transport.clone(),
                parts.click,
            ),
            output: crate::session::parts::OutputPart::new(
                self.opts.root.clone(),
                crate::policy::output_cell(table.output_facts()),
            ),
            reports: Default::default(),
            midi: crate::session::parts::MidiPart::new(&mut parts, &self.opts),
            graph: parts.graph,
            mods: crate::modulation::ModSession::new(
                self.opts.unresolved_warning,
                Arc::clone(&table),
            ),
            sampler,
            devices: crate::session::parts::DevicePart::new(&executor),
            inputs: crate::session::parts::Inputs::from_options(&mut self.opts, &executor),
            bounce: Default::default(),
            background: crate::background::Background::new(executor, parts.background, bells),
            plugins,
        };
        session.provide_values(&table, click_settings);
        Ok(session)
    }
}

impl<C: Clock> SessionBuilder<C> {
    /// Gives the hardware MIDI devices' threads the engine's block clock, so
    /// each places its arrivals on frames.
    fn clock_midi(parts: &Parts<C::Float>, engine: &tutti_engine::Engine<C::Float>) {
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = &parts.midi_io {
            io.ports()
                .inlets()
                .set_clock(Some(crate::midi::arrival_clock(engine.block_clock())));
        }
        #[cfg(not(feature = "midi-hardware"))]
        let _ = (parts, engine);
    }

    /// The root's width for `device`: the declared topology's, or else
    /// [`root_width`] of the floor.
    fn root_width(&self, device: ChannelLayout) -> usize {
        self.opts.root.as_ref().map_or_else(
            || root_width(self.opts.outputs_floor, device),
            |root| usize::from(root.width().count()),
        )
    }

    /// Refuses a `(root, device)` pair whose root has a channel the device has
    /// no place for, before anything is built: the engine would render it
    /// silent. A declared root is placed from its topology (and refused if it
    /// is empty or wider than the engine renders); an undeclared one is a bare
    /// width.
    ///
    /// The root is at least as wide as the device up to [`MAX_CHANNELS`],
    /// so what this refuses of an undeclared root is a project wider than the
    /// device where either width has no channel order (a 6-channel project on
    /// a 5-channel interface).
    fn check_placement(&self, root: usize, device: ChannelLayout) -> Result<()> {
        let placed = match &self.opts.device_topology {
            Some(stated) if stated.width() != device => {
                return Err(tutti_buffers::FoldError::from(
                    tutti_buffers::BufferError::WidthMismatch {
                        src: stated.width(),
                        dst: device,
                    },
                )
                .into());
            }
            Some(stated) => stated.clone(),
            None => ChannelTopology::default_for(device),
        };
        match &self.opts.root {
            // The same check `Graph::declare_root` makes a step later; made
            // here so a refused root builds nothing (no engine, no device
            // thread). Removing this arm alone changes no outcome, only
            // how much is built before the refusal.
            Some(declared) => Graph::<C::Float>::check_root(declared, Some(&placed))?,
            None => {
                let width = ChannelLayout::try_from(root).map_err(|e| {
                    tutti_buffers::FoldError::from(tutti_buffers::BufferError::from(e))
                })?;
                tutti_graph::fold_to_topology::<C::Float>(
                    &ChannelTopology::default_for(width),
                    &placed,
                )?;
            }
        }
        Ok(())
    }

    /// Declares the builder's root on the graph the engine was just built
    /// over, which publishes it to the engine.
    fn declare(&self, graph: &mut Graph<C::Float>) -> Result<()> {
        if self.opts.root.is_some() {
            graph.declare_root(self.opts.root.clone(), true)?;
        }
        Ok(())
    }
}

/// How wide the graph root is built, given what the project asks for and
/// what the device presents.
///
/// **These are two independent numbers, and neither may narrow the other.**
///
/// `project` is the project's width. A 5.1 project on a stereo laptop must
/// still render six channels: `Engine::process_segment` folds them into the
/// device buffer through the shared ITU matrices every block. Clamping the
/// root down to the device would make that fold a no-op that hides real
/// channel loss.
///
/// The device is a **floor** because a root narrower than the device leaves
/// the extra device channels permanently silent: the fold zero-fills
/// rather than upmixing, so nothing downstream can recover them.
///
/// The [`MAX_CHANNELS`] clamp is applied here, at construction:
/// `Engine::new` bounds the editor to that many global outputs, so a wider
/// root would be refused at its first commit and never play. An export's
/// fork is prepared on its own, with no such cap.
pub fn root_width(project: ChannelLayout, device: ChannelLayout) -> usize {
    usize::from(project.count())
        .max(usize::from(device.count()))
        .clamp(1, MAX_CHANNELS)
}

#[cfg(test)]
mod tests {
    use tutti_buffers::{ChannelLayout, MaxBlock};
    use tutti_units::{SampleRate, Samples};

    use super::LIVE_MAX_BLOCK;
    use crate::Session;

    /// **The defaults are 48 kHz, stereo and `LIVE_MAX_BLOCK`.** Each is read
    /// back from the session the bare builder made: the graph's prepared rate
    /// and block, the root's width, and the width the engine accepts for a
    /// pulled block.
    #[test]
    fn the_bare_builder_is_48k_stereo_at_the_live_block() {
        let session = Session::builder().build().expect("builds");
        let prepared = session.graph_ref().prepared();
        assert_eq!(prepared.sample_rate(), SampleRate::SR_48K);
        assert_eq!(prepared.max_block(), LIVE_MAX_BLOCK);
        assert_eq!(session.graph_ref().outputs(), 2);
        assert_eq!(session.sample_rate(), SampleRate::SR_48K);
        assert_eq!(
            session.output.taps.as_ref().map(|(_, layout)| *layout),
            Some(ChannelLayout::STEREO)
        );
    }

    /// **The setters are honoured, not the defaults.** A rate, a width and a
    /// block that are none of the defaults come back out of the session.
    #[test]
    fn the_setters_choose_the_rate_the_width_and_the_block() {
        let quad = ChannelLayout::from_count(4);
        let block = MaxBlock::of(Samples::new(256));
        let session = Session::builder()
            .sample_rate(SampleRate::SR_44K1)
            .outputs(quad)
            .max_block(block)
            .build()
            .expect("builds");
        let prepared = session.graph_ref().prepared();
        assert_eq!(prepared.sample_rate(), SampleRate::SR_44K1);
        assert_eq!(prepared.max_block(), block);
        assert_eq!(session.graph_ref().outputs(), 4);
        assert_eq!(session.output.taps.as_ref().map(|(_, l)| *l), Some(quad));
    }
}
