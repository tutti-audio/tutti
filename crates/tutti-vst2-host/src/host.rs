//! Host-side callbacks the VST2 plugin fires into.
//!
//! The `vst` crate's `Host` trait gets invoked whenever the plugin does
//! something asynchronous — parameter automation, outbound MIDI, or a
//! query for transport state. Those are funnelled back to the rest of the crate
//! through bounded lock-free queues and an atomic `time_info` snapshot, so the
//! audio-thread side never blocks on the main thread.
//!
//! # Why the queues are bounded
//!
//! `automate` and `process_events` are not host-scheduled: the plugin calls
//! them, and it calls them from inside `processReplacing` — the audio thread.
//! Anything they touch is therefore on the RT path, which rules out both halves
//! of an unbounded channel. The allocation is the immediate hazard (a list node
//! per event, at the plugin's discretion, inside the audio callback), and the
//! unboundedness is the slower one: a host that stops draining — an editor
//! open with no `drain_param_changes` caller, a plugin emitting MIDI into a
//! session with no consumer — grows the queue for the lifetime of the instance.
//!
//! [`ArrayQueue`] answers both: fixed storage allocated once at load, and a
//! `push` that refuses rather than growing. Refusing loses events, so each
//! queue pairs with a counter ([`HostState::dropped_param_changes`],
//! [`HostState::dropped_midi_out`]) — a lost knob move and a plugin that never
//! moved one must not look the same.
//!
//! # audioMaster query callbacks
//!
//! `tutti-vst2-ffi`'s `host_dispatch` wires the query opcodes, so the plugin gets
//! a real response rather than a silent 0. Of them:
//!
//! - `get_sample_rate`, `get_block_size` and `get_process_level` return **real
//!   data**: the rate and maximum block the plugin was last told, and
//!   realtime or offline for the render mode, answered by the asking thread
//!   (process thread, main thread, other). The first two are written
//!   before the opcode that tells the plugin, so a query from inside
//!   `effSetSampleRate` reads the new figure.
//! - `io_changed` and `update_display` raise [`Request`]s on an [`AtomicRequestFlags`]
//!   for the main thread to act on (`audioMasterIOChanged` is the plugin's
//!   one notice that its pins, latency or tail moved, so it raises all
//!   three), and `io_changed` answers 1; `can_do` answers what the host really
//!   supports.
//! - `size_window` keeps the plugin's requested editor size for the main thread
//!   (`poll_editor_resize_request`) and answers 1.
//! - `get_automation_state` answers the mode the session set, and the time info
//!   carries it in its automation flags.
//! - `get_input_latency` and `get_output_latency` return
//!   honest neutral defaults — `HostState` has no reference to the value each
//!   would need (see each method's note).

use crate::transport_cell::TransportReader;
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use tutti_plugin_types::{
    AutomationMode, MainThreadMarker, Request, RescanScope, WireMidi, WrittenValues, sysex_packets,
};
use tutti_rt::AtomicSampleRate;
use tutti_rt::{AtomicRequestFlags, Tally, TallyWriter, tally};
use tutti_ump::MidiGroup;
use tutti_units::{Normalized, SampleRate};
use vst::Host;
use vst::to_midi;
use vst::{AutomationState, ProcessLevel, Supported};

/// What the plugin reported of a parameter edit: `audioMasterBeginEdit`,
/// `audioMasterAutomate` (the value, widened from the `f32` the callback carries) and
/// `audioMasterEndEdit`, each with the parameter's index.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ParameterChange {
    /// `audioMasterBeginEdit`: the plugin took hold of the parameter.
    Begin(i32),
    /// `audioMasterAutomate`: the parameter now has this value.
    Automate(i32, Normalized),
    /// `audioMasterEndEdit`: the plugin let go of the parameter.
    End(i32),
}

/// Capacity of the plugin→host parameter-automation queue.
///
/// `audioMasterAutomate` fires once per parameter the plugin moves itself —
/// a knob dragged on its editor surface, or a preset switch touching every
/// parameter at once. The second case is what sizes this: a plugin with a few
/// hundred parameters reloading a preset emits one call each, in a burst, and
/// the host drains once per block. 512 covers that burst outright while
/// staying a fixed 4 KiB of storage.
///
/// Overflowing means a host that has stopped draining, not a plugin that is
/// unusually busy — see [`HostState::dropped_param_changes`].
pub(crate) const PARAM_QUEUE_CAPACITY: usize = 512;

