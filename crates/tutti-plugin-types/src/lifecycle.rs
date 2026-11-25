//! The plugin lifecycle as a pure function.
//!
//! [`step`] decides activation, restart, rescan and the latency, tail and
//! layout re-reads for every plugin format. It has no FFI, no plugin and no
//! clock: an [`Event`] goes in, a new [`State`] and the [`Effect`]s to perform
//! come out. A shell performs each effect against its plugin and feeds the
//! result back as [`Event::Replied`].
//!
//! Everything here runs on the control (main) thread. The only effects that
//! touch the realtime thread are [`Effect::Recall`] and [`Effect::Lend`], which
//! move the Processor between the threads.
//!
//! # Plans
//!
//! A transition is a split or a rejoin: [`Effect::CallActivate`] turns a whole
//! plugin into two halves, [`Effect::CallDeactivate`] and [`Effect::Rebuild`]
//! need the halves back together. A plan that deactivates or rebuilds an active
//! plugin therefore starts with [`Effect::Recall`]. A step emits effects up to
//! and including the next one that can fail or wait (`Recall`, `CallDeactivate`,
//! `Rebuild`, `CallActivate`) and keeps the rest of the plan in the [`State`]; the
//! reply to that effect continues it. A plan parked behind a recall that found
//! the Processor lent runs once [`Event::Returned`] arrives.

use std::sync::Arc;

use arrayvec::ArrayVec;
use tutti_buffers::FloatWidth;
use tutti_plugin_ipc_ffi::FdId;

use crate::{
    IpcError, Layout, MaxBlock, PluginError, PluginTail, RateRefusal, RateRefusalCause, RenderMode,
    RenderModeSupport, SampleRate, Samples, TransportRequest,
};

/// How many effects one [`step`] can emit.
///
/// A bound, so a step never allocates its output and never grows with its
/// input.
pub const EFFECT_CAPACITY: usize = 15;

/// The effects of one [`step`], in the order to perform them.
pub(crate) type Effects = ArrayVec<Effect, EFFECT_CAPACITY>;

/// How many due timers one tick runs. The rest stay pending: see
/// [`State::has_backlog`].
pub const TIMERS_PER_TICK: usize = 3;

/// How many ready file descriptors one tick services. The rest stay pending:
/// see [`State::has_backlog`].
pub const FDS_PER_TICK: usize = 3;

/// The most effects one [`step`] emits: a tick that runs a full set of timers
/// and descriptors, reads the latency, tail and layout, notifies a values
/// rescan, reads and notifies an info rescan, rebuilds the MIDI mapping and tells it,
/// and notifies a note-expression change. No other step comes near it;
/// the assertion below keeps [`EFFECT_CAPACITY`] at or above it.
pub const MAX_STEP_EFFECTS: usize = TIMERS_PER_TICK + FDS_PER_TICK + 9;

const _: () = assert!(MAX_STEP_EFFECTS <= EFFECT_CAPACITY);

// The id widths follow the formats' ABIs (CLAP's `clap_id` timer, a POSIX fd).

/// A plugin timer, as the format names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub u32);

/// What `activate` needs, and what a change of any field costs: a deactivate
/// and an activate.
///
/// A [`SampleRate`] is finite and positive and a block holds at least one
/// frame, so no host is handed an activation its format would refuse.
///
/// # Examples
///
/// ```
/// use tutti_plugin_types::{MaxBlock, Prepare, RenderMode};
/// use tutti_units::{SampleRate, Samples};
///
/// let block = MaxBlock::of(Samples::new(512));
/// let prepare = Prepare::new(SampleRate::SR_48K, block);
/// assert_eq!(prepare.mode(), RenderMode::Realtime);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prepare {
    rate: SampleRate,
    max_block: MaxBlock,
    format: FloatWidth,
    mode: RenderMode,
    quantum: Option<MaxBlock>,
}

impl Prepare {
    /// An activation at `rate` with blocks of up to `max_block` frames, in
    /// 32-bit audio under realtime pressure.
    #[must_use]
    pub fn new(rate: SampleRate, max_block: MaxBlock) -> Self {
        Self {
            rate,
            max_block,
            format: FloatWidth::F32,
            mode: RenderMode::default(),
            quantum: None,
        }
    }

    /// This activation, with blocks of up to `max_block` frames.
    #[must_use]
    pub const fn with_max_block(mut self, max_block: MaxBlock) -> Self {
        self.max_block = max_block;
        self
    }

    /// This activation, for a device whose callbacks carry `quantum` frames:
    /// what a served plugin's pipeline is sized by. Unset by
    /// [`new`](Self::new).
    #[must_use]
    pub const fn with_quantum(mut self, quantum: MaxBlock) -> Self {
        self.quantum = Some(quantum);
        self
    }

    /// This activation, at `rate`.
    #[must_use]
    pub const fn with_rate(mut self, rate: SampleRate) -> Self {
        self.rate = rate;
        self
    }

    /// This activation, exchanging audio in `format`.
    #[must_use]
    pub const fn with_format(mut self, format: FloatWidth) -> Self {
        self.format = format;
        self
    }

    /// This activation, under `mode`.
    #[must_use]
    pub const fn with_mode(mut self, mode: RenderMode) -> Self {
        self.mode = mode;
        self
    }

    /// The sample rate blocks are rendered at.
    #[must_use]
    pub const fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The most frames one block carries.
    #[must_use]
    pub const fn max_block(&self) -> MaxBlock {
        self.max_block
    }

    /// The device's callback size, when one is known. A [`MaxBlock`], as
    /// `tutti_graph::Prepare::quantum` is: the longest callback is the same
    /// quantity as the longest block.
    #[must_use]
    pub const fn quantum(&self) -> Option<MaxBlock> {
        self.quantum
    }

    /// The sample width audio is exchanged in.
    #[must_use]
    pub const fn format(&self) -> FloatWidth {
        self.format
    }

    /// Whether blocks arrive under realtime pressure.
    #[must_use]
    pub const fn mode(&self) -> RenderMode {
        self.mode
    }
}

/// The fields of a [`Prepare`] a live retune takes ([`Processor::RETUNE`](crate::Processor::RETUNE)):
/// the one place they are named, for what a retune applies and for what still
/// needs an activation.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Live {
    rate: SampleRate,
    max_block: MaxBlock,
    quantum: Option<MaxBlock>,
}

impl Prepare {
    /// What an activation takes and nothing else: the render mode, which a
    /// plugin may take live, is set aside, and so is the callback size unless
    /// `retunes` (only a Processor that retunes takes it, and one out of hand
    /// is activated for it); a block ceiling at or below the one `told` is the
    /// one it was told (its blocks are split to it).
    fn restart_key(self, told: Self, retunes: bool) -> Self {
        let max_block = if self.max_block.get() > told.max_block.get() {
            self.max_block
        } else {
            told.max_block
        };
        Self {
            quantum: if retunes { self.quantum } else { None },
            mode: RenderMode::default(),
            max_block,
            ..self
        }
    }

    fn live(&self) -> Live {
        Live {
            rate: self.rate,
            max_block: self.max_block,
            quantum: self.quantum,
        }
    }

    fn with_live(mut self, live: Live) -> Self {
        self.rate = live.rate;
        self.max_block = live.max_block;
        self.quantum = live.quantum;
        self
    }
}

