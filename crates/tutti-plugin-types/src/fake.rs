//! [`FakePlugin`]: a plugin that implements [`Loaded`], [`Processor`] and
//! [`Controller`] without a plugin behind them.
//!
//! It records every call it receives in a preallocated lock-free log, so a test
//! asserts the sequence of calls rather than a render. It refuses a chosen
//! rate, can be told to lose itself on a refusal, to panic in `activate`, to
//! refuse a deactivation or a rebuild, and to fail a block. Both halves write
//! the log, so [`FakeProcessor`] takes no lock and may run inside
//! `assert_no_alloc`.
//!
//! [`FakePlugin`] is the test's handle on the shared state: it outlives the
//! halves, changes the figures they report, and raises plugin-initiated
//! requests on the word the shell would take.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use tutti_rt::LockExt;

use dpi::{PhysicalSize, Size};
use tutti_plugin_ipc_ffi::FdId;
use tutti_rt::AtomicRequestFlags;

use crate::PluginClass;
use crate::StateError;
use crate::{
    ChannelLayout, ContentScale, Control, Controller, EditReader, EditWriter, EditorError,
    EditorWindow, Layout, Loaded, LoadedPlugin, MainThreadMarker, ParamEdit, ParamFlags,
    ParameterInfo, PluginBus, PluginDescriptor, PluginError, PluginIo, PluginResult, PluginTail,
    Prepare, Processor, Rebuilt, Refused, RenderMode, RenderModeSupport, Request, RescanScope,
    RestartRules, SampleRate, Samples, Stuck, TimerId, Widths, WindowHandle, edit_slots,
};
use tutti_buffers::{Float, FloatWidth};
use tutti_units::Normalized;
use tutti_units::ParamId;

/// One call the fake received.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Call {
    /// `Loaded::activate`, at this rate and block size.
    Activate(SampleRate, Samples),
    /// `Loaded::deactivate` that rejoined.
    Deactivate,
    /// `Loaded::rebuild`.
    Rebuild,
    /// `Processor::process`, with this many frames.
    Process(usize),
    /// `Processor::stop`.
    Stop,
    /// `Processor::reset`.
    Reset,
    /// An edit the Processor applied at the head of a block.
    Edit(ParamId, Normalized),
    /// `Controller::latency`.
    Latency,
    /// `Controller::tail`.
    Tail,
    /// `Control::wiring`.
    Wiring,
    /// `Control::set_parameter` on the Controller or the loaded plugin.
    SetParameter(ParamId),
    /// `Control::save_state`.
    SaveState,
    /// `Control::load_state`, with this many bytes.
    LoadState(usize),
    /// `Control::rescan_parameters`.
    RescanParameters(RescanScope),
    /// `Control::rebuild_midi_mapping`.
    RebuildMidiMapping,
    /// `Control::run_main_thread`.
    RunMainThread,
    /// `Control::run_timer`.
    RunTimer(TimerId),
    /// `Control::service_fd`.
    ServiceFd(FdId),
    /// `EditorWindow::open_editor`.
    OpenEditor,
    /// `EditorWindow::close_editor`.
    CloseEditor,
    /// `EditorWindow::editor_idle`.
    EditorIdle,
    /// `EditorWindow::set_editor_scale`, with the factor's bits.
    SetEditorScale(u64),
    /// `Control::inform_controller`: the plugin's edit controller was told of a
    /// host write.
    InformController(ParamId, Normalized),
    /// A parameter value a block's context carried: the last point of a queue.
    BlockParam(ParamId, Normalized),
    /// `Processor::flush_parameters`, with this many parameters' changes.
    FlushParameters(usize),
}

const SCOPES: [RescanScope; 4] = [
    RescanScope::Values,
    RescanScope::Text,
    RescanScope::Info,
    RescanScope::All,
];

impl Call {
    /// The slot's three words: a kind (never zero) and two arguments.
    fn encode(self) -> [u64; 3] {
        match self {
            Self::Activate(rate, block) => [1, rate.get().to_bits(), block.get() as u64],
            Self::Deactivate => [2, 0, 0],
            Self::Rebuild => [3, 0, 0],
            Self::Process(frames) => [4, frames as u64, 0],
            Self::Stop => [5, 0, 0],
            Self::Edit(id, value) => [6, u64::from(id.get()), value.get().to_bits()],
            Self::Latency => [7, 0, 0],
            Self::Tail => [8, 0, 0],
            Self::Wiring => [9, 0, 0],
            Self::SetParameter(id) => [10, u64::from(id.get()), 0],
            Self::SaveState => [11, 0, 0],
            Self::LoadState(len) => [12, len as u64, 0],
            Self::RescanParameters(scope) => {
                let at = SCOPES.iter().position(|&s| s == scope).unwrap_or(0);
                [13, at as u64, 0]
            }
            Self::RunMainThread => [14, 0, 0],
            Self::RunTimer(id) => [15, u64::from(id.0), 0],
            Self::ServiceFd(id) => [16, u64::from(id.get() as u32), 0],
            Self::RebuildMidiMapping => [17, 0, 0],
            Self::Reset => [18, 0, 0],
            Self::OpenEditor => [19, 0, 0],
            Self::CloseEditor => [20, 0, 0],
            Self::EditorIdle => [21, 0, 0],
            Self::SetEditorScale(bits) => [22, bits, 0],
            Self::InformController(id, value) => [23, u64::from(id.get()), value.get().to_bits()],
            Self::BlockParam(id, value) => [24, u64::from(id.get()), value.get().to_bits()],
            Self::FlushParameters(count) => [25, count as u64, 0],
        }
    }