/// Capacity of the plugin→host MIDI-out queue.
///
/// Sized in *blocks*, not events: [`crate::Vst2Processor::process_planes`] drains
/// this at the end of every block, so the steady-state occupancy is one
/// block's emission. `tutti_plugin_types::RT_MIDI_CAPACITY` puts a per-block
/// plugin emission ceiling at 64 events on the same reasoning ("already far
/// above what a plugin emits in normal use"); 256 is four such blocks, so the
/// queue absorbs a burst that spans several blocks and only refuses when
/// nothing is draining it at all.
///
/// Deliberately not matched to `tutti-plugin-types`' `MIDI_STACK_CAPACITY`
/// (also 256) — that number bounds a *wire message*, and the two would drift
/// apart for unrelated reasons.
pub(crate) const MIDI_OUT_QUEUE_CAPACITY: usize = 256;

/// [`HostState::automation`]'s read bit: the host plays automation back.
const AUTOMATION_READ: u8 = 1;
/// [`HostState::automation`]'s write bit: the host records automation.
const AUTOMATION_WRITE: u8 = 2;

/// An address unique to the calling thread while it lives: that of a thread-local byte.
/// Reading it neither allocates nor locks, so the audio thread may.
fn thread_token() -> usize {
    thread_local! {
        static TOKEN: u8 = const { 0 };
    }
    TOKEN.with(|t| std::ptr::from_ref(t) as usize)
}

/// The span of one process call on a thread, ended on drop (a panicking plugin included).
pub(crate) struct ProcessScope<'a>(&'a HostState);

impl Drop for ProcessScope<'_> {
    fn drop(&mut self) {
        self.0.process_thread.store(0, Ordering::Release);
    }
}

/// Implements `vst::Host`. Owns the producer end of the queues the
/// plugin writes into.
pub(crate) struct HostState {
    param_tx: Arc<ArrayQueue<ParameterChange>>,
    midi_out_tx: Arc<ArrayQueue<WireMidi>>,
    /// Parameter changes refused because [`param_tx`](Self::param_tx) was
    /// full. Monotonic for the instance's life.
    ///
    /// A counter, not a flag: the question a caller asks is "did I lose
    /// automation, and how much", and a boolean answers only the first. Read
    /// through [`Vst2Loaded::dropped_param_changes`](crate::Vst2Loaded::dropped_param_changes).
    dropped_param_changes: Tally<u64>,
    /// The audio-side end of [`dropped_param_changes`](Self::dropped_param_changes), minted
    /// with the state.
    dropped_param_changes_writer: TallyWriter<u64>,
    /// MIDI events refused because [`midi_out_tx`](Self::midi_out_tx) was
    /// full. Monotonic for the instance's life.
    ///
    /// Worth counting separately from the parameter drops because the
    /// consequence differs in kind: a dropped note-off whose note-on landed is
    /// a stuck note, which is audible, whereas a dropped automation point is
    /// merely a stale readout.
    dropped_midi_out: Tally<u64>,
    /// The audio-side end of [`dropped_midi_out`](Self::dropped_midi_out), minted with the
    /// state. The MIDI inbox's own overflow adds to the same tally.
    dropped_midi_out_writer: TallyWriter<u64>,
    /// Callbacks the host did not answer (`Host::refused_callback`): an opcode it does not
    /// implement, or a malformed pointer.
    refused_callbacks: Tally<u64>,
    refused_callbacks_writer: TallyWriter<u64>,
    time_info: TransportReader,
    /// The value last written to each parameter, sized when the plugin's
    /// parameter count is known ([`size_written`](Self::size_written)). A report
    /// equal to the `f32` narrowing of it is the plugin repeating the write, not
    /// an edit ([`automate`](Host::automate)); any other report clears it.
    written: std::sync::OnceLock<WrittenValues>,
    /// The maximum block size the plugin was last told (`effSetBlockSize`), in
    /// samples. Served back through `audioMasterGetBlockSize`.
    ///
    /// Written by [`set_block_size`](Self::set_block_size) *before* the
    /// opcode is dispatched, so a plugin that asks from inside
    /// `effSetBlockSize` or the `effMainsChanged(1)` after it reads the new
    /// figure. Atomic: the plugin asks from whichever thread it likes.
    block_size: AtomicU64,
    /// The rate the plugin was last told (`effSetSampleRate`), as `f64` bits.
    /// Answers `audioMasterGetSampleRate`, and is written before the opcode is
    /// dispatched for the same reason as [`block_size`](Self::block_size).
    sample_rate: AtomicSampleRate,
    /// Whether the host is rendering offline, answered back through
    /// `audioMasterGetCurrentProcessLevel`.
    ///
    /// Atomic rather than a plain `bool` because the plugin asks from its own
    /// thread while the control thread sets it, and this struct is shared
    /// behind an `Arc` with no lock by design: every field it exposes is
    /// already lock-free.
    offline: std::sync::atomic::AtomicBool,
    /// The thread inside the plugin's process call ([`thread_token`]), or 0 outside one.
    /// Answers `audioMasterGetCurrentProcessLevel` by caller: written by the process
    /// thread's [`ProcessScope`], read from whichever thread the plugin calls from.
    process_thread: AtomicUsize,
    /// The host's automation mode ([`AutomationMode`] as two bits: [`AUTOMATION_READ`],
    /// [`AUTOMATION_WRITE`]), answered through `audioMasterGetAutomationState` and
    /// served in the time info's flags. Written by the control thread, read from
    /// whichever thread the plugin calls from and by the process thread's snapshot.
    automation: AtomicU8,
    /// What the plugin asked of the host through `audioMasterIOChanged` and
    /// `audioMasterUpdateDisplay`: bits for the main thread to take, because
    /// the plugin may call from its audio thread, where a re-read dispatching
    /// opcodes would be wrong. A request is a level, so ten changes between
    /// two passes are one re-read.
    requests: AtomicRequestFlags,
    /// The size the plugin last asked the editor window to take
    /// (`audioMasterSizeWindow`): width in the high 32 bits, height in the low, `0` for none.
    /// The latest request wins, as a request is a level; the main thread takes it with
    /// [`take_size_request`](Self::take_size_request), because the host window is not
    /// reachable from the plugin's thread.
    size_request: AtomicU64,
    /// What `audioMasterCurrentId` answers: the `uniqueID` of the sub-plugin of a shell to
    /// create, `0` when the load names none (and for a plugin that is no shell, which
    /// never asks).
    current_id: i32,
    /// The thread the host was made on, under a `MainThreadMarker`: the one a call from
    /// outside a process scope answers `User` for.
    main_thread: std::thread::ThreadId,
}