/// Where a plugin stands after a refused rate, for the error: the rate it runs
/// at, or that it is deactivated. Every format's refusal says it the same way.
#[must_use]
pub fn standing(active: bool, rate: SampleRate) -> String {
    if active {
        format!("running at {}", rate.get())
    } else {
        "deactivated".to_string()
    }
}

/// Which changes a format needs a restart for.
///
/// A format not listed here re-reads the figure instead. A plugin's own
/// request to restart, a reload and a rescan of everything restart on every
/// format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestartRules {
    /// An audio-ports change (`IoChanged`) needs a deactivate and an activate.
    pub io: bool,
    /// A latency change (`LatencyChanged`) needs a deactivate and an activate.
    pub latency: bool,
}

impl RestartRules {
    /// CLAP: latency and I/O.
    pub const CLAP: Self = Self {
        io: true,
        latency: true,
    };
    /// VST3: I/O and latency (and a reload, which every format restarts for).
    pub const VST3: Self = Self {
        io: true,
        latency: true,
    };
    /// VST2: I/O. `audioMasterIOChanged` is the plugin's one notice for its pins, latency and
    /// tail, and the pins are baked into the activation's buffers, so it restarts; the
    /// latency and the tail it also announces are read again after the restart.
    pub const VST2: Self = Self {
        io: true,
        latency: false,
    };
    /// AU: none, it re-reads.
    pub const NONE: Self = Self {
        io: false,
        latency: false,
    };
}

/// What a host asks of a plugin, an effect finished, or the plugin asked.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug)]
pub enum Event {
    /// An operation the host asks for.
    Op(HostOp),
    /// A plugin-initiated call, raised on any thread, delivered here.
    Request(Request),
    /// The main-thread idle pass: act on what has been requested.
    Tick,
    /// What the shell found when it interpreted the last [`Effect`].
    Replied(Reply),
    /// The Processor is back from the realtime thread after a
    /// [`Effect::Recall`] that answered [`Reply::Lent`].
    Returned,
    /// The plugin answered a setting the host told it without waiting
    /// ([`Processor::RETUNE`](crate::Processor::RETUNE), or a render mode it
    /// [answers later](crate::ModeTold::Asked)).
    Answered(Told),
    /// The process hosting the plugin is gone.
    Died,
}

/// What a plugin answered to a setting the host told it live: the rate its
/// Processor retunes to, or the render mode it was told.
///
/// An answer names the setting it answers, and [`step`] applies it only while
/// the state is still waiting for exactly that: a rate that is not the one the
/// Processor was last told, a mode that is not the one last asked, and any
/// answer after an activation began change nothing. What a taken rate applies
/// is what the Processor was told with it, not what the host wants since.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug, Clone, PartialEq)]
pub enum Told {
    /// The plugin runs at the rate.
    RateTook(SampleRate),
    /// The plugin did not take the rate, and what it was left as.
    RateRefused(RateRefusal),
    /// The plugin runs in the mode.
    ModeTook(RenderMode),
    /// The plugin did not take the mode: it runs in the mode it had.
    ModeRefused(RenderMode),
}

/// How far a plugin is from what the host wants, and the cheapest step that
/// closes it ([`State::behind`]).
///
/// Exhaustive: every shell matches every variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behind {
    /// The plugin runs as the host wants, or what differs is told and not yet
    /// answered.
    No,
    /// Only the render mode differs, and the plugin takes it live
    /// ([`HostOp::SetMode`]).
    Mode(RenderMode),
    /// Something an activation takes differs ([`HostOp::Reconfigure`]).
    Restart,
}

/// What the host asks for.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostOp {
    /// Split the plugin for rendering. A plugin already active is left as it is.
    Activate(Prepare),
    /// A new rate, block size, format or mode: deactivate and activate again.
    /// On a plugin that is not active this is [`Activate`](Self::Activate).
    Reconfigure(Prepare),
    /// Tell an active plugin its render mode, with no deactivation: for a
    /// plugin that takes it live ([`RenderModeSupport::Live`]). On a plugin
    /// that is not active the mode waits in the wanted [`Prepare`] for the next
    /// activation.
    SetMode(RenderMode),
    /// Rejoin the halves and stop rendering.
    Deactivate,
    /// Bring the Processor back to the main thread and lend it again, with
    /// no deactivation: the plugin keeps running, and the lend goes to the
    /// realtime end [`Hand::install`](crate::Hand::install) named. On a plugin
    /// that is not active, or while a plan is under way, this does nothing: an
    /// activation in flight lends at its end.
    Relend,
}

/// What the plugin called on the host.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Request {
    /// The plugin asked to be restarted.
    Restart,
    /// An audio-ports rescan (CLAP), `kIoChanged` (VST3), `audioMasterIOChanged`
    /// (VST2), a stream-format property (AU).
    IoChanged,
    /// Parameters changed; the scope says how much.
    RescanParams(RescanScope),
    /// The latency changed.
    LatencyChanged,
    /// The tail changed.
    TailChanged,
    /// `kReloadComponent`: rebuild the instance.
    Reload,
    /// `request_callback` (CLAP): run `on_main_thread`. Dropped while the
    /// plugin is `Refused`, which has nothing to run.
    Callback,
    /// VST3 `kMidiCCAssignmentChanged`: the plugin's controller-to-parameter
    /// mapping changed, so the host rebuilds its own.
    MidiCcAssignmentChanged,
    /// VST3 `kNoteExpressionChanged`: the plugin's note-expression types changed.
    NoteExpressionChanged,
    /// A registered timer is due.
    Timer(TimerId),
    /// A registered file descriptor is ready.
    Fd(FdId),
    /// The plugin asked the transport to do something (CLAP
    /// `transport-control`). Not the plugin's own affair: the lifecycle passes
    /// it on as a [`Notice::Transport`] for the application to answer.
    Transport(TransportRequest),
}

/// How much of the parameter set a rescan touches.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RescanScope {
    /// Values only.
    Values,
    /// Display text only.
    Text,
    /// Names, ranges and flags.
    Info,
    /// Everything, including a change that needs a restart.
    All,
}

/// What the shell is asked to do. Each effect but [`Notify`](Self::Notify) is
/// answered by an [`Event::Replied`]; `Notify` is delivered to the host and
/// has no reply.
///
/// Exhaustive: every shell matches every variant.
#[derive(Debug)]
pub enum Effect {
    /// Splits the plugin into a Processor and a Controller.
    CallActivate(Prepare),
    /// Rejoins the halves. Needs both in hand, so [`Recall`](Self::Recall)
    /// precedes it.
    CallDeactivate,
    /// Rebuild the instance from its load parameters (VST3 reload). Needs the
    /// plugin loaded, so a `Recall` and a `CallDeactivate` precede it.
    Rebuild,
    /// Ask the realtime thread to hand the Processor back, between blocks.
    Recall,
    /// Give the Processor to the realtime thread.
    Lend,
    /// Tell the plugin its render mode. Answered by [`Reply::Answered`], or
    /// by [`Reply::Asked`] when the plugin answers later ([`Event::Answered`]).
    SetMode(RenderMode),
    /// Read the latency from the plugin.
    ReadLatency,
    /// Read the tail from the plugin.
    ReadTail,
    /// Read the audio-port layout from the plugin.
    ReadLayout,
    /// Re-read the parameter set.
    ReadParams(RescanScope),
    /// Rebuild the host's MIDI controller-to-parameter mapping from the plugin.
    RebuildMidiMapping,
    /// Run the plugin's main-thread work (CLAP `on_main_thread`).
    RunMainThread,
    /// Run a due plugin timer.
    RunTimer(TimerId),
    /// Service a ready plugin file descriptor.
    ServiceFd(FdId),
    /// Tell the host.
    Notify(Notice),
}