    fn decode([kind, a, b]: [u64; 3]) -> Option<Self> {
        Some(match kind {
            1 => Self::Activate(
                SampleRate::new(f64::from_bits(a))?,
                Samples::new(b as usize),
            ),
            2 => Self::Deactivate,
            3 => Self::Rebuild,
            4 => Self::Process(a as usize),
            5 => Self::Stop,
            6 => Self::Edit(
                ParamId::new(a as u32),
                Normalized::clamped(f64::from_bits(b)),
            ),
            7 => Self::Latency,
            8 => Self::Tail,
            9 => Self::Wiring,
            10 => Self::SetParameter(ParamId::new(a as u32)),
            11 => Self::SaveState,
            12 => Self::LoadState(a as usize),
            13 => Self::RescanParameters(SCOPES[a as usize]),
            14 => Self::RunMainThread,
            15 => Self::RunTimer(TimerId(a as u32)),
            16 => Self::ServiceFd(
                FdId::new((a as u32 & i32::MAX as u32) as i32).expect("masked non-negative"),
            ),
            17 => Self::RebuildMidiMapping,
            18 => Self::Reset,
            19 => Self::OpenEditor,
            20 => Self::CloseEditor,
            21 => Self::EditorIdle,
            22 => Self::SetEditorScale(a),
            24 => Self::BlockParam(
                ParamId::new(a as u32),
                Normalized::clamped(f64::from_bits(b)),
            ),
            25 => Self::FlushParameters(a as usize),
            23 => Self::InformController(
                ParamId::new(a as u32),
                Normalized::clamped(f64::from_bits(b)),
            ),
            _ => return None,
        })
    }
}

/// How many calls the log holds.
const LOG_CAPACITY: usize = 4096;

/// A preallocated, lock-free record of calls. Any thread appends; read it once
/// the threads are quiet.
#[derive(Debug)]
struct CallLog {
    next: AtomicUsize,
    slots: Box<[[AtomicU64; 3]]>,
}

impl CallLog {
    fn new() -> Self {
        Self {
            next: AtomicUsize::new(0),
            slots: (0..LOG_CAPACITY)
                .map(|_| [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)])
                .collect(),
        }
    }

    fn push(&self, call: Call) {
        let at = self.next.fetch_add(1, Ordering::Relaxed);
        let Some(slot) = self.slots.get(at) else {
            return;
        };
        let [kind, a, b] = call.encode();
        slot[1].store(a, Ordering::Relaxed);
        slot[2].store(b, Ordering::Relaxed);
        slot[0].store(kind, Ordering::Release);
    }

    fn snapshot(&self) -> Vec<Call> {
        let used = self.next.load(Ordering::Relaxed);
        assert!(used <= LOG_CAPACITY, "the call log overflowed");
        self.slots[..used]
            .iter()
            .filter_map(|slot| {
                let kind = slot[0].load(Ordering::Acquire);
                Call::decode([
                    kind,
                    slot[1].load(Ordering::Relaxed),
                    slot[2].load(Ordering::Relaxed),
                ])
            })
            .collect()
    }

    fn clear(&self) {
        let used = self.next.swap(0, Ordering::Relaxed).min(LOG_CAPACITY);
        for slot in &self.slots[..used] {
            slot[0].store(0, Ordering::Relaxed);
        }
    }
}