impl HostState {
    /// Queues one report of a parameter edit, unless it is an echo of the
    /// host's own write ([`host_write`]); a full queue drops it and counts it.
    fn report(&self, change: ParameterChange) {
        if IN_HOST_WRITE.with(std::cell::Cell::get) {
            return;
        }
        if self.param_tx.push(change).is_err() {
            self.dropped_param_changes_writer.add(1);
        }
    }

    /// Sizes the record of written values for `count` parameters. Called once,
    /// when the plugin's parameter count is read; a second call changes nothing.
    pub(crate) fn size_written(&self, count: usize) {
        let _ = self.written.set(WrittenValues::new(count));
    }

    /// The word `setParameter` takes for `value`: VST2 carries a parameter as
    /// `f32`, so this is the one place a value narrows.
    pub(crate) fn narrow(value: Normalized) -> f32 {
        value.get() as f32
    }

    /// A host write of `value` to parameter `index`: runs `set` (the plugin's
    /// `setParameter`) with its `f32` narrowing, marked as a host write
    /// ([`host_write`]), and records the `f64` written when `set` took it.
    /// Returns what `set` returned.
    pub(crate) fn write(
        &self,
        index: i32,
        value: Normalized,
        set: impl FnOnce(f32) -> bool,
    ) -> bool {
        let taken = host_write(|| set(Self::narrow(value)));
        if taken && let (Some(written), Ok(slot)) = (self.written.get(), usize::try_from(index)) {
            written.record(slot, value);
        }
        taken
    }

    /// Forgets every written value: the plugin set its own state (a chunk, a
    /// program), so what it reports next is not the host's write repeated.
    pub(crate) fn clear_written(&self) {
        if let Some(written) = self.written.get() {
            written.clear();
        }
    }

    /// Whether `report` for parameter `index` is the plugin repeating the
    /// narrowing of the value the host last wrote there. A report that is not
    /// clears the record ([`WrittenValues::absorbs`]).
    fn absorbs_report(&self, index: i32, report: f32) -> bool {
        let slot = usize::try_from(index).ok();
        self.written.get().zip(slot).is_some_and(|(written, slot)| {
            written.absorbs(slot, |value| Self::narrow(value) == report)
        })
    }