/// What the shell found when it performed an [`Effect`].
///
/// Exhaustive: [`step`] matches every variant.
#[derive(Debug)]
pub enum Reply {
    /// `CallActivate`: the plugin is split.
    Activated,
    /// `CallActivate` or `Rebuild`: the plugin would not take it and is loaded.
    Refused(PluginError),
    /// `CallDeactivate`: the halves are rejoined.
    Deactivated,
    /// `Rebuild`: the instance is rebuilt and loaded, and `restored` says
    /// whether it took the state the old one was saved with.
    Rebuilt {
        /// `Err` when the rebuilt instance is at its defaults.
        restored: Result<(), PluginError>,
    },
    /// `ReadLatency`: the figure.
    Latency(Samples),
    /// `ReadTail`: the figure.
    Tail(PluginTail),
    /// `ReadLayout`: the layout.
    Layout(Layout),
    /// The effect needs a state the plugin is not in (a read while loaded).
    NotActive,
    /// `Recall`: the Processor is already held.
    Returned,
    /// `Recall`: the Processor is on the realtime thread; [`Event::Returned`]
    /// follows.
    Lent,
    /// `CallDeactivate`: the plugin would not stop. Both halves are intact.
    Stuck(PluginError),
    /// `SetMode`: what the plugin answered.
    Answered(Told),
    /// `SetMode`: the plugin was told, and answers later ([`Event::Answered`]).
    Asked,
    /// Any other effect, performed.
    Done,
}

/// What the host is told, in the order it happened.
///
/// Exhaustive: every host matches every variant.
#[derive(Debug)]
pub enum Notice {
    /// The latency moved, or the plugin announced it.
    LatencyChanged(Samples),
    /// The tail moved, or the plugin announced it.
    TailChanged(PluginTail),
    /// The audio-port layout differs from the one the host was last given.
    IoChanged,
    /// Parameter values or their display text changed.
    ParamValuesChanged,
    /// The plugin's note-expression types changed.
    NoteExpressionChanged,
    /// Parameters were added, removed or described differently.
    ParamTitlesChanged,
    /// The plugin's declared controller table changed and was re-read
    /// ([`Control::rebuild_midi_mapping`](crate::Control::rebuild_midi_mapping)):
    /// [`Control::wiring`](crate::Control::wiring) answers the new one.
    MidiMapChanged,
    /// The instance was rebuilt.
    Reloaded,
    /// The rebuilt instance did not take the state the old one was saved with:
    /// it is at its defaults. Follows [`Reloaded`](Self::Reloaded).
    StateNotRestored {
        /// Why.
        reason: PluginError,
    },
    /// The plugin asked the host's transport to do something.
    Transport(TransportRequest),
    /// The plugin refused to run at `rate`. `recovery` says whether it runs
    /// again at the setting it last accepted; when it does not, it renders
    /// nothing until an activation succeeds.
    Refused {
        /// The rate that was refused.
        rate: SampleRate,
        /// Why.
        reason: Arc<PluginError>,
        /// What the core did about it.
        recovery: Recovery,
    },
    /// The plugin would not stop; it is as it was.
    Stuck {
        /// Why.
        reason: PluginError,
    },
    /// The plugin did not take `mode`: it runs in the mode it had.
    ModeRefused {
        /// The mode that was asked for.
        mode: RenderMode,
    },
    /// A call into the plugin panicked outside an activation, a deactivation or
    /// a rebuild (those tell the host through [`Refused`](Self::Refused)). The
    /// plugin is lost and nothing leaves [`Stage::Poisoned`].
    Lost {
        /// Why.
        reason: PluginError,
    },
}

/// What the core did after the plugin refused an activation: it activates
/// again with the setting the plugin last accepted ([`State::applied`]), so a
/// refused rate or mode never leaves a plugin that ran deactivated.
///
/// A plugin lost during the activation is told by this notice with
/// [`Inactive`](Self::Inactive), not by [`Notice::Lost`]: a shell reports the
/// loss by its own route (the instance's lost latch, the server's exit), and
/// does not take the refusal for a refused rate.
///
/// Exhaustive: every shell matches every variant.
#[derive(Debug, Clone)]
pub enum Recovery {
    /// No other setting was run: the plugin had accepted none besides the one
    /// refused (a first activation, or a restart at the setting it already
    /// runs at) or a rebuild was refused: the host concludes the plugin is not
    /// active until an activation succeeds.
    Inactive,
    /// The plugin is active again, at `running`, the setting it last accepted:
    /// the host concludes it runs there, not at the setting it asked for.
    RolledBack {
        /// The setting the plugin runs at.
        running: Prepare,
    },
    /// The activation at the accepted setting was refused too: the host
    /// concludes the plugin is not active until an activation succeeds.
    RollbackRefused {
        /// Why the accepted setting was refused.
        reason: Arc<PluginError>,
    },
}

/// Where a plugin is in its life.
///
/// Exhaustive: [`step`] and every shell match every variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stage {
    /// Loaded and not running.
    Loaded,
    /// Split into a Processor and a Controller.
    Active,
    /// Loaded; it refused to run at `rate` and renders nothing. An activation
    /// retries.
    Refused {
        /// The rate that was refused.
        rate: SampleRate,
    },
    /// The plugin is lost. Reached only by [`Event::Died`], which the shell
    /// sends when a transition leaves no plugin (a failed activation that hands
    /// nothing back, a panic) or the process hosting it is gone. Nothing leaves
    /// this stage.
    Poisoned,
}

/// What a plan still has to do, front first.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Task {
    Deactivate,
    Rebuild,
    Activate(Prepare),
    ReadParams(RescanScope),
    RebuildMidiMapping,
    Lend,
}

/// The effect a plan is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Wait {
    #[default]
    Idle,
    /// A `Recall` is in flight.
    Recall,
    /// The Processor is on the realtime thread: the plan resumes on
    /// [`Event::Returned`].
    Parked,
    Deactivate,
    Rebuild,
    Activate,
    /// The activation at the accepted setting that follows a refused one.
    Rollback,
}

/// The refusal a rollback is answering.
#[derive(Debug, Clone)]
struct Refusal {
    rate: SampleRate,
    reason: Arc<PluginError>,
}

#[derive(Debug, Clone, Default)]
struct Plan {
    tasks: ArrayVec<Task, 5>,
    wait: Wait,
}