#[derive(Debug)]
struct Shared {
    log: CallLog,
    refuse_rate: AtomicU64,
    lose_on_refuse: AtomicBool,
    panic_on_activate: AtomicBool,
    stuck: AtomicU32,
    rebuild_fails: AtomicBool,
    panic_in_deactivate: AtomicBool,
    panic_in_rebuild: AtomicBool,
    panic_in_latency: AtomicBool,
    process_fails: AtomicBool,
    latency: AtomicUsize,
    tail: AtomicUsize,
    outputs: AtomicU32,
    requests: AtomicRequestFlags,
    state: Mutex<Vec<u8>>,
    /// Frames the fake sounds after each trigger, or `0` for a silent fake
    /// (`FakePlugin::ring_out`).
    ring: AtomicUsize,
    /// Whether the fake says it is asleep over sound (`FakePlugin::sleep_over_sound`).
    sleeps_over_sound: AtomicBool,
    /// The plugin processes 32-bit audio only ([`FakePlugin::only_32_bit`]).
    only_32_bit: AtomicBool,
    /// The width of the last activation: 0 before one, else 1 (32-bit) or 2.
    activated: AtomicU8,
    /// Blocks processed at 32-bit, and at 64-bit.
    blocks_32: AtomicU64,
    blocks_64: AtomicU64,
    /// Whether the fake has an editor (`FakePlugin::with_editor`).
    editor: AtomicBool,
    /// Whether the editor takes a new scale live (`FakePlugin::with_editor_scale`).
    takes_scale: AtomicBool,
    /// The size the plugin asked its window to take, packed `width << 32 | height`,
    /// until the host polls it; `0` when it has not asked.
    resize_request: AtomicU64,
    /// The events per block the fake's input storage takes
    /// (`FakePlugin::hold_events`); `0` for the node's intake room.
    event_input: AtomicU32,
    /// How the fake takes a render mode (`FakePlugin::take_render_mode`):
    /// `0` at activation, `1` live, `2` not at all.
    render_mode_support: AtomicU8,
    /// The mode the fake was last told live: `0` none, `1` realtime, `2`
    /// offline.
    told_mode: AtomicU8,
    /// The edits the fake reports the host did not hear
    /// (`FakePlugin::drop_reported_edits`).
    dropped_edits: AtomicU64,
    /// The parameters the fake declares and whether each is read-only. None
    /// until [`FakePlugin::declare`]: the fake refuses a write to an id it does
    /// not declare, as every format does.
    declared: Mutex<Vec<(ParamId, bool)>>,
}

impl Shared {
    /// The declared parameters, if the fake declares any.
    fn declared(&self) -> Vec<(ParamId, bool)> {
        self.declared.lock_recover().clone()
    }

    fn wiring(&self) -> LoadedPlugin {
        self.log.push(Call::Wiring);
        let outputs = ChannelLayout::try_from(self.outputs.load(Ordering::Relaxed))
            .expect("a fake plugin has few outputs");
        LoadedPlugin {
            inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
            outputs: vec![PluginBus::main(outputs)],
            latency_samples: Samples::new(self.latency.load(Ordering::Relaxed)),
            tail: self.tail(),
            ..LoadedPlugin::default()
        }
    }

    fn tail(&self) -> PluginTail {
        match self.tail.load(Ordering::Relaxed) {
            0 => PluginTail::None,
            n => PluginTail::Finite(Samples::new(n)),
        }
    }

    fn descriptor() -> PluginDescriptor {
        PluginDescriptor::new("Fake", PluginClass::Unknown)
    }
}

/// The test's handle on a fake plugin: it outlives the halves, and everything
/// it sets is seen by them.
#[derive(Debug, Clone)]
pub struct FakePlugin(Arc<Shared>);

impl FakePlugin {
    /// A loaded fake and the handle on it. Reports no latency or tail and a
    /// stereo output.
    #[must_use]
    pub fn new() -> (Self, FakeLoaded) {
        let shared = Arc::new(Shared {
            log: CallLog::new(),
            refuse_rate: AtomicU64::new(0),
            lose_on_refuse: AtomicBool::new(false),
            panic_on_activate: AtomicBool::new(false),
            stuck: AtomicU32::new(0),
            rebuild_fails: AtomicBool::new(false),
            panic_in_deactivate: AtomicBool::new(false),
            panic_in_rebuild: AtomicBool::new(false),
            panic_in_latency: AtomicBool::new(false),
            process_fails: AtomicBool::new(false),
            latency: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            outputs: AtomicU32::new(2),
            requests: AtomicRequestFlags::new(),
            state: Mutex::new(Vec::new()),
            ring: AtomicUsize::new(0),
            sleeps_over_sound: AtomicBool::new(false),
            only_32_bit: AtomicBool::new(false),
            activated: AtomicU8::new(0),
            blocks_32: AtomicU64::new(0),
            blocks_64: AtomicU64::new(0),
            editor: AtomicBool::new(false),
            takes_scale: AtomicBool::new(false),
            resize_request: AtomicU64::new(0),
            event_input: AtomicU32::new(0),
            render_mode_support: AtomicU8::new(0),
            told_mode: AtomicU8::new(0),
            dropped_edits: AtomicU64::new(0),
            declared: Mutex::new(Vec::new()),
        });
        (
            Self(shared.clone()),
            FakeLoaded {
                shared,
                descriptor: Shared::descriptor(),
                values: Vec::new(),
            },
        )
    }