    pub(crate) fn new(
        _main: MainThreadMarker,
        param_tx: Arc<ArrayQueue<ParameterChange>>,
        midi_out_tx: Arc<ArrayQueue<WireMidi>>,
        time_info: TransportReader,
        block_size: usize,
        default_sample_rate: SampleRate,
        current_id: i32,
    ) -> Self {
        let dropped_param_changes = tally();
        let dropped_midi_out = tally();
        let refused_callbacks = tally();
        Self {
            param_tx,
            midi_out_tx,
            dropped_param_changes_writer: dropped_param_changes.writer(),
            dropped_param_changes,
            dropped_midi_out_writer: dropped_midi_out.writer(),
            dropped_midi_out,
            refused_callbacks_writer: refused_callbacks.writer(),
            refused_callbacks,
            time_info,
            written: std::sync::OnceLock::new(),
            block_size: AtomicU64::new(block_size as u64),
            sample_rate: AtomicSampleRate::new(default_sample_rate),
            offline: std::sync::atomic::AtomicBool::new(false),
            process_thread: AtomicUsize::new(0),
            automation: AtomicU8::new(0),
            requests: AtomicRequestFlags::new(),
            size_request: AtomicU64::new(0),
            current_id,
            main_thread: std::thread::current().id(),
        }
    }

    /// The editor size the plugin asked for since the last call, if any: what the shared
    /// layer's `poll_editor_resize_request` hands the host.
    pub(crate) fn take_size_request(&self) -> Option<dpi::Size> {
        let packed = self.size_request.swap(0, Ordering::AcqRel);
        (packed != 0).then(|| crate::editor::rect_size((packed >> 32) as u32, packed as u32))
    }

    /// Records the block size the plugin is about to be told, for
    /// `audioMasterGetBlockSize`.
    pub(crate) fn set_block_size(&self, block_size: usize) {
        self.block_size.store(block_size as u64, Ordering::Release);
    }

    /// Records the rate the plugin is about to be told, for
    /// `audioMasterGetSampleRate`.
    pub(crate) fn set_sample_rate(&self, rate: SampleRate) {
        self.sample_rate.store(rate, Ordering::Release);
    }

    /// The requests the plugin raised, for the main thread to take.
    pub(crate) fn requests(&self) -> &AtomicRequestFlags {
        &self.requests
    }

    /// Sets the render mode reported through
    /// `audioMasterGetCurrentProcessLevel`.
    ///
    /// `&self` because the plugin holds this behind an `Arc` — the mode moves
    /// without rebuilding the link.
    pub(crate) fn set_offline(&self, offline: bool) {
        self.offline
            .store(offline, std::sync::atomic::Ordering::Release);
    }