/// What the plugin has asked for and the next tick has not yet acted on.
#[derive(Debug, Clone, Default)]
struct Pending {
    restart: bool,
    io: bool,
    latency: bool,
    tail: bool,
    reload: bool,
    callback: bool,
    values: bool,
    midi_cc: bool,
    note_expr: bool,
    text: bool,
    info: bool,
    all: bool,
    timers: Vec<TimerId>,
    fds: Vec<FdId>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        !(self.restart
            || self.io
            || self.latency
            || self.tail
            || self.reload
            || self.callback
            || self.values
            || self.midi_cc
            || self.note_expr
            || self.text
            || self.info
            || self.all)
            && self.timers.is_empty()
            && self.fds.is_empty()
    }

    fn clear_requests(&mut self) {
        let (timers, fds, callback) = (
            std::mem::take(&mut self.timers),
            std::mem::take(&mut self.fds),
            self.callback,
        );
        *self = Self::default();
        self.timers = timers;
        self.fds = fds;
        self.callback = callback;
    }
}

/// The render modes a plugin was told and has not answered, counted.
///
/// Relies on one FIFO command lane per instance and one answer per tell. The
/// client's control lane sends each `SetRenderMode` and waits for its reply
/// before the next (`handle_setting`'s `Setting::RenderMode` arm in
/// `tutti-plugin-ipc`'s `client/audio/dispatch.rs`), and the settings it takes
/// queue without replacing one another (`Settings::modes` and
/// `Requests::modes` in `channels.rs`); the server answers every request on
/// that connection in order (`Handle<SetRenderMode>` in
/// `tutti-plugin-server`'s `session/handlers.rs`); the answers reach the
/// lifecycle in that order through `Unsolicited::modes` and
/// `Control::take_answers`. A plugin in this process answers each tell at
/// once, through `Reply::Answered`, so its count goes 0, 1, 0. A lost bridge
/// answers nothing and poisons the state, which clears the count.
#[derive(Debug, Clone, Copy, Default)]
struct ModeTells {
    /// Tells not yet answered.
    out: u32,
    /// How many of the oldest answers belong to tells an activation settled.
    stale: u32,
    /// The mode of the latest tell, while any is out.
    last: Option<RenderMode>,
}

impl ModeTells {
    /// The plugin was told `mode`.
    fn told(&mut self, mode: RenderMode) {
        self.out += 1;
        self.last = Some(mode);
    }

    /// Every tell out is settled by an activation: its answer only counts.
    fn superseded(&mut self) {
        self.stale = self.out;
        self.last = None;
    }

    /// An answer arrived; whether it is to a tell still waited for.
    fn answered(&mut self) -> bool {
        if self.stale > 0 {
            self.stale -= 1;
            self.out = self.out.saturating_sub(1);
            if self.out == 0 {
                self.last = None;
            }
            return false;
        }
        self.out = self.out.saturating_sub(1);
        if self.out == 0 {
            self.last = None;
        }
        true
    }

    /// Whether the answer about to be counted is to the latest tell, of `mode`.
    fn is_latest(&self, mode: RenderMode) -> bool {
        self.out == 1 && self.stale == 0 && self.last == Some(mode)
    }

    /// The mode the plugin will run in once its tells are answered, if any is
    /// out and still waited for.
    fn pending(&self) -> Option<RenderMode> {
        self.last
    }
}

/// The lifecycle decisions of one plugin: where it is, what it last reported
/// and what it still has to do.
///
/// A value: [`step`] consumes one and returns the next.
#[derive(Debug, Clone)]
pub struct State {
    rules: RestartRules,
    stage: Stage,
    /// What the plugin last accepted: set when an activation succeeds.
    applied: Option<Prepare>,
    /// What the activation in flight asks for.
    trying: Option<Prepare>,
    /// What the host wants the plugin to run at: the latest `Prepare` it asked
    /// for, applied or not. A refusal gives it up (the host asks again).
    wanted: Option<Prepare>,
    /// The refusal the rollback in flight answers.
    refusal: Option<Refusal>,
    /// The render modes the plugin was told and has not answered.
    tells: ModeTells,
    /// The live fields the Processor was told and the plugin has not answered.
    retuning: Option<Live>,
    latency: Option<Samples>,
    tail: Option<PluginTail>,
    layout: Option<Layout>,
    announced_latency: bool,
    announced_tail: bool,
    revived: bool,
    after_main: bool,
    /// The host's latest op, raised while an activation was in flight.
    later: Option<HostOp>,
    /// A rebuild was refused: the next activation rebuilds first.
    rebuild_pending: bool,
    /// Parameters changed while no activation was to follow: the next
    /// activation reads them all.
    reread_params: bool,
    pending: Pending,
    plan: Plan,
}

impl State {
    /// A freshly loaded plugin of a format with these restart rules.
    #[must_use]
    pub fn new(rules: RestartRules) -> Self {
        Self {
            rules,
            stage: Stage::Loaded,
            applied: None,
            trying: None,
            wanted: None,
            refusal: None,
            tells: ModeTells::default(),
            retuning: None,
            latency: None,
            tail: None,
            layout: None,
            announced_latency: false,
            announced_tail: false,
            revived: false,
            after_main: false,
            later: None,
            rebuild_pending: false,
            reread_params: false,
            pending: Pending::default(),
            plan: Plan::default(),
        }
    }