    /// Every call so far, in the order they were made. Read it once the threads
    /// are quiet.
    ///
    /// # Panics
    ///
    /// If more than 4096 calls were made.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.0.log.snapshot()
    }

    /// Forgets the calls so far.
    pub fn clear_calls(&self) {
        self.0.log.clear();
    }

    /// Makes `activate` refuse this rate. `None` takes every rate again.
    pub fn refuse_rate(&self, rate: Option<SampleRate>) {
        let bits = rate.map_or(0, |r| r.get().to_bits());
        self.0.refuse_rate.store(bits, Ordering::Relaxed);
    }

    /// Makes a refused activation hand no plugin back, as AU can.
    pub fn lose_plugin_on_refusal(&self, lose: bool) {
        self.0.lose_on_refuse.store(lose, Ordering::Relaxed);
    }

    /// Makes `activate` panic.
    pub fn panic_in_activate(&self, panic: bool) {
        self.0.panic_on_activate.store(panic, Ordering::Relaxed);
    }

    /// Makes the next `n` deactivations refuse, handing both halves back.
    pub fn stick_deactivations(&self, n: u32) {
        self.0.stuck.store(n, Ordering::Relaxed);
    }

    /// Makes `rebuild` refuse.
    pub fn fail_rebuild(&self, fail: bool) {
        self.0.rebuild_fails.store(fail, Ordering::Relaxed);
    }

    /// Makes `deactivate` panic.
    pub fn panic_in_deactivate(&self, panic: bool) {
        self.0.panic_in_deactivate.store(panic, Ordering::Relaxed);
    }

    /// Makes `rebuild` panic.
    pub fn panic_in_rebuild(&self, panic: bool) {
        self.0.panic_in_rebuild.store(panic, Ordering::Relaxed);
    }

    /// Makes every block fail (`Processor::process` returns an error) while
    /// set: the block's parameter changes are then not delivered.
    pub fn fail_blocks(&self, fail: bool) {
        self.0.process_fails.store(fail, Ordering::Relaxed);
    }

    /// Makes `Controller::latency` panic.
    pub fn panic_in_latency(&self, panic: bool) {
        self.0.panic_in_latency.store(panic, Ordering::Relaxed);
    }

    /// The latency the plugin reports from now on.
    pub fn set_latency(&self, latency: Samples) {
        self.0.latency.store(latency.get(), Ordering::Relaxed);
    }

    /// The tail the plugin reports from now on: none for zero.
    pub fn set_tail(&self, tail: Samples) {
        self.0.tail.store(tail.get(), Ordering::Relaxed);
    }

    /// Makes the fake sound for `frames` after each trigger (a non-silent input
    /// frame or a MIDI event), at 0.5 on every output channel from the
    /// trigger's frame on, and say what CLAP's process status would: *sleep*
    /// in a block that gave it nothing and sounded nothing, *continue*
    /// otherwise. `0`, the default, is a silent fake that says nothing.
    pub fn ring_out(&self, frames: usize) {
        self.0.ring.store(frames, Ordering::Relaxed);
    }

    /// Makes the fake process 32-bit audio only, as a plugin that declares no
    /// 64-bit support ([`Loaded::widths`]).
    pub fn only_32_bit(&self) {
        self.0.only_32_bit.store(true, Ordering::Relaxed);
    }

    /// The width the host activated the fake at, as it sent it: `None` before
    /// an activation.
    #[must_use]
    pub fn activated_width(&self) -> Option<FloatWidth> {
        match self.0.activated.load(Ordering::Relaxed) {
            1 => Some(FloatWidth::F32),
            2 => Some(FloatWidth::F64),
            _ => None,
        }
    }

    /// How many blocks the host sent the fake at `width`, as the block's own
    /// sample type said.
    #[must_use]
    pub fn blocks_at(&self, width: FloatWidth) -> u64 {
        match width {
            FloatWidth::F32 => self.0.blocks_32.load(Ordering::Relaxed),
            FloatWidth::F64 => self.0.blocks_64.load(Ordering::Relaxed),
        }
    }

    /// Makes the fake say *sleep* every block, sounding or not: a plugin
    /// that lies about being asleep.
    pub fn sleep_over_sound(&self, lie: bool) {
        self.0.sleeps_over_sound.store(lie, Ordering::Relaxed);
    }

    /// The fake's Processor takes `per_block` events a block on its event
    /// input from now on (`Processor::event_input`); the node's intake room by default.
    pub fn hold_events(&self, per_block: u32) {
        self.0.event_input.store(per_block, Ordering::Relaxed);
    }

    /// How the fake takes a change of render mode from now on. The default is
    /// [`RenderModeSupport::OnActivation`], as VST3 takes it.
    pub fn take_render_mode(&self, support: RenderModeSupport) {
        let code = match support {
            RenderModeSupport::OnActivation => 0,
            RenderModeSupport::Live => 1,
            RenderModeSupport::Unsupported => 2,
        };
        self.0.render_mode_support.store(code, Ordering::Relaxed);
    }

    /// The mode the fake was last told live, through
    /// [`Control::set_render_mode`]; `None` before any.
    #[must_use]
    pub fn told_render_mode(&self) -> Option<RenderMode> {
        match self.0.told_mode.load(Ordering::Relaxed) {
            1 => Some(RenderMode::Realtime),
            2 => Some(RenderMode::Offline),
            _ => None,
        }
    }

    /// The plugin reports `n` more parameter edits the host did not hear
    /// ([`Control::dropped_reported_edits`]).
    pub fn drop_reported_edits(&self, n: u64) {
        self.0.dropped_edits.fetch_add(n, Ordering::Relaxed);
    }

    /// The output channel count the plugin reports from now on.
    pub fn set_outputs(&self, channels: u16) {
        self.0.outputs.store(u32::from(channels), Ordering::Relaxed);
    }

    /// Declares the fake's parameters: each `(id, read_only)`. The fake follows
    /// the write policy of every format: a write to an id it does not declare is
    /// [`Undeclared`](crate::ParamWriteError::Undeclared), and one to a read-only
    /// parameter is [`ReadOnly`](crate::ParamWriteError::ReadOnly). It declares
    /// nothing until this is called.
    pub fn declare(&self, parameters: &[(ParamId, bool)]) {
        *self.0.declared.lock_recover() = parameters.to_vec();
    }

    /// Gives the fake an editor: `open_editor` answers 640 by 480 and the
    /// window calls are logged. The default is none.
    pub fn with_editor(&self, has: bool) {
        self.0.editor.store(has, Ordering::Relaxed);
    }

    /// Makes the fake's editor take a new scale live. The default is the trait's
    /// refusal, `EditorError::ScaleUnsupported`, as VST2 and AU answer.
    pub fn with_editor_scale(&self, takes: bool) {
        self.0.takes_scale.store(takes, Ordering::Relaxed);
    }

    /// The plugin asks its window to take `size`; the host's next
    /// `poll_editor_resize_request` answers it, once. The fake counts in device
    /// pixels, as it answers `open_editor`.
    pub fn request_editor_size(&self, size: PhysicalSize<u32>) {
        let packed = (u64::from(size.width) << 32) | u64::from(size.height);
        self.0.resize_request.store(packed, Ordering::Relaxed);
    }

    /// The word the plugin's host callbacks raise, which the shell takes.
    #[must_use]
    pub fn request_flags(&self) -> &AtomicRequestFlags {
        &self.0.requests
    }
}