    /// Marks the calling thread as inside the plugin's process call until the returned
    /// scope drops. Real-time safe: one atomic store each way.
    pub(crate) fn enter_process(&self) -> ProcessScope<'_> {
        self.process_thread.store(thread_token(), Ordering::Release);
        ProcessScope(self)
    }

    /// Records the automation mode the plugin is told from now on.
    pub(crate) fn set_automation(&self, mode: AutomationMode) {
        let bits = match mode {
            AutomationMode::Off => 0,
            AutomationMode::Reading => AUTOMATION_READ,
            AutomationMode::Writing => AUTOMATION_WRITE,
            AutomationMode::ReadWriting => AUTOMATION_READ | AUTOMATION_WRITE,
        };
        self.automation.store(bits, Ordering::Release);
    }

    /// The automation mode last recorded: `Off` until one is.
    pub(crate) fn automation(&self) -> AutomationMode {
        match self.automation.load(Ordering::Acquire) {
            AUTOMATION_READ => AutomationMode::Reading,
            AUTOMATION_WRITE => AutomationMode::Writing,
            bits if bits == AUTOMATION_READ | AUTOMATION_WRITE => AutomationMode::ReadWriting,
            _ => AutomationMode::Off,
        }
    }

    pub(crate) fn is_offline(&self) -> bool {
        self.offline.load(std::sync::atomic::Ordering::Acquire)
    }

    /// How many `audioMasterAutomate` reports have been dropped because the
    /// queue was full, since load.
    pub(crate) fn dropped_param_changes(&self) -> &Tally<u64> {
        &self.dropped_param_changes
    }

    /// Queues a SysEx message the plugin emitted as UMP SysEx7 packets (any
    /// length), copying its bytes out of the plugin's buffer, which is valid
    /// only for the callback. The packets go in whole or not at all: one that
    /// does not fit the queue is dropped and counted once in
    /// [`dropped_midi_out`](Self::dropped_midi_out), because a `START`
    /// without its `END` would corrupt what follows.
    ///
    /// That holds for the one producer VST2 allows: `audioMasterProcessEvents`
    /// is an audio-thread callback, so nothing else fills the queue between
    /// the room check and the pushes. The queue cannot reserve several slots
    /// at once or take a push back, so a second producer racing this check
    /// could leave a `START` without its `END`.
    pub(crate) fn push_sysex_out(&self, delta_frames: i32, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // A payload byte of 128 or more is a malformed message: dropped, counted.
        let Some(packets) = sysex_packets(MidiGroup::FIRST, bytes) else {
            self.dropped_midi_out_writer.add(1);
            return;
        };
        // The queue's only other end pops, so room seen now can only grow.
        let room = self.midi_out_tx.capacity() - self.midi_out_tx.len();
        if packets.len() > room {
            self.dropped_midi_out_writer.add(1);
            return;
        }
        let frame = delta_frames.max(0) as u32;
        for ump in packets {
            let _ = self.midi_out_tx.push(WireMidi { offset: frame, ump });
        }
    }

    /// How many plugin-emitted MIDI events have been dropped because the queue
    /// was full, since load.
    pub(crate) fn dropped_midi_out(&self) -> &Tally<u64> {
        &self.dropped_midi_out
    }

    /// How many plugin callbacks the host did not answer, since load.
    pub(crate) fn refused_callbacks(&self) -> &Tally<u64> {
        &self.refused_callbacks
    }
}

thread_local! {
    /// Set while this thread is inside the host's own `setParameter` call
    /// ([`host_write`]). `const`-initialized: reading it on the audio thread
    /// neither allocates nor registers a destructor.
    static IN_HOST_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `write` (the host's `setParameter` dispatch) marked as a host write:
/// an `audioMasterAutomate` the plugin calls from inside it, on this thread,
/// is the plugin echoing the value the host just set, not an edit of its
/// own, and [`HostState::automate`] does not report it.
///
/// Without this an echoing plugin turns every host write into a reported
/// edit: a host that takes reported edits as the user's (a modulation lane's
/// knob) adopts the modulated value as the new base, and the
/// next write adds the offsets again, ratcheting the base to the clamp.
/// An edit made on another thread (the editor) while a write runs is still
/// reported: the mark is this thread's.
fn host_write<R>(write: impl FnOnce() -> R) -> R {
    /// Restores the mark even if `write` unwinds.
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            IN_HOST_WRITE.with(|w| w.set(self.0));
        }
    }
    let _restore = Restore(IN_HOST_WRITE.with(|w| w.replace(true)));
    write()
}

impl Host for HostState {
    /// Called from inside the plugin's `processReplacing` — the audio thread.
    ///
    /// `push` returns the value back when the queue is full; dropping it there
    /// is the policy, and the counter is what keeps that visible. `Relaxed` is
    /// enough: nothing is published *through* the counter, and a reader only
    /// wants the tally eventually.
    ///
    /// A report made from inside the host's own `setParameter` is an echo of
    /// the host's value and is not queued ([`host_write`]).
    fn automate(&self, index: i32, value: f32) {
        if self.absorbs_report(index, value) {
            return;
        }
        // The callback's word is an `f32`; it widens here, once, into a quantity.
        self.report(ParameterChange::Automate(
            index,
            Normalized::clamped(f64::from(value)),
        ));
    }

    /// `audioMasterBeginEdit` (aeffectx.h): the plugin took hold of a
    /// parameter. Queued with the automation it brackets, so the order the
    /// plugin made them in is the order the host reads.
    fn begin_edit(&self, index: i32) {
        self.report(ParameterChange::Begin(index));
    }

    /// `audioMasterEndEdit`: the plugin let go of a parameter. See
    /// [`begin_edit`](Self::begin_edit).
    fn end_edit(&self, index: i32) {
        self.report(ParameterChange::End(index));
    }