    /// Whether an [`Event::Tick`] would plan nothing: no request is pending, no
    /// plan is in flight and no op is waiting to be replayed.
    ///
    /// A shell that has taken no request since its last pass can skip
    /// [`Lifecycle::drive`](crate::Lifecycle::drive) for the tick while this
    /// holds (and call [`Control::flush_edits`](crate::Control::flush_edits)
    /// itself, which is all else a tick does), so an idle pass of a realtime
    /// thread allocates nothing. Conservative: `false` may still tick to nothing.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        matches!(self.plan.wait, Wait::Idle | Wait::Parked)
            && self.plan.tasks.is_empty()
            && self.later.is_none()
            && !self.after_main
            && self.pending.is_empty()
    }

    /// Where the plugin is.
    #[must_use]
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Whether the plugin is split and producing audio: true only in
    /// [`Stage::Active`].
    #[must_use]
    pub fn renders(&self) -> bool {
        matches!(self.stage, Stage::Active)
    }

    /// The `Prepare` the plugin last accepted: it changes only when an
    /// activation succeeds. A refused activation leaves it, and a plugin's own
    /// restart and every rollback activate with it.
    #[must_use]
    pub fn applied(&self) -> Option<Prepare> {
        self.applied
    }

    /// The `Prepare` the host wants the plugin to run at: the latest it asked
    /// for ([`want`](Self::want), or an activation op), else
    /// [`applied`](Self::applied). A request for one setting builds on it, so
    /// it keeps what an earlier, unanswered request asked for.
    #[must_use]
    pub fn wanted(&self) -> Option<Prepare> {
        self.wanted.or(self.applied)
    }

    /// Records what the host wants, without activating anything: the shell
    /// compares it with [`applied`](Self::applied) and reconfigures.
    pub fn want(&mut self, wanted: Prepare) {
        self.wanted = Some(wanted);
    }

    /// Records that the Processor in hand was told the wanted rate, block
    /// ceiling and callback size outside a plan: they are in flight until the
    /// plugin answers ([`Event::Answered`]), and not behind meanwhile.
    pub fn retune_asked(&mut self) {
        self.retuning = self.wanted().map(|w| w.live());
    }

    /// Records that the plugin was told the wanted render mode outside a plan:
    /// it is in flight until the plugin answers ([`Event::Answered`]), and not
    /// behind meanwhile.
    pub fn mode_asked(&mut self, mode: RenderMode) {
        self.tells.told(mode);
    }

    /// How far the plugin is from what the host wants, and the cheapest step
    /// that closes it.
    ///
    /// `retunes` is whether its Processor takes the rate, block ceiling and
    /// callback size live ([`RETUNE`](crate::Processor::RETUNE)): the node
    /// tells it, and a difference in them is [`Behind::Restart`] only while the
    /// Processor has not been told ([`retune_asked`](Self::retune_asked)); an
    /// answer makes it what the plugin has. A Processor that does not retune
    /// takes no callback size, so a difference in it is not a restart there.
    /// A difference in any other field but the render mode needs an
    /// activation, which no field added to a [`Prepare`] escapes. `support` is how the plugin
    /// takes a render mode: a plugin that takes it live is told
    /// ([`Behind::Mode`]) until it answers, one that reads it at activation
    /// restarts, and one with no mode is never behind in it.
    #[must_use]
    pub fn behind(&self, retunes: bool, support: RenderModeSupport) -> Behind {
        let (Some(wanted), Some(applied)) = (self.wanted(), self.applied) else {
            return Behind::No;
        };
        let (w, a) = (
            wanted.restart_key(applied, retunes),
            applied.restart_key(applied, retunes),
        );
        let told = retunes && self.retuning == Some(wanted.live());
        let compared = if told { w.with_live(a.live()) } else { w };
        if compared != a {
            return Behind::Restart;
        }
        if wanted.mode == applied.mode {
            return Behind::No;
        }
        match support {
            RenderModeSupport::Unsupported => Behind::No,
            RenderModeSupport::OnActivation => Behind::Restart,
            RenderModeSupport::Live => {
                if self.tells.pending() == Some(wanted.mode) {
                    Behind::No
                } else {
                    Behind::Mode(wanted.mode)
                }
            }
        }
    }

    /// The latency last reported.
    #[must_use]
    pub fn latency(&self) -> Option<Samples> {
        self.latency
    }

    /// The tail last reported.
    #[must_use]
    pub fn tail(&self) -> Option<PluginTail> {
        self.tail
    }

    /// The layout last reported.
    #[must_use]
    pub fn layout(&self) -> Option<&Layout> {
        self.layout.as_ref()
    }

    /// Whether the rest of a plan waits for the Processor to return from the
    /// realtime thread.
    #[must_use]
    pub fn is_parked(&self) -> bool {
        self.plan.wait == Wait::Parked
    }

    /// Whether timers or file descriptors are left over from a tick that ran
    /// its share ([`TIMERS_PER_TICK`], [`FDS_PER_TICK`]): the shell ticks again.
    #[must_use]
    pub fn has_backlog(&self) -> bool {
        !self.pending.timers.is_empty() || !self.pending.fds.is_empty()
    }

    /// Whether a plan is under way, so an effect's reply is awaited.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.plan.wait != Wait::Idle
    }
}

/// Decides what happens next.
///
/// Total: every event in every state yields a state and at most
/// [`EFFECT_CAPACITY`] effects. A reply that answers no effect in flight is
/// ignored.
#[must_use = "the effects are the decisions"]
pub fn step(mut state: State, event: Event) -> (State, Effects) {
    let mut out = Effects::new();
    state.on_event(event, &mut out);
    (state, out)
}

fn emit(out: &mut Effects, effect: Effect) {
    // A dropped effect would wedge the plugin mid-plan, so overflow is a bug
    // that must stop. `MAX_STEP_EFFECTS` bounds every step below the capacity.
    assert!(
        out.try_push(effect).is_ok(),
        "a step exceeded EFFECT_CAPACITY"
    );
}

impl State {
    fn on_event(&mut self, event: Event, out: &mut Effects) {
        if matches!(self.stage, Stage::Poisoned) {
            return;
        }
        match event {
            Event::Died => {
                // A refusal whose rollback never answered is still told.
                let refusal = self.refusal.take();
                self.poison();
                if let Some(Refusal { rate, reason }) = refusal {
                    let recovery = Recovery::Inactive;
                    emit(
                        out,
                        Effect::Notify(Notice::Refused {
                            rate,
                            reason,
                            recovery,
                        }),
                    );
                }
            }
            Event::Op(op) => self.on_op(op, out),
            // Not the plugin's own affair: nothing is pending, the one event
            // is the one notice.
            Event::Request(Request::Transport(asked)) => {
                emit(out, Effect::Notify(Notice::Transport(asked)));
            }
            Event::Request(request) => self.on_request(request),
            Event::Tick => self.tick(out, true),
            Event::Replied(reply) => self.on_reply(reply, out),
            Event::Answered(answer) => self.on_answer(answer, out),
            Event::Returned => {
                if self.plan.wait == Wait::Parked {
                    self.advance(out);
                }
            }
        }
    }

    fn poison(&mut self) {
        self.stage = Stage::Poisoned;
        self.trying = None;
        self.refusal = None;
        self.tells = ModeTells::default();
        self.retuning = None;
        self.plan = Plan::default();
        self.pending = Pending::default();
        self.after_main = false;
        self.later = None;
        self.rebuild_pending = false;
        self.reread_params = false;
    }

    fn on_op(&mut self, op: HostOp, out: &mut Effects) {
        match op {
            // An activate leaves an active plugin as it is: it wants nothing.
            HostOp::Activate(p) if !self.leads_active() => self.wanted = Some(p),
            HostOp::Reconfigure(p) => self.wanted = Some(p),
            HostOp::SetMode(mode) => self.wanted = self.wanted().map(|w| w.with_mode(mode)),
            HostOp::Activate(_) | HostOp::Deactivate | HostOp::Relend => {}
        }
        // A relend asks for nothing an activation in flight does not do: it
        // lends at its end.
        if op == HostOp::Relend && self.activating() {
            return;
        }
        // An op raised while an activation is in flight cannot change the
        // plan yet: the latest one is replayed once the activation is answered.
        if self.activating() {
            self.later = Some(merge_later(self.later, op));
            return;
        }
        match op {
            HostOp::Activate(p) => match self.plan.wait {
                Wait::Idle => {
                    if !matches!(self.stage, Stage::Active) {
                        self.begin(&[Task::Activate(p)], out);
                    }
                }
                // An activate leaves an active plugin as it is, and a plan that
                // ends active already has its `Prepare`.
                _ if self.leads_active() => {}
                _ => self.plan.set_activate(p),
            },
            HostOp::Reconfigure(p) => match (self.plan.wait, self.stage) {
                (Wait::Idle, Stage::Active) => {
                    self.begin(&[Task::Deactivate, Task::Activate(p)], out)
                }
                (Wait::Idle, _) => self.begin(&[Task::Activate(p)], out),
                _ => self.plan.set_activate(p),
            },
            HostOp::SetMode(mode) => {
                // Against what the plugin will run in once the ask out is
                // answered, so a newer want while one is out is told.
                let behind = self
                    .applied
                    .is_some_and(|applied| self.tells.pending().unwrap_or(applied.mode) != mode);
                if behind && matches!((self.plan.wait, self.stage), (Wait::Idle, Stage::Active)) {
                    self.tells.told(mode);
                    emit(out, Effect::SetMode(mode));
                }
            }
            HostOp::Relend => {
                if matches!((self.plan.wait, self.stage), (Wait::Idle, Stage::Active)) {
                    self.begin(&[Task::Lend], out);
                }
            }
            HostOp::Deactivate => match (self.plan.wait, self.stage) {
                (Wait::Idle, Stage::Active) => self.begin(&[Task::Deactivate], out),
                (Wait::Recall | Wait::Parked, _) => {
                    self.plan.tasks.clear();
                    self.plan.tasks.push(Task::Deactivate);
                }
                (Wait::Deactivate | Wait::Rebuild, _) => self.plan.tasks.clear(),
                _ => {}
            },
        }
    }