/// The fake, loaded.
///
/// `Clone`, so a test can lend it as a `MainThreadControl`: a clone shares the
/// fake's state and call log, and copies its parameter values.
#[derive(Debug, Clone)]
pub struct FakeLoaded {
    shared: Arc<Shared>,
    descriptor: PluginDescriptor,
    values: Vec<(ParamId, Normalized)>,
}

/// The realtime half of an active fake.
#[derive(Debug)]
pub struct FakeProcessor {
    shell: Arc<ActivationShell>,
    shared: Arc<Shared>,
    drain: EditReader,
    /// Frames of the voice still to sound (`FakePlugin::ring_out`).
    sounding: usize,
}

/// The main-thread half of an active fake.
#[derive(Debug)]
pub struct FakeController {
    shell: Arc<ActivationShell>,
    shared: Arc<Shared>,
    descriptor: PluginDescriptor,
    sink: EditWriter,
    values: Vec<(ParamId, Normalized)>,
}

/// What the two halves of one activation share, so a pair from two activations
/// is told apart by identity.
#[derive(Debug)]
struct ActivationShell;

fn set_value(values: &mut Vec<(ParamId, Normalized)>, id: ParamId, value: Normalized) {
    match values.iter_mut().find(|(known, _)| *known == id) {
        Some(slot) => slot.1 = value,
        None => values.push((id, value)),
    }
}