    /// Called from inside the plugin's `processReplacing` — the audio thread.
    /// See [`automate`](Self::automate) for the drop policy.
    fn process_plugin_events(&self, events: vst::PluginEvents<'_>) {
        use vst::PluginEvent;

        for event in events.iter() {
            match event {
                PluginEvent::Midi(midi_event) => {
                    if let Some(converted) = to_midi(&midi_event)
                        && self.midi_out_tx.push(converted).is_err()
                    {
                        self.dropped_midi_out_writer.add(1);
                    }
                }
                PluginEvent::SysEx(sysex) => {
                    self.push_sysex_out(sysex.delta_frames(), sysex.payload());
                }
            }
        }
    }

    fn refused_callback(&self, _opcode: i32) {
        self.refused_callbacks_writer.add(1);
    }

    /// `audioMasterCurrentId`: the sub-plugin a shell is to create, which it asks during
    /// `VSTPluginMain` (`aeffect.h`: "current unique identifier on shell plug-in").
    fn get_plugin_id(&self) -> i32 {
        self.current_id
    }

    fn idle(&self) {}

    /// `audioMasterUpdateDisplay` — the plugin changed what the host is
    /// showing, usually by switching preset or program from its own editor.
    ///
    /// VST 2.4 gives the plugin no way to say *what* changed, so the only
    /// correct response is to re-read: the parameter list, names and displayed
    /// values the host cached are all potentially stale. A request rather than a
    /// re-read here: this arrives on whatever thread the plugin's editor runs
    /// on, and a synchronous re-read would dispatch opcodes from it.
    fn update_display(&self) {
        let _ = Request::RescanParams(RescanScope::Info).raise(&self.requests);
        let _ = Request::RescanParams(RescanScope::Values).raise(&self.requests);
    }

    /// Called re-entrantly from inside the plugin's `process`, on the audio
    /// thread, so the read must be wait-free. Returning by value is enough:
    /// `host_dispatch` copies the `TimeInfo` into a thread-local `Cell` and
    /// hands the plugin a pointer to that copy, retaining nothing from here.
    fn get_time_info(&self, _mask: i32) -> Option<vst::TimeInfo> {
        self.time_info.read()
    }

    /// Maximum render block size, in samples: the figure a plugin polling
    /// `audioMasterGetBlockSize` sizes its buffers from.
    fn get_block_size(&self) -> isize {
        self.block_size.load(Ordering::Acquire) as isize
    }

    // ── audioMaster query callbacks, answerable via the tutti-vst2-ffi host half ──
    //
    // `tutti-vst2-ffi`'s `host_dispatch` routes each to a trait method, so
    // each callback returns a real answer instead of 0. Two carry live data
    // (sample rate, process level); the rest are honest neutral defaults
    // pending engine plumbing (documented per method below).

    /// The rate the plugin was last told. Not the block's `TimeInfo` rate: that
    /// is the rate the graph renders at, and the two differ between a rate
    /// change and the moment the plugin hears of it (the in-process node parks
    /// the new rate for the main thread). A plugin asking from inside
    /// `effSetSampleRate` must read the figure being set.
    fn get_sample_rate(&self) -> f32 {
        // `audioMasterGetSampleRate` answers a C `float`.
        self.sample_rate.load(Ordering::Acquire).get() as f32
    }

    /// The level of the thread asking (`audioMasterGetCurrentProcessLevel`): from inside the
    /// plugin's process call, `Offline` while the host renders offline and `Realtime`
    /// otherwise; from the thread the host was made on under a `MainThreadMarker`, `User`; from any other thread,
    /// `Unknown`. `Prefetch` is never answered: the host runs no sequencing thread. The
    /// caller is told what its own thread is, never a guess.
    fn get_process_level(&self) -> ProcessLevel {
        let me = thread_token();
        if self.process_thread.load(Ordering::Acquire) == me {
            if self.is_offline() {
                ProcessLevel::Offline
            } else {
                ProcessLevel::Realtime
            }
        } else if std::thread::current().id() == self.main_thread {
            ProcessLevel::User
        } else {
            ProcessLevel::Unknown
        }
    }