    /// Whether an activation, a rollback's included, is in flight.
    fn activating(&self) -> bool {
        matches!(self.plan.wait, Wait::Activate | Wait::Rollback)
    }

    /// Whether the plugin is, or the plan in flight leaves it, active.
    fn leads_active(&self) -> bool {
        match self.plan.wait {
            Wait::Idle => matches!(self.stage, Stage::Active),
            Wait::Activate | Wait::Rollback => true,
            _ => self
                .plan
                .tasks
                .iter()
                .any(|t| matches!(t, Task::Activate(_))),
        }
    }

    /// Starts a plan: an active plugin is recalled first, a loaded one goes
    /// straight to the first task.
    fn begin(&mut self, tasks: &[Task], out: &mut Effects) {
        self.plan.tasks.clear();
        self.plan.tasks.extend(tasks.iter().copied());
        if self.rebuild_pending && !matches!(self.stage, Stage::Active) {
            self.plan.tasks.insert(0, Task::Rebuild);
        }
        if matches!(self.stage, Stage::Active) {
            emit(out, Effect::Recall);
            self.plan.wait = Wait::Recall;
        } else {
            self.advance(out);
        }
    }

    /// Emits the plan's tasks up to the next effect that is waited on.
    fn advance(&mut self, out: &mut Effects) {
        self.plan.wait = Wait::Idle;
        while !self.plan.tasks.is_empty() {
            match self.plan.tasks.remove(0) {
                Task::Deactivate => {
                    emit(out, Effect::CallDeactivate);
                    self.plan.wait = Wait::Deactivate;
                    return;
                }
                Task::Rebuild => {
                    emit(out, Effect::Rebuild);
                    self.plan.wait = Wait::Rebuild;
                    return;
                }
                Task::Activate(p) => {
                    // What the host wants when the activation starts, not
                    // when it was planned.
                    let wanted = self.wanted().unwrap_or(p);
                    self.activate(wanted, Wait::Activate, out);
                    return;
                }
                Task::RebuildMidiMapping => {
                    emit(out, Effect::RebuildMidiMapping);
                    emit(out, Effect::Notify(Notice::MidiMapChanged));
                }
                Task::Lend => emit(out, Effect::Lend),
                Task::ReadParams(scope) => {
                    emit(out, Effect::ReadParams(scope));
                    if matches!(scope, RescanScope::Info | RescanScope::All) {
                        emit(out, Effect::Notify(Notice::ParamTitlesChanged));
                    }
                    if scope == RescanScope::All {
                        emit(out, Effect::Notify(Notice::ParamValuesChanged));
                    }
                }
            }
        }
    }

    /// Asks the shell to activate at `p`, and waits on `wait`.
    fn activate(&mut self, p: Prepare, wait: Wait, out: &mut Effects) {
        if matches!(self.stage, Stage::Refused { .. }) {
            self.revived = true;
            self.stage = Stage::Loaded;
        }
        // An answer to a setting told before this activation names a setting
        // the activation takes at `p`.
        self.tells.superseded();
        self.retuning = None;
        self.trying = Some(p);
        emit(out, Effect::CallActivate(p));
        self.plan.wait = wait;
    }

    fn on_request(&mut self, request: Request) {
        let active = matches!(self.stage, Stage::Active);
        let activating = self.plan.wait == Wait::Activate;
        let accepted = match request {
            // The plugin runs these whenever it is loaded.
            Request::Callback | Request::Timer(_) | Request::Fd(_) => {
                active || matches!(self.stage, Stage::Loaded)
            }
            // Not while loaded: the reads after an activation answer them.
            Request::LatencyChanged | Request::TailChanged => active,
            // A parameter rescan and a port rescan are legal on a plugin that
            // is loaded and not active (CLAP: `params.rescan` is
            // `[main-thread]`, values and text "take effect immediately"). A
            // loaded plugin has nothing to restart, so the tick answers them.
            Request::RescanParams(_) | Request::IoChanged => {
                active || activating || matches!(self.stage, Stage::Loaded)
            }
            _ => active || activating,
        };
        if !accepted {
            return;
        }
        let p = &mut self.pending;
        match request {
            Request::Restart => p.restart = true,
            Request::IoChanged => p.io = true,
            Request::LatencyChanged => p.latency = true,
            Request::TailChanged => p.tail = true,
            Request::Reload => p.reload = true,
            Request::Callback => p.callback = true,
            Request::MidiCcAssignmentChanged => p.midi_cc = true,
            Request::NoteExpressionChanged => p.note_expr = true,
            Request::RescanParams(RescanScope::Values) => p.values = true,
            Request::RescanParams(RescanScope::Text) => p.text = true,
            Request::RescanParams(RescanScope::Info) => p.info = true,
            Request::RescanParams(RescanScope::All) => p.all = true,
            Request::Timer(id) => {
                if !p.timers.contains(&id) {
                    p.timers.push(id);
                }
            }
            Request::Fd(id) => {
                if !p.fds.contains(&id) {
                    p.fds.push(id);
                }
            }
            // Passed on by `on_event`, before this runs.
            Request::Transport(_) => {}
        }
    }
}

/// The op to replay once the activation in flight is answered, given the one
/// kept so far and a new one. The latest op wins by what it asks for: an
/// `Activate` asks only for an active plugin, which the activation in flight
/// already gives, so it never displaces a kept `Reconfigure` or `Activate`, and
/// after a kept `Deactivate` it asks for an activation at its own `Prepare`.
fn merge_later(kept: Option<HostOp>, op: HostOp) -> HostOp {
    match (op, kept) {
        (HostOp::Activate(p), Some(HostOp::Deactivate)) => HostOp::Reconfigure(p),
        (HostOp::Activate(_), Some(kept)) => kept,
        // A mode asked for beside an activation to come is part of it.
        (HostOp::SetMode(mode), Some(HostOp::Activate(p))) => HostOp::Activate(p.with_mode(mode)),
        (HostOp::SetMode(mode), Some(HostOp::Reconfigure(p))) => {
            HostOp::Reconfigure(p.with_mode(mode))
        }
        (HostOp::SetMode(_), Some(HostOp::Deactivate)) => HostOp::Deactivate,
        (op, _) => op,
    }
}

impl Plan {
    /// Replaces the activation still to come, or adds one.
    fn set_activate(&mut self, p: Prepare) {
        match self
            .tasks
            .iter_mut()
            .find(|t| matches!(t, Task::Activate(_)))
        {
            Some(task) => *task = Task::Activate(p),
            None => {
                let at = self
                    .tasks
                    .iter()
                    .position(|t| matches!(t, Task::ReadParams(_)))
                    .unwrap_or(self.tasks.len());
                self.tasks.insert(at, Task::Activate(p));
            }
        }
    }
}

