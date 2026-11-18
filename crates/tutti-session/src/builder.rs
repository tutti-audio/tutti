//! [`SessionBuilder`]: one ordered, fallible build of a graph, its engine and
//! the nodes the engine inserts, offline or on a device.

use std::num::NonZeroUsize;
use std::sync::Arc;

use tutti_graph::NodeKey;
use tutti_graph::PrerollTable;
use tutti_nodes::{ClickNode, ClickSettings};
use tutti_transport::Transport;
use tutti_types::MAX_CHANNELS;
use tutti_types::{ChannelLayout, ChannelTopology, MaxBlock};
use tutti_units::{SampleRate, Samples};

use crate::graph::{Graph, LIVE_MAX_BLOCK};
use crate::{Clock, Offline, OfflineSession, Result, Session, SystemClock};
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
/// Everything else (workers, MIDI, plugins, the root topology, the spawner) is
/// set the same way on every clock.
///
/// ```
/// use tutti_units::{SampleRate, Samples};
/// use tutti_types::{ChannelLayout, MaxBlock};
/// use tutti_session::{Session, Workers};
///
/// let session = Session::builder()
///     .sample_rate(SampleRate::SR_44K1)
///     .max_block(MaxBlock::of(Samples(512)))
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
struct Options {
    inputs: ChannelLayout,
    outputs_floor: ChannelLayout,
    root: Option<ChannelTopology>,
    /// What the output device's channels are, when the host states it
    /// ([`device_topology`](SessionBuilder::device_topology)).
    device_topology: Option<ChannelTopology>,
    render_workers: Workers,
    /// The platform host the output and every input the session opens use.
    #[cfg(feature = "device")]
    host: tutti_cpal::AudioHost,
    /// Where the session's inputs open, if not the sound card
    /// ([`input_opener`](Self::input_opener)).
    #[cfg(all(feature = "device", feature = "audio-io"))]
    input_opener: Option<Box<dyn crate::InputOpener>>,
    render_worker_start: Option<crate::graph::RenderWorkerStart>,
    /// The OS MIDI layer's options when it is on
    /// ([`midi_hardware`](Self::midi_hardware)); `None` is no OS MIDI layer.
    #[cfg(feature = "midi-hardware")]
    midi_hardware: Option<tutti_midi_hardware::MidiHardwareOptions>,
    /// The backend the OS MIDI layer runs over, if not the platform's.
    #[cfg(feature = "midi-hardware")]
    midi_backend: Option<MidiBackend>,
    #[cfg(feature = "midi")]
    mpe_mode: tutti_midi_protocol::MpeMode,
    #[cfg(feature = "midi")]
    midi_ports: tutti_midi_runtime::MidiInputPorts,
    unresolved_warning: std::time::Duration,
    #[cfg(feature = "plugin")]
    snapshot_interval: std::time::Duration,
    #[cfg(feature = "plugin")]
    recovery: Option<crate::RecoveryPolicy>,
    #[cfg(feature = "plugin")]
    hang: crate::HangPolicy,
    spawner: crate::jobs::Spawner,
    pool: crate::PoolConfig,
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
            #[cfg(all(feature = "device", feature = "audio-io"))]
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
            #[cfg(feature = "plugin")]
            snapshot_interval: crate::plugin::SNAPSHOT_INTERVAL,
            #[cfg(feature = "plugin")]
            recovery: Some(crate::RecoveryPolicy::default()),
            #[cfg(feature = "plugin")]
            hang: crate::HangPolicy::default(),
            spawner: crate::jobs::Spawner::default(),
            pool: crate::PoolConfig::default(),
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
struct Parts {
    graph: Graph,
    transport: Transport,
    click: Option<NodeKey>,
    click_settings: Arc<ClickSettings>,
    compensation: Arc<PrerollTable>,
    #[cfg(feature = "midi")]
    midi: Option<crate::MidiNodes>,
    #[cfg(feature = "midi-hardware")]
    midi_io: Option<tutti_midi_hardware::MidiSession>,
    #[cfg(feature = "midi-hardware")]
    device_events: Option<tutti_midi_hardware::DeviceEvents>,
}

impl Default for SessionBuilder<Offline> {
    fn default() -> Self {
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

impl SessionBuilder<Offline> {
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

    /// Steps the blocks with an OS timer thread instead: no sound card, for an
    /// app that plays MIDI. The rate, the width and the block carry over, and
    /// every other option with them.
    #[must_use]
    pub fn system_clock(self) -> SessionBuilder<SystemClock> {
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
    ///     .device(Default::default())
    ///     .sample_rate(SampleRate::SR_44K1);
    /// ```
    #[cfg(feature = "device")]
    #[must_use]
    pub fn device(self, device: tutti_cpal::DeviceSelector) -> SessionBuilder<crate::DeviceClock> {
        SessionBuilder {
            opts: self.opts,
            config: device,
            dates: (),
        }
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
    pub fn build(mut self) -> Result<OfflineSession> {
        let BlockConfig {
            rate,
            outputs: layout,
            max_block,
        } = self.config;
        let root = self.root_width(layout);
        self.check_placement(root, layout)?;
        let mut parts = self.parts(rate, max_block, None, root)?;
        // The width of every block the host pulls.
        let engine = parts.graph.engine(
            &parts.transport,
            layout,
            self.opts.device_topology.clone(),
            self.dates,
        )?;
        Self::clock_midi(&parts, &engine);
        self.declare(&mut parts.graph)?;
        let heard = engine.heard_taps();
        let mut engine = engine;
        let load = engine.take_load_meter();
        let clock = engine.block_clock();
        let mut session = self.session(parts, crate::clock::OfflineEngine { engine }, true)?;
        session.load = load;
        session.transport_handle.set_clock(clock);
        session.heard = Some((heard, layout));
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
    ) -> Result<Session<&'static str, crate::Detached>> {
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
        self.session(
            Parts {
                graph,
                transport,
                click: None,
                click_settings: Arc::new(ClickSettings::new()),
                compensation: Arc::default(),
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

impl SessionBuilder<SystemClock> {
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
    /// ([`Error::Clock`](crate::Error::Clock)).
    pub fn build(mut self) -> Result<crate::SystemSession> {
        let BlockConfig {
            rate,
            outputs: layout,
            max_block,
        } = self.config;
        let root = self.root_width(layout);
        self.check_placement(root, layout)?;
        let mut parts = self.parts(rate, max_block, Some(max_block), root)?;
        let mut engine = parts.graph.engine(
            &parts.transport,
            layout,
            self.opts.device_topology.clone(),
            BlockDates::Os,
        )?;
        Self::clock_midi(&parts, &engine);
        self.declare(&mut parts.graph)?;
        let load = engine.take_load_meter();
        let heard = engine.heard_taps();
        let clock = engine.block_clock();
        let spec = tutti_engine::OutputSpec::new(rate, layout, tutti_buffers::SampleFormat::F32)
            .with_quantum(max_block);
        let mut host = tutti_engine::StreamHost::new(
            tutti_engine::OutputStream::new(spec),
            Arc::new(tutti_engine::AudioCallbackState::new(engine)),
        );
        let latency = host.stream().latency();
        host.start_with(tutti_engine::TimerDriver::new())?;
        let mut session = self.session(
            parts,
            crate::clock::SystemEngine {
                host: Some(host),
                #[cfg(feature = "device")]
                audio_host: self.opts.host,
            },
            true,
        )?;
        session.load = load;
        session.heard = Some((heard, layout));
        // The outputs are timed by the session's one clock reader and the
        // stream's latency, which the timer reports as zero.
        #[cfg(feature = "midi")]
        session
            .midi_state
            .set_output_clock(crate::midi::OutputClockSource::new(clock.clone(), latency));
        #[cfg(not(feature = "midi"))]
        let _ = latency;
        session.transport_handle.set_clock(clock);
        Ok(session)
    }
}

#[cfg(feature = "device")]
impl SessionBuilder<crate::DeviceClock> {
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
    pub fn build(self) -> Result<crate::LiveSession> {
        let host = tutti_cpal::DeviceHost::open(self.opts.host)?;
        let device = self.config.clone();
        let engine = tutti_cpal::AudioEngine::open(host, device.clone())?;
        self.build_on(engine, device, |engine, state| engine.start(state))
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
    ) -> Result<crate::LiveSession>
    where
        D: tutti_engine::StreamDriver,
        D::Error: Into<tutti_cpal::Error>,
    {
        let device = self.config.clone();
        self.build_on(
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
        mut audio_engine: tutti_cpal::AudioEngine,
        chosen: tutti_cpal::DeviceSelector,
        start: impl FnOnce(
            &mut tutti_cpal::AudioEngine,
            Arc<tutti_engine::AudioCallbackState>,
        ) -> tutti_cpal::Result<()>,
    ) -> Result<crate::LiveSession> {
        let spec = audio_engine.spec().clone();
        let root = self.root_width(spec.channels);
        self.check_placement(root, spec.channels)?;
        let mut parts = self.parts(spec.sample_rate, LIVE_MAX_BLOCK, spec.quantum, root)?;
        let mut engine = parts.graph.engine(
            &parts.transport,
            spec.channels,
            self.opts.device_topology.clone(),
            BlockDates::Os,
        )?;
        Self::clock_midi(&parts, &engine);
        self.declare(&mut parts.graph)?;
        let mut session: crate::LiveSession = self.session(
            parts,
            crate::device::DeviceHandle::vacant(self.opts.host),
            true,
        )?;
        session.load = engine.take_load_meter();
        session.heard = Some((engine.heard_taps(), spec.channels));
        let clock = engine.block_clock();
        // The outputs are timed by the session's one clock reader and the
        // stream's latency, read on every drain.
        #[cfg(feature = "midi")]
        session
            .midi_state
            .set_output_clock(crate::midi::OutputClockSource::new(
                clock.clone(),
                audio_engine.latency(),
            ));
        session.transport_handle.set_clock(clock);
        #[cfg(feature = "audio-io")]
        session.inputs.set_output_latency(audio_engine.latency());
        let state = Arc::new(tutti_engine::AudioCallbackState::new(engine));
        start(&mut audio_engine, Arc::clone(&state))?;
        #[cfg(feature = "audio-io")]
        let input_latency = session.inputs.latency();
        #[cfg(not(feature = "audio-io"))]
        let input_latency = Arc::default();
        let device = crate::Device::new(
            tutti_cpal::TuttiDriver::new(audio_engine, state),
            chosen,
            input_latency,
        );
        session.faults_seen = device.faults().count();
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
    /// opens (`Session::add_input`, feature `audio-io`) go through.
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
    /// its width ([`ChannelTopology::assumed`]: mono, stereo, quad, 5.1, 7.1,
    /// 7.1.4, and independent channels at any other width) and is read back as
    /// [`Placement::Assumed`](tutti_types::Placement::Assumed)
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
    #[cfg(all(feature = "device", feature = "audio-io", feature = "testing"))]
    #[must_use]
    pub fn input_opener(mut self, opener: impl crate::InputOpener + 'static) -> Self {
        self.opts.input_opener = Some(Box::new(opener));
        self
    }

    /// The spawner the session runs its blocking work on:
    /// a file load, a plugin round trip, a recording's pump. It is handed
    /// each [`Task`](crate::Task) and calls [`Task::run`](crate::Task::run) on a
    /// thread of its own (`tokio::task::spawn_blocking`, a rayon pool); what
    /// the job reports lands in [`Session::update`]. The default is a named
    /// `tutti_rt::spawn_named` thread per task; dropping the session stops
    /// them within 100 ms, and a thread still running then (a worker blocked on
    /// the main thread) is detached with a `tracing` warning and outlives the
    /// session. A task the spawner drops unrun ends as
    /// [`WorkerError::Dropped`](crate::WorkerError::Dropped), so a spawner that
    /// refuses work loses none silently.
    ///
    /// A recording take (`Session::record`, feature `audio-io`) holds one job for the whole take:
    /// over a bounded pool with no worker free it does not start until one is.
    ///
    /// ```
    /// # use tutti_session::Session;
    /// let builder = Session::builder().spawn_with(|job| {
    ///     std::thread::spawn(move || job.run());
    /// });
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn spawn_with(mut self, spawn: impl Fn(crate::Task) + Send + Sync + 'static) -> Self {
        self.opts.spawner = crate::jobs::Spawner::new(spawn);
        self
    }

    /// How the pool runs its loads ([`PoolConfig`](crate::PoolConfig)): how
    /// many worker stages run at once. Default four.
    #[must_use]
    pub const fn pool_config(mut self, config: crate::PoolConfig) -> Self {
        self.opts.pool = config;
        self
    }

    /// Open the OS MIDI layer: its devices are listed by
    /// [`Midi::devices`](crate::Midi::devices) and opened by the pool's
    /// [`midi_input`](crate::midi_input) and [`midi_output`](crate::midi_output)
    /// entries, each input feeding the graph through an ingest node and an event
    /// inlet of its own, with `options` (where input times come from). Who the
    /// session is on the wire (MIDI-CI, its UMP Endpoint) is the session's
    /// [`MidiPolicy`](crate::MidiPolicy). Default off.
    #[cfg(feature = "midi-hardware")]
    #[must_use]
    pub fn midi_hardware(mut self, options: tutti_midi_hardware::MidiHardwareOptions) -> Self {
        self.opts.midi_hardware = Some(options);
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
        options: tutti_midi_hardware::MidiHardwareOptions,
    ) -> Self {
        self.opts.midi_hardware = Some(options);
        self.opts.midi_backend = Some(MidiBackend(backend));
        self
    }

    /// The session's MPE mode to begin with: what the engine's MIDI input
    /// ingests in, and what a synth added through
    /// [`Session::add_instrument`](crate::Session::add_instrument) is built
    /// to follow ([`Midi::set_mpe`](crate::Midi::set_mpe) changes it later).
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
    /// (a job on the session's spawner, [`spawn_with`](Self::spawn_with)).
    /// Default 8 s.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn snapshot_interval(mut self, every: std::time::Duration) -> Self {
        self.opts.snapshot_interval = every;
        self
    }

    /// What a hosted plugin's death or hang does. Default
    /// `Some(RecoveryPolicy::default())`: the plugin reloads at its node with
    /// its last snapshot. `None`: a pooled plugin's node stays silent at its
    /// key and the entry reads lost, and a node of a plugin the host inserted
    /// itself is removed ([`PluginChange::Died`](crate::PluginChange::Died));
    /// the host decides, a prompt to the user for instance. A hang is detected
    /// either way ([`plugin_hang`](Self::plugin_hang)): the hung server is
    /// ended and its death is what this decides.
    ///
    /// It covers out-of-process plugins loaded through the session's load
    /// path ([`Session::add_pooled_plugin`](crate::Session::add_pooled_plugin)). It does not
    /// cover an in-process VST2 plugin (its crash takes the process, and a
    /// hang in it is not observed), nor one a host inserted itself with
    /// `Graph::insert_plugin` (the session does not know its path): those are
    /// removed or left as before.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn plugin_recovery(mut self, policy: Option<crate::RecoveryPolicy>) -> Self {
        self.opts.recovery = policy;
        self
    }

    /// When a hosted plugin's unanswered blocks are a hang. Default
    /// [`HangPolicy::default`](crate::HangPolicy). The session watches every
    /// hosted plugin for it, whether or not
    /// [`plugin_recovery`](Self::plugin_recovery) is set.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn plugin_hang(mut self, policy: crate::HangPolicy) -> Self {
        self.opts.hang = policy;
        self
    }

    /// Build the graph at `rate` for blocks of up to `max_block`, with the
    /// device's callback `quantum` when known, `outputs` wide; insert the
    /// metronome and the engine's MIDI nodes.
    fn parts(
        &mut self,
        rate: SampleRate,
        max_block: MaxBlock,
        quantum: Option<MaxBlock>,
        outputs: usize,
    ) -> Result<Parts> {
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
                    tutti_midi_hardware::MidiSession::open_over(backend, inlets, options)
                        .map_err(crate::Error::MidiHardware)?
                }
                None => tutti_midi_hardware::MidiSession::open(inlets, options)
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

        // The metronome. It reads the beat, play and record state from its
        // block's `Env`, and the count-in flag off the transport (read only).
        // It is not wired to the output: what it feeds is the host's
        // declaration, like every other node's.
        let (click, _settings) = graph.add(ClickNode::new(&transport, Arc::clone(&click_settings)));

        // The engine's MIDI as graph nodes: the sync node (sends nothing until a
        // host sets its messages). An input port's ingest node and an output's
        // node are made when their pool entries load, and the sync node feeds
        // each output.
        #[cfg(feature = "midi")]
        let midi = {
            let meter = click_settings.project_meter();
            let (sync, sync_controls) = graph.add(
                tutti_midi_runtime::SyncOut::new(tutti_ump::MidiGroup::FIRST)
                    .meter(move || meter.reader()),
            );
            Some(crate::MidiNodes {
                input_ports: self.opts.midi_ports,
                sync: sync.erase(),
                sync_controls,
            })
        };

        Ok(Parts {
            graph,
            transport,
            click: Some(click.erase()),
            click_settings,
            compensation: Arc::default(),
            #[cfg(feature = "midi")]
            midi,
            #[cfg(feature = "midi-hardware")]
            midi_io,
            #[cfg(feature = "midi-hardware")]
            device_events,
        })
    }

    /// The session over `parts`, `engine` its offline engine if any, with
    /// (`sampler`) a disk streamer when `streamer`.
    fn session<N: Clock>(
        &mut self,
        parts: Parts,
        running: N::Running,
        streamer: bool,
    ) -> Result<Session<&'static str, N>> {
        #[cfg(feature = "sampler")]
        let disk_streamer = if streamer {
            Some(tutti_sampler::DiskStreamer::new(
                parts.graph.prepared().sample_rate(),
                tutti_sampler::DiskStreamerConfig::default(),
            )?)
        } else {
            None
        };
        #[cfg(not(feature = "sampler"))]
        let _ = streamer;
        #[cfg(feature = "plugin")]
        let (main_work, work) = tutti_plugin_types::MainWork::new();
        #[cfg(feature = "plugin")]
        let main_work = crate::session::SharedWork::new(main_work);
        Ok(Session {
            values: crate::value::ValueState::new(parts.graph.value_table()),
            #[cfg(feature = "plugin")]
            _early_work: crate::session::EarlyWork(main_work.clone()),
            running,
            graph: parts.graph,
            resource_ids: crate::peer::ResourceIds::new(),
            pool: crate::Pool::new(self.opts.pool),
            #[cfg(feature = "plugin")]
            pooled_swaps: std::collections::BTreeSet::new(),
            transport_handle: crate::TransportHandle::new(parts.transport.clone()),
            transport: parts.transport,
            load: None,
            heard: None,
            root_request: self.opts.root.clone(),
            root_refusal: None,
            output_policy: crate::OutputPolicy::default(),
            output_policy_staged: false,
            click: parts.click,
            click_settings: parts.click_settings,
            compensation: parts.compensation,
            latency: Samples(0),
            resumed: false,
            silenced_seen: 0,
            #[cfg(feature = "midi")]
            midi: parts.midi,
            #[cfg(all(feature = "midi", not(feature = "midi-hardware")))]
            midi_state: crate::midi::MidiState::default().with_mpe(self.opts.mpe_mode),
            #[cfg(feature = "midi-hardware")]
            midi_state: crate::midi::MidiState::with_device_events(parts.device_events)
                .with_mpe(self.opts.mpe_mode),
            mods: crate::modulation::ModSession::new(self.opts.unresolved_warning),
            #[cfg(feature = "midi-hardware")]
            midi_io: parts.midi_io,
            #[cfg(feature = "midi-hardware")]
            midi_policy: crate::MidiPolicy::default(),
            #[cfg(feature = "sampler")]
            disk_streamer,
            #[cfg(feature = "sampler")]
            sampler_opens: tutti_types::Mint::new(),
            faults_seen: 0,
            #[cfg(feature = "device")]
            output_listing: None,
            #[cfg(feature = "device")]
            outputs_listed: false,
            #[cfg(all(feature = "device", feature = "audio-io"))]
            inputs: crate::input::Inputs::new(self.opts.host, self.opts.input_opener.take()),
            #[cfg(feature = "plugin")]
            snapshot_interval: self.opts.snapshot_interval,
            #[cfg(feature = "plugin")]
            recovery: self.opts.recovery,
            #[cfg(feature = "plugin")]
            hang: self.opts.hang,
            #[cfg(feature = "plugin")]
            plugins_now: std::time::Instant::now(),
            #[cfg(feature = "plugin")]
            render_mode: Default::default(),
            #[cfg(feature = "export")]
            bounce: Default::default(),
            jobs: crate::jobs::Jobs::new(self.opts.spawner.clone()),
            #[cfg(feature = "plugin")]
            work,
            #[cfg(feature = "plugin")]
            main_work,
            carried_events: Vec::new(),
        })
    }
}

impl<C: Clock> SessionBuilder<C> {
    /// Gives the hardware MIDI devices' threads the engine's block clock, so
    /// each places its arrivals on frames.
    fn clock_midi(parts: &Parts, engine: &tutti_engine::Engine) {
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
            None => ChannelTopology::assumed(device),
        };
        match &self.opts.root {
            // The same check `Graph::declare_root` makes a step later; made
            // here so a refused root builds nothing (no engine, no device
            // thread). Removing this arm alone changes no outcome, only
            // how much is built before the refusal.
            Some(declared) => Graph::check_root(declared, Some(&placed))?,
            None => {
                let width = ChannelLayout::try_from(root).map_err(|e| {
                    tutti_buffers::FoldError::from(tutti_buffers::BufferError::from(e))
                })?;
                tutti_graph::fold_to_topology::<f32>(&ChannelTopology::assumed(width), &placed)?;
            }
        }
        Ok(())
    }

    /// Declares the builder's root on the graph the engine was just built
    /// over, which publishes it to the engine.
    fn declare(&self, graph: &mut Graph) -> Result<()> {
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
    use tutti_types::{ChannelLayout, MaxBlock};
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
            session.heard.as_ref().map(|(_, layout)| *layout),
            Some(ChannelLayout::STEREO)
        );
    }

    /// **The setters are honoured, not the defaults.** A rate, a width and a
    /// block that are none of the defaults come back out of the session.
    #[test]
    fn the_setters_choose_the_rate_the_width_and_the_block() {
        let quad = ChannelLayout::from_count(4);
        let block = MaxBlock::of(Samples(256));
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
        assert_eq!(session.heard.as_ref().map(|(_, l)| *l), Some(quad));
    }
}