macro_rules! fake_control {
    ($ty:ty, $set:expr, $flush:expr) => {
        impl Control for $ty {
            fn info(&self) -> &PluginDescriptor {
                &self.descriptor
            }

            fn wiring(&self, _main: MainThreadMarker) -> LoadedPlugin {
                self.shared.wiring()
            }

            fn parameters(&self, _main: MainThreadMarker) -> Vec<ParameterInfo> {
                self.shared
                    .declared()
                    .into_iter()
                    .map(|(id, read_only)| {
                        let flags = if read_only {
                            ParamFlags::READ_ONLY
                        } else {
                            ParamFlags::empty()
                        };
                        ParameterInfo::new(id, "p").with_flags(ParamFlags::READ_ONLY, flags)
                    })
                    .collect()
            }

            fn parameter(&self, _main: MainThreadMarker, id: ParamId) -> Option<Normalized> {
                self.values.iter().find(|(k, _)| *k == id).map(|&(_, v)| v)
            }

            fn parameter_plain(
                &self,
                _main: MainThreadMarker,
                _id: ParamId,
                value: Normalized,
            ) -> Option<f64> {
                Some(value.get())
            }

            fn parameter_normalized(
                &self,
                _main: MainThreadMarker,
                _id: ParamId,
                plain: f64,
            ) -> Option<Normalized> {
                Normalized::new(plain)
            }

            fn inform_controller(
                &mut self,
                _main: MainThreadMarker,
                id: ParamId,
                value: Normalized,
            ) {
                self.shared.log.push(Call::InformController(id, value));
            }

            /// Follows the write policy: only a parameter the fake declares
            /// ([`FakePlugin::declare`]) is written.
            fn set_parameter(
                &mut self,
                main: MainThreadMarker,
                id: ParamId,
                value: Normalized,
            ) -> Result<(), $crate::ParamWriteError> {
                self.parameter_writable(main, id)?;
                self.shared.log.push(Call::SetParameter(id));
                set_value(&mut self.values, id, value);
                let set: fn(&mut Self, ParamId, Normalized) = $set;
                set(self, id, value);
                Ok(())
            }

            fn save_state(
                &mut self,
                _main: MainThreadMarker,
                _: $crate::StateUse,
            ) -> PluginResult<Vec<u8>> {
                self.shared.log.push(Call::SaveState);
                Ok(self.shared.state.lock().map_err(|_| poisoned())?.clone())
            }

            fn load_state(
                &mut self,
                _main: MainThreadMarker,
                bytes: &[u8],
                _: $crate::StateUse,
            ) -> PluginResult<()> {
                self.shared.log.push(Call::LoadState(bytes.len()));
                *self.shared.state.lock().map_err(|_| poisoned())? = bytes.to_vec();
                Ok(())
            }

            fn flush_edits(&mut self, main: MainThreadMarker) {
                let flush: fn(&mut Self, MainThreadMarker) = $flush;
                flush(self, main);
            }

            fn rebuild_midi_mapping(&mut self, _main: MainThreadMarker) {
                self.shared.log.push(Call::RebuildMidiMapping);
            }

            fn rescan_parameters(&mut self, _main: MainThreadMarker, scope: RescanScope) {
                self.shared.log.push(Call::RescanParameters(scope));
            }

            fn run_main_thread(&mut self, _main: MainThreadMarker) {
                self.shared.log.push(Call::RunMainThread);
            }

            fn run_timer(&mut self, _main: MainThreadMarker, id: TimerId) {
                self.shared.log.push(Call::RunTimer(id));
            }

            fn service_fd(&mut self, _main: MainThreadMarker, id: FdId) {
                self.shared.log.push(Call::ServiceFd(id));
            }

            /// What a test raised on [`FakePlugin::request_flags`], taken once.
            fn take_requests(&mut self, _main: MainThreadMarker, out: &mut Vec<Request>) {
                out.extend(crate::taken(&self.shared.requests));
            }

            /// As [`FakePlugin::take_render_mode`] set it; at activation by
            /// default, as VST3 takes it.
            fn render_mode_support(&self, _main: MainThreadMarker) -> RenderModeSupport {
                match self.shared.render_mode_support.load(Ordering::Relaxed) {
                    1 => RenderModeSupport::Live,
                    2 => RenderModeSupport::Unsupported,
                    _ => RenderModeSupport::OnActivation,
                }
            }

            /// Takes the mode where the fake is [`RenderModeSupport::Live`],
            /// recording it for [`FakePlugin::told_render_mode`].
            fn set_render_mode(&mut self, main: MainThreadMarker, mode: RenderMode) -> bool {
                if self.render_mode_support(main) != RenderModeSupport::Live {
                    return false;
                }
                let code = if mode.is_offline() { 2 } else { 1 };
                self.shared.told_mode.store(code, Ordering::Relaxed);
                true
            }

            /// The fake reports no edits outside a block.
            fn take_reported_edits(&mut self, _main: MainThreadMarker, _out: &mut Vec<ParamEdit>) {}

            /// As [`FakePlugin::drop_reported_edits`] counted them: none until a
            /// test says so (the fake reports edits in `ProcessOutput`, like
            /// CLAP and VST3).
            fn dropped_reported_edits(&self) -> u64 {
                self.shared.dropped_edits.load(Ordering::Relaxed)
            }
        }

        /// The fake has an editor only when the test gave it one.
        impl EditorWindow for $ty {
            fn has_editor(&self, _main: MainThreadMarker) -> bool {
                self.shared.editor.load(Ordering::Relaxed)
            }

            fn open_editor(
                &mut self,
                _main: MainThreadMarker,
                _parent: WindowHandle,
                _scale: ContentScale,
            ) -> PluginResult<Size> {
                if !self.shared.editor.load(Ordering::Relaxed) {
                    return Err(PluginError::Editor(EditorError::PluginError(
                        "the fake plugin has no editor".into(),
                    )));
                }
                self.shared.log.push(Call::OpenEditor);
                Ok(PhysicalSize::new(640, 480).into())
            }

            fn set_editor_scale(
                &mut self,
                _main: MainThreadMarker,
                scale: ContentScale,
            ) -> PluginResult<()> {
                if !self.shared.takes_scale.load(Ordering::Relaxed) {
                    return Err(PluginError::Editor(EditorError::ScaleUnsupported));
                }
                self.shared
                    .log
                    .push(Call::SetEditorScale(scale.get().to_bits()));
                Ok(())
            }

            fn close_editor(&mut self, _main: MainThreadMarker) {
                self.shared.log.push(Call::CloseEditor);
            }

            fn editor_idle(&mut self, _main: MainThreadMarker) {
                self.shared.log.push(Call::EditorIdle);
            }

            /// The default capabilities: this surface reports no resize support.
            fn editor_capabilities(
                &mut self,
                _main: MainThreadMarker,
            ) -> crate::EditorCapabilities {
                crate::EditorCapabilities::default()
            }

            /// Not supported here.
            fn set_editor_size(
                &mut self,
                _main: MainThreadMarker,
                _requested: dpi::Size,
            ) -> crate::PluginResult<dpi::Size> {
                Err(crate::PluginError::Editor(crate::EditorError::PluginError(
                    "set_editor_size not supported".into(),
                )))
            }

            /// What the test asked the plugin to request, once.
            fn poll_editor_resize_request(&mut self, _main: MainThreadMarker) -> Option<dpi::Size> {
                match self.shared.resize_request.swap(0, Ordering::Relaxed) {
                    0 => None,
                    packed => {
                        Some(dpi::PhysicalSize::new((packed >> 32) as u32, packed as u32).into())
                    }
                }
            }
        }
    };
}