impl State {
    /// The idle pass. `callback` is false when this pass continues one that
    /// already ran the plugin's main-thread work.
    fn tick(&mut self, out: &mut Effects, callback: bool) {
        if !matches!(self.plan.wait, Wait::Idle | Wait::Parked) {
            return;
        }
        if !matches!(self.stage, Stage::Active | Stage::Loaded) {
            return;
        }
        // The callback may ask for more, so nothing else is decided until it
        // has run.
        if callback && self.pending.callback {
            self.pending.callback = false;
            self.after_main = true;
            emit(out, Effect::RunMainThread);
            return;
        }
        let timers = self.pending.timers.len().min(TIMERS_PER_TICK);
        for id in self.pending.timers.drain(..timers) {
            emit(out, Effect::RunTimer(id));
        }
        let fds = self.pending.fds.len().min(FDS_PER_TICK);
        for id in self.pending.fds.drain(..fds) {
            emit(out, Effect::ServiceFd(id));
        }
        if !matches!(self.stage, Stage::Active) {
            self.read_while_loaded(out);
            return;
        }
        if self.plan.wait == Wait::Idle && self.plan_restart(out) {
            return;
        }
        self.read_without_restart(out);
    }

    /// Plans the restart the pending requests call for. Returns whether there
    /// is one.
    fn plan_restart(&mut self, out: &mut Effects) -> bool {
        let rules = self.rules;
        let p = &self.pending;
        let restart =
            p.reload || p.restart || p.all || (p.io && rules.io) || (p.latency && rules.latency);
        let Some(prepare) = self.applied.filter(|_| restart) else {
            return false;
        };
        let reload = p.reload;
        self.announced_latency |= p.latency;
        self.announced_tail |= p.tail;
        let scope = if reload || p.all {
            Some(RescanScope::All)
        } else if p.info {
            Some(RescanScope::Info)
        } else {
            None
        };
        if scope != Some(RescanScope::All) && (p.values || p.text) {
            emit(out, Effect::Notify(Notice::ParamValuesChanged));
        }
        if p.note_expr {
            emit(out, Effect::Notify(Notice::NoteExpressionChanged));
        }
        let midi_cc = p.midi_cc;
        self.pending.clear_requests();
        let mut tasks = ArrayVec::<Task, 5>::new();
        tasks.push(Task::Deactivate);
        if reload {
            tasks.push(Task::Rebuild);
        }
        tasks.push(Task::Activate(prepare));
        if let Some(scope) = scope {
            tasks.push(Task::ReadParams(scope));
        }
        if midi_cc {
            tasks.push(Task::RebuildMidiMapping);
        }
        self.plan.tasks = tasks;
        emit(out, Effect::Recall);
        self.plan.wait = Wait::Recall;
        true
    }

    /// What a rescan raised while the plugin was loaded and not active asks
    /// for: the parameters read again and the host told, and the host told the
    /// ports changed (it reads them from the plugin it holds). There is no
    /// restart to plan, so nothing is left pending for an activation to
    /// misread as one.
    fn read_while_loaded(&mut self, out: &mut Effects) {
        let p = &mut self.pending;
        if std::mem::take(&mut p.io) {
            emit(out, Effect::Notify(Notice::IoChanged));
        }
        let all = std::mem::take(&mut p.all);
        let info = std::mem::take(&mut p.info);
        let values = std::mem::take(&mut p.values);
        let text = std::mem::take(&mut p.text);
        if all || info {
            let scope = if all {
                RescanScope::All
            } else {
                RescanScope::Info
            };
            emit(out, Effect::ReadParams(scope));
            emit(out, Effect::Notify(Notice::ParamTitlesChanged));
            if all {
                emit(out, Effect::Notify(Notice::ParamValuesChanged));
            }
        }
        if (values || text) && !all {
            emit(out, Effect::Notify(Notice::ParamValuesChanged));
        }
    }

    /// The reads and notices that need no restart. A request that does is left
    /// pending.
    fn read_without_restart(&mut self, out: &mut Effects) {
        let rules = self.rules;
        let p = &mut self.pending;
        if p.latency && !rules.latency {
            p.latency = false;
            self.announced_latency = true;
            emit(out, Effect::ReadLatency);
        }
        if p.tail {
            p.tail = false;
            self.announced_tail = true;
            emit(out, Effect::ReadTail);
        }
        if p.io && !rules.io {
            p.io = false;
            emit(out, Effect::ReadLayout);
        }
        if p.values || p.text {
            p.values = false;
            p.text = false;
            emit(out, Effect::Notify(Notice::ParamValuesChanged));
        }
        if p.midi_cc {
            p.midi_cc = false;
            emit(out, Effect::RebuildMidiMapping);
            emit(out, Effect::Notify(Notice::MidiMapChanged));
        }
        if p.note_expr {
            p.note_expr = false;
            emit(out, Effect::Notify(Notice::NoteExpressionChanged));
        }
        if p.info && !p.all {
            p.info = false;
            emit(out, Effect::ReadParams(RescanScope::Info));
            emit(out, Effect::Notify(Notice::ParamTitlesChanged));
        }
    }

    fn on_reply(&mut self, reply: Reply, out: &mut Effects) {
        let wait = self.plan.wait;
        // `RunMainThread` is emitted alone, so the next reply of any kind is
        // its answer.
        let continuing = std::mem::take(&mut self.after_main);
        match reply {
            Reply::Activated if matches!(wait, Wait::Activate | Wait::Rollback) => {
                self.stage = Stage::Active;
                self.applied = self.trying.take();
                self.tells.superseded();
                self.retuning = None;
                emit(out, Effect::ReadLatency);
                emit(out, Effect::ReadTail);
                emit(out, Effect::ReadLayout);
                emit(out, Effect::Lend);
                if let (Some(Refusal { rate, reason }), Some(running)) =
                    (self.refusal.take(), self.applied)
                {
                    emit(
                        out,
                        Effect::Notify(Notice::Refused {
                            rate,
                            reason,
                            recovery: Recovery::RolledBack { running },
                        }),
                    );
                }
                if std::mem::take(&mut self.reread_params) {
                    self.plan
                        .tasks
                        .retain(|t| !matches!(t, Task::ReadParams(_)));
                    self.plan.tasks.push(Task::ReadParams(RescanScope::All));
                }
                self.advance(out);
                self.replay(out);
            }
            Reply::Refused(reason)
                if matches!(wait, Wait::Activate | Wait::Rollback | Wait::Rebuild) =>
            {
                self.on_refused(wait, Arc::new(reason), out);
            }
            Reply::Deactivated if wait == Wait::Deactivate => {
                self.stage = Stage::Loaded;
                self.settle_requests();
                self.advance(out);
            }
            Reply::Stuck(reason) if wait == Wait::Deactivate => {
                self.plan = Plan::default();
                emit(out, Effect::Notify(Notice::Stuck { reason }));
                emit(out, Effect::Lend);
            }
            Reply::Rebuilt { restored } if wait == Wait::Rebuild => {
                self.rebuild_pending = false;
                emit(out, Effect::Notify(Notice::Reloaded));
                if let Err(reason) = restored {
                    emit(out, Effect::Notify(Notice::StateNotRestored { reason }));
                }
                self.advance(out);
            }
            Reply::Answered(answer) => self.on_answer(answer, out),
            Reply::Returned if wait == Wait::Recall => self.advance(out),
            Reply::Lent if wait == Wait::Recall => self.plan.wait = Wait::Parked,
            Reply::Latency(now) => {
                let announced = std::mem::take(&mut self.announced_latency);
                let before = self.latency.replace(now);
                if before.is_some_and(|b| b != now || announced) {
                    emit(out, Effect::Notify(Notice::LatencyChanged(now)));
                }
            }
            Reply::Tail(now) => {
                let announced = std::mem::take(&mut self.announced_tail);
                let before = self.tail.replace(now);
                if before.is_some_and(|b| b != now || announced) {
                    emit(out, Effect::Notify(Notice::TailChanged(now)));
                }
            }
            Reply::Layout(now) => {
                let revived = std::mem::take(&mut self.revived);
                let changed = match &self.layout {
                    None => revived,
                    Some(before) => revived || *before != now,
                };
                self.layout = Some(now);
                if changed {
                    emit(out, Effect::Notify(Notice::IoChanged));
                }
            }
            Reply::Done if continuing => self.tick(out, false),
            // A reply to nothing in flight, or one that needs no action.
            _ => {}
        }
    }
}