    /// `audioMasterSizeWindow`: `[index]` the new width, `[value]` the new height
    /// (`aeffectx.h`, "1 if supported"). The request is kept for the main thread, which owns
    /// the host window (`poll_editor_resize_request`); answering 1 says the host will resize
    /// it. A size that is not positive is refused (0).
    fn size_window(&self, index: i32, value: isize) -> bool {
        let (Ok(width), Ok(height)) = (u32::try_from(index), u32::try_from(value)) else {
            return false;
        };
        if width == 0 || height == 0 {
            return false;
        }
        self.size_request.store(
            u64::from(width) << 32 | u64::from(height),
            Ordering::Release,
        );
        true
    }

    /// Neutral default: real input latency needs the engine's latency
    /// value threaded into `HostState`. PDC / `LatencyGraph` is engine-side
    /// and not wired here, so report 0 until that plumbing exists.
    fn get_input_latency(&self) -> isize {
        0
    }

    /// Neutral default: see [`get_input_latency`](Self::get_input_latency).
    fn get_output_latency(&self) -> isize {
        0
    }

    /// `audioMasterIOChanged`: the plugin changed its pin counts or latency.
    /// VST2 has no separate latency or tail callback and plugins call this for
    /// all three, so it raises all three for the main thread to re-read, and is
    /// answered `true` (1, "handled") because the host does act on it: a plugin
    /// that reads 0 as "host ignores this" may skip the change.
    fn io_changed(&self) -> bool {
        let _ = Request::IoChanged.raise(&self.requests);
        let _ = Request::LatencyChanged.raise(&self.requests);
        let _ = Request::TailChanged.raise(&self.requests);
        true
    }

    /// `audioMasterCanDo`: what this host really does, so a plugin that gates a
    /// feature on the answer is not told "don't know" about everything.
    ///
    /// `Yes`: the host sends MIDI events to the plugin (`sendVstEvents`,
    /// `sendVstMidiEvent`), takes the MIDI it emits (`receiveVstEvents`,
    /// `receiveVstMidiEvent`), and serves `audioMasterGetTime`
    /// (`sendVstTimeInfo`).
    ///
    /// `Yes` also for `sizeWindow` (the host honours
    /// [`size_window`](Self::size_window) through the editor's resize slot).
    ///
    /// `Yes` for `supportShell` and `shellCategory`: the host walks a shell's
    /// sub-plugins (`effShellGetNextPlugin`) and answers `audioMasterCurrentId`.
    ///
    /// `Yes` for `acceptIOChanges`: `audioMasterIOChanged` is answered 1 and restarts the
    /// plugin on the pins it reports now; for `startStopProcess`: an activation sends
    /// `effStartProcess` and a rejoin or reset `effStopProcess`; and for
    /// `receiveVstSysexEvent`: a `kVstSysExType` event the plugin emits reaches the host
    /// whole.
    ///
    /// `No`, because the host does not do it: `supplyIdle`
    /// (`audioMasterIdle` is not routed to anything). Everything else is
    /// `Maybe`, VST 2.4's "don't know": among them `sendVstMidiEventFlagIsRealtime`. The host
    /// marks the events of a realtime block `kVstMidiEventIsRealtime` and not those of an
    /// offline render, but the flag means an event is played live and not in playback, and
    /// the engine's events carry no such distinction.
    ///
    /// The strings and the reference set are JUCE's host `canDo` answers
    /// (`juce_VSTPluginFormat.cpp`), an open re-implementation of the
    /// protocol, trimmed to what tutti does.
    fn can_do(&self, can_do: &str) -> Supported {
        match can_do {
            "sendVstEvents"
            | "sendVstMidiEvent"
            | "sendVstTimeInfo"
            | "receiveVstEvents"
            | "receiveVstMidiEvent"
            | "sizeWindow"
            | "supportShell"
            | "shellCategory"
            | "acceptIOChanges"
            | "startStopProcess"
            | "receiveVstSysexEvent" => Supported::Yes,
            "supplyIdle" | "openFileSelector" | "closeFileSelector" => Supported::No,
            _ => Supported::Maybe,
        }
    }

    /// `audioMasterGetAutomationState`: the mode the session last set through
    /// `Control::set_automation_state` (`Off` until one is), as
    /// `kVstAutomationOff`, `Read`, `Write` or `ReadWrite`.
    fn get_automation_state(&self) -> AutomationState {
        match self.automation() {
            AutomationMode::Off => AutomationState::Off,
            AutomationMode::Reading => AutomationState::Read,
            AutomationMode::Writing => AutomationState::Write,
            AutomationMode::ReadWriting => AutomationState::ReadWrite,
        }
    }
}

#[cfg(test)]
mod tests;