fn poisoned() -> PluginError {
    PluginError::StateFailed(StateError::PluginCrashed)
}

fake_control!(FakeLoaded, |_, _, _| {}, |_, _| {});
fake_control!(
    FakeController,
    |this, id, value| {
        // The edit reaches the Processor at the head of its next block. The
        // write policy refused an undeclared id before this call.
        let _ = this.sink.write(id, value);
    },
    |_, _| {}
);

impl Loaded for FakeLoaded {
    type Processor = FakeProcessor;
    type Controller = FakeController;

    const RESTARTS: RestartRules = RestartRules::CLAP;

    fn widths(&self) -> Widths {
        Widths::new(!self.shared.only_32_bit.load(Ordering::Relaxed))
    }

    fn activate(
        self,
        _main: MainThreadMarker,
        prepare: Prepare,
    ) -> Result<(FakeProcessor, FakeController), Refused<Self>> {
        let shared = self.shared.clone();
        shared.activated.store(
            match prepare.format() {
                FloatWidth::F32 => 1,
                FloatWidth::F64 => 2,
            },
            Ordering::Relaxed,
        );
        shared.log.push(Call::Activate(
            prepare.rate(),
            prepare.max_block().samples(),
        ));
        assert!(
            !shared.panic_on_activate.load(Ordering::Relaxed),
            "the fake was told to panic in activate"
        );
        let refused = f64::from_bits(shared.refuse_rate.load(Ordering::Relaxed));
        if refused != 0.0 && refused == prepare.rate().get() {
            let reason = PluginError::Other(format!("the fake refuses {refused} Hz"));
            let plugin = (!shared.lose_on_refuse.load(Ordering::Relaxed)).then_some(self);
            return Err(Refused { plugin, reason });
        }
        let shell = Arc::new(ActivationShell);
        let (sink, drain) = edit_slots(shared.declared().into_iter().map(|(id, _)| id));
        Ok((
            FakeProcessor {
                shell: shell.clone(),
                shared: shared.clone(),
                drain,
                sounding: 0,
            },
            FakeController {
                shell,
                shared,
                descriptor: self.descriptor,
                sink,
                values: self.values,
            },
        ))
    }

    fn deactivate(
        _main: MainThreadMarker,
        processor: FakeProcessor,
        controller: FakeController,
    ) -> Result<Self, Stuck<FakeProcessor, FakeController>> {
        if !Arc::ptr_eq(&processor.shell, &controller.shell) {
            return Err(Stuck {
                processor,
                controller,
                reason: PluginError::Other("the halves are of two activations".into()),
            });
        }
        let shared = controller.shared.clone();
        assert!(
            !shared.panic_in_deactivate.load(Ordering::Relaxed),
            "the fake was told to panic in deactivate"
        );
        if shared
            .stuck
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Stuck {
                processor,
                controller,
                reason: PluginError::Other("the fake will not stop".into()),
            });
        }
        shared.log.push(Call::Deactivate);
        Ok(FakeLoaded {
            shared,
            descriptor: controller.descriptor,
            values: controller.values,
        })
    }

    fn rebuild(self, _main: MainThreadMarker) -> Result<Rebuilt<Self>, Refused<Self>> {
        self.shared.log.push(Call::Rebuild);
        assert!(
            !self.shared.panic_in_rebuild.load(Ordering::Relaxed),
            "the fake was told to panic in rebuild"
        );
        if self.shared.rebuild_fails.load(Ordering::Relaxed) {
            return Err(Refused {
                plugin: Some(self),
                reason: PluginError::Other("the fake cannot rebuild".into()),
            });
        }
        Ok(Rebuilt::restored(self))
    }
}