impl State {
    /// What the plugin answered to a setting it was told live. A taken setting
    /// is what the plugin has ([`applied`](Self::applied)); a refused one is
    /// given up, as a refused activation's is, and the plugin runs as it did.
    /// An answer to a setting that is not the one last told applies nothing:
    /// the plugin is still behind in what the host wants, and
    /// [`behind`](Self::behind) says how.
    fn on_answer(&mut self, answer: Told, out: &mut Effects) {
        match answer {
            Told::RateTook(rate) => {
                let Some(told) = self.retuning.filter(|told| told.rate == rate) else {
                    return;
                };
                self.retuning = None;
                if let Some(applied) = self.applied {
                    self.applied = Some(applied.with_live(told));
                }
            }
            Told::RateRefused(refusal) => {
                let Some(told) = self.retuning.filter(|told| told.rate == refusal.rate) else {
                    return;
                };
                let recovery = match (&refusal.cause, self.applied) {
                    (RateRefusalCause::RolledBack { .. }, Some(running)) => {
                        Recovery::RolledBack { running }
                    }
                    _ => Recovery::Inactive,
                };
                // The refused ask, still the current one, gives up its live
                // fields and keeps the rest (the mode); a different ask made
                // since is the host's later one: it stays, and the plugin is
                // behind in it again.
                if let (Some(wanted), Some(applied)) = (self.wanted, self.applied)
                    && wanted.live() == told
                {
                    self.wanted = Some(wanted.with_live(applied.live()));
                }
                self.retuning = None;
                let rate = refusal.rate;
                let reason = Arc::new(PluginError::Bridge(Box::new(IpcError::RateRefused(
                    refusal,
                ))));
                emit(
                    out,
                    Effect::Notify(Notice::Refused {
                        rate,
                        reason,
                        recovery,
                    }),
                );
            }
            // Each answer is the oldest tell not yet answered. One an
            // activation since settled is stale and only counts: applying it
            // would put back the mode the activation replaced. What the host
            // wants stays its own.
            Told::ModeTook(mode) => {
                if !self.tells.answered() {
                    return;
                }
                if let Some(applied) = &mut self.applied {
                    applied.mode = mode;
                }
            }
            // A refusal of the latest tell gives the want up; one of an
            // earlier tell leaves it, for the later tell's answer to settle.
            Told::ModeRefused(mode) => {
                let latest = self.tells.is_latest(mode);
                if !self.tells.answered() || !latest {
                    return;
                }
                let running = self.applied.map(|applied| applied.mode);
                if let (Some(wanted), Some(running)) = (&mut self.wanted, running)
                    && wanted.mode == mode
                {
                    wanted.mode = running;
                    emit(out, Effect::Notify(Notice::ModeRefused { mode }));
                }
            }
        }
    }

    /// The plugin refused the activation or rebuild `wait` was on. A refused
    /// activation with another accepted setting to go back to is followed by
    /// an activation at it; the host is told once the plugin's standing is
    /// known.
    fn on_refused(&mut self, wait: Wait, reason: Arc<PluginError>, out: &mut Effects) {
        let refused = self.trying.take();
        let (rate, reason, recovery) = match (wait, self.refusal.take()) {
            (
                Wait::Rollback,
                Some(Refusal {
                    rate,
                    reason: first,
                }),
            ) => (rate, first, Recovery::RollbackRefused { reason }),
            (Wait::Activate, _) => {
                let Some(rate) = refused.map(|p| p.rate) else {
                    return;
                };
                (rate, reason, Recovery::Inactive)
            }
            (Wait::Rebuild, _) => {
                let rate = self.plan.tasks.iter().find_map(|t| match t {
                    Task::Activate(p) => Some(p.rate),
                    _ => None,
                });
                let Some(rate) = rate.or(self.applied.map(|p| p.rate)) else {
                    return;
                };
                (rate, reason, Recovery::Inactive)
            }
            _ => return,
        };
        self.rebuild_pending |= wait == Wait::Rebuild;
        self.reread_params = false;
        self.wanted = self.applied;
        self.plan = Plan::default();
        self.pending = Pending::default();
        self.announced_latency = false;
        self.announced_tail = false;
        self.revived = false;
        let back_to = self
            .applied
            .filter(|applied| wait == Wait::Activate && Some(*applied) != refused);
        if let Some(applied) = back_to {
            self.refusal = Some(Refusal { rate, reason });
            // The plugin was refused and is revived, as by any activation out
            // of `Refused`: the host is told its layout again.
            self.revived = true;
            self.activate(applied, Wait::Rollback, out);
            return;
        }
        self.stage = Stage::Refused { rate };
        emit(
            out,
            Effect::Notify(Notice::Refused {
                rate,
                reason,
                recovery,
            }),
        );
        self.replay(out);
    }

    /// Replays the op that was raised during an activation.
    fn replay(&mut self, out: &mut Effects) {
        if let Some(op) = self.later.take() {
            self.on_op(op, out);
        }
    }

    /// The plugin is deactivated and will be activated again by the plan: the
    /// restart-needing requests raised meanwhile are answered by it. The reads
    /// after the activation answer a latency, tail or layout change; a reload
    /// is folded into the plan; a rescan of everything re-reads the parameters.
    fn settle_requests(&mut self) {
        let p = &mut self.pending;
        let (reload, all) = (p.reload, p.all);
        self.announced_latency |= p.latency;
        self.announced_tail |= p.tail;
        p.restart = false;
        p.io = false;
        p.latency = false;
        p.tail = false;
        p.reload = false;
        p.all = false;
        let tasks = &mut self.plan.tasks;
        if reload && !tasks.contains(&Task::Rebuild) {
            tasks.insert(0, Task::Rebuild);
        }
        if reload || all {
            if tasks.iter().any(|t| matches!(t, Task::Activate(_))) {
                tasks.retain(|t| !matches!(t, Task::ReadParams(_)));
                tasks.push(Task::ReadParams(RescanScope::All));
            } else {
                // No activation follows: the parameters are read after the
                // next one.
                self.reread_params = true;
            }
        }
    }
}

#[cfg(test)]
mod tests;