impl Controller for FakeController {
    type Loaded = FakeLoaded;
    type Processor = FakeProcessor;

    fn latency(&self, _main: MainThreadMarker) -> Samples {
        self.shared.log.push(Call::Latency);
        assert!(
            !self.shared.panic_in_latency.load(Ordering::Relaxed),
            "the fake was told to panic in latency"
        );
        Samples::new(self.shared.latency.load(Ordering::Relaxed))
    }

    fn tail(&self, _main: MainThreadMarker) -> PluginTail {
        self.shared.log.push(Call::Tail);
        self.shared.tail()
    }
}

impl Processor for FakeProcessor {
    fn event_input(&self) -> std::num::NonZeroU32 {
        std::num::NonZeroU32::new(self.shared.event_input.load(Ordering::Relaxed))
            .unwrap_or(crate::MIDI_STACK_BOUND)
    }

    fn process<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
        let log = &self.shared.log;
        match S::WIDTH {
            FloatWidth::F32 => &self.shared.blocks_32,
            FloatWidth::F64 => &self.shared.blocks_64,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.drain
            .drain(|id, value| log.push(Call::Edit(id, value)));
        if self.shared.process_fails.load(Ordering::Relaxed) {
            return Err(PluginError::Block("the fake fails its blocks"));
        }
        if let Some(changes) = io.ctx.param_changes {
            for queue in &changes.queues {
                if let Some(point) = queue.points.last() {
                    log.push(Call::BlockParam(queue.param_id, point.value));
                }
            }
        }
        let ring = self.shared.ring.load(Ordering::Relaxed);
        let frames = io.buffer.frames().get();
        io.buffer.output().silence();
        if ring > 0 {
            // A voice: every trigger (a non-silent input frame, a MIDI
            // event at the frame) starts `ring` frames of 0.5.
            let mut triggered = false;
            let (input, mut output) = io.buffer.split();
            for i in 0..frames {
                let hit = io
                    .ctx
                    .midi_events
                    .iter()
                    .any(|(at, _)| at.get() as usize == i)
                    || input.iter().any(|c| c.samples()[i] != S::ZERO);
                if hit {
                    self.sounding = ring;
                    triggered = true;
                }
                if self.sounding > 0 {
                    for mut out in output.iter_mut() {
                        out.samples_mut()[i] = S::lit(0.5);
                    }
                    self.sounding -= 1;
                    triggered = true;
                }
            }
            io.out.set_status(
                if triggered && !self.shared.sleeps_over_sound.load(Ordering::Relaxed) {
                    crate::BlockStatus::Continue
                } else {
                    crate::BlockStatus::Sleep
                },
            );
        }
        log.push(Call::Process(frames));
        Ok(())
    }

    fn flush_parameters(&mut self, changes: &crate::ParameterChanges) -> PluginResult<()> {
        let log = &self.shared.log;
        self.drain
            .drain(|id, value| log.push(Call::Edit(id, value)));
        log.push(Call::FlushParameters(changes.queues.len()));
        Ok(())
    }

    fn stop(&mut self) {
        self.shared.log.push(Call::Stop);
    }

    fn reset(&mut self) {
        self.shared.log.push(Call::Reset);
    }
}

impl From<&FakeLoaded> for Layout {
    fn from(fake: &FakeLoaded) -> Self {
        Layout::from(&fake.shared.wiring())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake that declares parameters follows the write policy of every
    /// format: an undeclared id and a read-only parameter are refused at the
    /// call and write nothing; a declared one is written.
    #[test]
    fn a_fake_that_declares_parameters_follows_the_write_policy() {
        let (plugin, mut loaded) = FakePlugin::new();
        let (open, meter, gone) = (ParamId::new(1), ParamId::new(2), ParamId::new(3));
        plugin.declare(&[(open, false), (meter, true)]);
        let main = tutti_fixtures::main_thread();
        let half = Normalized::of(0.5);
        assert_eq!(loaded.set_parameter(main, open, half), Ok(()));
        assert_eq!(
            loaded.set_parameter(main, meter, half),
            Err(crate::ParamWriteError::ReadOnly(meter))
        );
        assert_eq!(
            loaded.set_parameter(main, gone, half),
            Err(crate::ParamWriteError::Undeclared(gone))
        );
        assert_eq!(loaded.parameter(main, meter), None);
        assert_eq!(loaded.parameter(main, open), Some(half));
    }
}
