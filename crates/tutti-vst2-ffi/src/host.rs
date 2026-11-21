//! Host specific structures.

use num_traits::Float;
use tracing::{error, warn};
use tutti_rt::{LockExt, Tally, TallyWriter, tally};

use libloading::Library;
use std::cell::{Cell, UnsafeCell};
use std::collections::BTreeSet;
use std::convert::TryFrom;
use std::ffi::CString;
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::ThreadId;
use std::{fmt, ptr, slice};

use tutti_plugin_types::{
    MAX_STATE_BYTES, MainThreadMarker, MainWorkHandle, WindowHandle, c_str_prefix, ffi_guard,
    read_c_str,
};

use crate::{
    api::{self, AEffect, GetChunkError, PluginFlags, PluginMain, Supported, TimeInfo, consts::*},
    buffer::AudioBuffer,
    editor::{KeyCode, Rect},
    interfaces,
    plugin::{self, Category, Info},
    primitive_enum::primitive_enum,
    speaker::{ArrangementRead, SpeakerArrangement},
};

primitive_enum! {
    #[derive(Clone, Copy, Debug)]
    #[doc(hidden)]
    pub enum AudioMasterOpCode: i32 {
        /// [index]: parameter index
        /// [opt]: parameter value
        Automate = 0,
        /// [return]: host vst version (e.g. 2400 for VST 2.4)
        Version,
        /// [return]: current plugin ID (useful for shell plugins to figure out which plugin to load in
        ///           `VSTPluginMain()`).
        CurrentId,
        /// No arguments. Give idle time to Host application, e.g. if plug-in editor is doing mouse
        /// tracking in a modal loop.
        Idle,
        /// Deprecated.
        _PinConnected = 4,

        /// Deprecated.
        _WantMidi = 6, // Not a typo
        /// [value]: request mask. see `VstTimeInfoFlags`
        /// [return]: `VstTimeInfo` pointer or null if not supported.
        GetTime,
        /// Inform host that the plugin has MIDI events ready to be processed. Should be called at the
        /// end of the plugin's process call.
        /// [ptr]: `VstEvents*` the events to be processed.
        /// [return]: 1 if supported and processed OK.
        ProcessEvents,
        /// Deprecated.
        _SetTime,
        /// Deprecated.
        _TempoAt,
        /// Deprecated.
        _GetNumAutomatableParameters,
        /// Deprecated.
        _GetParameterQuantization,

        /// Notifies the host that the input/output setup has changed. This can allow the host to check
        /// numInputs/numOutputs or call `getSpeakerArrangement()`.
        /// [return]: 1 if supported.
        IOChanged,

        /// Deprecated.
        _NeedIdle,

        /// Request the host to resize the plugin window.
        /// [index]: new width.
        /// [value]: new height.
        SizeWindow,
        /// [return]: the current sample rate.
        GetSampleRate,
        /// [return]: the current block size.
        GetBlockSize,
        /// [return]: the input latency in samples.
        GetInputLatency,
        /// [return]: the output latency in samples.
        GetOutputLatency,

        /// Deprecated.
        _GetPreviousPlug,
        /// Deprecated.
        _GetNextPlug,
        /// Deprecated.
        _WillReplaceOrAccumulate,

        /// [return]: the current process level, see `VstProcessLevels`
        GetCurrentProcessLevel,
        /// [return]: the current automation state, see `VstAutomationStates`
        GetAutomationState,

        /// The plugin is ready to begin offline processing.
        /// [index]: number of new audio files.
        /// [value]: number of audio files.
        /// [ptr]: `AudioFile*` the host audio files. Flags can be updated from plugin.
        OfflineStart,
        /// Called by the plugin to read data.
        /// [index]: (bool)
        ///    VST offline processing allows a plugin to overwrite existing files. If this value is
        ///    true then the host will read the original file's samples, but if it is false it will
        ///    read the samples which the plugin has written via `OfflineWrite`
        /// [value]: see `OfflineOption`
        /// [ptr]: `OfflineTask*` describing the task.
        /// [return]: 1 on success
        OfflineRead,
        /// Called by the plugin to write data.
        /// [value]: see `OfflineOption`
        /// [ptr]: `OfflineTask*` describing the task.
        OfflineWrite,
        /// Unknown. Used in offline processing.
        OfflineGetCurrentPass,
        /// Unknown. Used in offline processing.
        OfflineGetCurrentMetaPass,

        /// Deprecated.
        _SetOutputSampleRate,
        /// Deprecated.
        _GetOutputSpeakerArrangement,

        /// Get the vendor string.
        /// [ptr]: `char*` for vendor string, limited to `MAX_VENDOR_STR_LEN`.
        GetVendorString,
        /// Get the product string.
        /// [ptr]: `char*` for vendor string, limited to `MAX_PRODUCT_STR_LEN`.
        GetProductString,
        /// [return]: vendor-specific version
        GetVendorVersion,
        /// Vendor specific handling.
        VendorSpecific,

        /// Deprecated.
        _SetIcon,

        /// Check if the host supports a feature.
        /// [ptr]: `char*` can do string
        /// [return]: 1 if supported
        CanDo,
        /// Get the language of the host.
        /// [return]: `VstHostLanguage`
        GetLanguage,

        /// Deprecated.
        _OpenWindow,
        /// Deprecated.
        _CloseWindow,

        /// Get the current directory.
        /// [return]: `FSSpec` on OS X, `char*` otherwise
        GetDirectory,
        /// Tell the host that the plugin's parameters have changed, refresh the UI.
        ///
        /// No arguments.
        UpdateDisplay,
        /// Tell the host that if needed, it should record automation data for a control.
        ///
        /// Typically called when the plugin editor begins changing a control.
        ///
        /// [index]: index of the control.
        /// [return]: true on success.
        BeginEdit,
        /// A control is no longer being changed.
        ///
        /// Typically called after the plugin editor is done.
        ///
        /// [index]: index of the control.
        /// [return]: true on success.
        EndEdit,
        /// Open the host file selector.
        /// [ptr]: `VstFileSelect*`
        /// [return]: true on success.
        OpenFileSelector,
        /// Close the host file selector.
        /// [ptr]: `VstFileSelect*`
        /// [return]: true on success.
        CloseFileSelector,

        /// Deprecated.
        _EditFile,
        /// Deprecated.
        /// [ptr]: char[2048] or sizeof (FSSpec).
        /// [return]: 1 if supported.
        _GetChunkFile,
        /// Deprecated.
        _GetInputSpeakerArrangement,
    }
}

/// Implemented by all VST hosts.
#[allow(unused_variables)]
pub trait Host {
    /// Automate a parameter; the value has been changed.
    fn automate(&self, index: i32, value: f32) {}

    /// Signal that automation of a parameter started (the knob has been touched / mouse button down).
    fn begin_edit(&self, index: i32) {}

    /// Signal that automation of a parameter ended (the knob is no longer been touched / mouse button up).
    fn end_edit(&self, index: i32) {}

    /// Get the plugin ID of the currently loading plugin.
    ///
    /// This is only useful for shell plugins where this value will change the plugin returned.
    fn get_plugin_id(&self) -> i32 {
        0
    }

    /// An idle call.
    ///
    /// This is useful when the plugin is doing something such as mouse tracking in the UI.
    fn idle(&self) {}

    /// Handle incoming events from the plugin: what the host callback hands the host for
    /// `audioMasterProcessEvents`, as a view only the callback can make.
    fn process_plugin_events(&self, events: api::PluginEvents<'_>) {}

    /// Get time information.
    fn get_time_info(&self, mask: i32) -> Option<TimeInfo> {
        None
    }

    /// Get block size.
    fn get_block_size(&self) -> isize {
        0
    }

    /// Get the current sample rate (`audioMasterGetSampleRate`).
    fn get_sample_rate(&self) -> f32 {
        44_100.0
    }

    /// Honor a plugin-requested editor-window resize (`audioMasterSizeWindow`).
    /// `index` = new width, `value` = new height. Return `true` if the host
    /// resized the window.
    fn size_window(&self, _index: i32, _value: isize) -> bool {
        false
    }

    /// The plugin's input/output setup changed (`audioMasterIOChanged`).
    /// Return `true` if handled.
    fn io_changed(&self) -> bool {
        false
    }

    /// Report the host's input latency in samples (`audioMasterGetInputLatency`).
    fn get_input_latency(&self) -> isize {
        0
    }

    /// Report the host's output latency in samples (`audioMasterGetOutputLatency`).
    fn get_output_latency(&self) -> isize {
        0
    }

    /// Report the current process level (`audioMasterGetCurrentProcessLevel`,
    /// `VstProcessLevels`) of the thread asking. The default is `Unknown`: the host
    /// does not say.
    fn get_process_level(&self) -> api::ProcessLevel {
        api::ProcessLevel::Unknown
    }

    /// Report the host's automation state (`audioMasterGetAutomationState`,
    /// `VstAutomationStates`). The default is `Unsupported`: the host does not say.
    fn get_automation_state(&self) -> api::AutomationState {
        api::AutomationState::Unsupported
    }

    /// Answer the plugin's `audioMasterCanDo` query for the capability named
    /// `can_do` (`"sendVstEvents"`, `"sendVstTimeInfo"`, ...).
    ///
    /// `Supported::Yes` for what the host does, `Supported::No` for what it
    /// declines, `Supported::Maybe` for a name it does not know: VST 2.4's
    /// `1`, `-1` and `0`. The default answers `Maybe` to everything.
    fn can_do(&self, can_do: &str) -> Supported {
        Supported::Maybe
    }

    /// A callback the host did not answer: an opcode it does not implement, or one whose
    /// pointer was null or misaligned. Runs on whichever thread the plugin called from, the
    /// audio thread included, so an implementation counts it and neither logs nor allocates.
    /// The default ignores it.
    fn refused_callback(&self, opcode: i32) {}

    /// Refresh UI after the plugin's parameters changed.
    ///
    /// Note: some hosts query parameters from within the `update_display` call, through the
    /// parameter value, label, name and text opcodes.
    fn update_display(&self) {}
}

/// Why a VST2 plugin did not load, by the stage that failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PluginLoadError {
    /// The operating system could not open the library: it is missing, unreadable, the
    /// wrong architecture, or one of its own dependencies is absent.
    #[error("could not open the library at {}", path.display())]
    OpenLibrary {
        /// The path that was opened.
        path: PathBuf,
        /// The loader's error.
        #[source]
        source: libloading::Error,
    },

    /// The library opened but has no `VSTPluginMain` entry point: it is no VST2.4 plugin.
    #[error("the library at {} has no VSTPluginMain entry point", path.display())]
    MissingEntryPoint {
        /// The library's path.
        path: PathBuf,
        /// The loader's error.
        #[source]
        source: libloading::Error,
    },

    /// The entry point returned no plugin.
    ///
    /// This can happen for many reasons, such as if the plugin requires a different version of
    /// the VST API to be used, or due to improper licensing.
    #[error("the plugin entry point returned no instance")]
    InstanceFailed,

    /// The API version which the plugin used is not supported by this library.
    #[error("the plugin API version {version} is below 2400")]
    InvalidApiVersion {
        /// What `effGetVstVersion` answered.
        version: isize,
    },

    /// The entry point returned an `AEffect` that is not one: its `magic` is not
    /// `VstP`, or it has no dispatcher. Nothing was called on it.
    #[error("the plugin entry point returned an invalid AEffect")]
    InvalidEffect,

    /// The plugin installed neither `processReplacing` nor the deprecated `process`, so it
    /// has nothing to render with. Refused at load, not per block.
    #[error("the plugin installed neither processReplacing nor process")]
    NoProcessEntry,
}

/// Wrapper for an externally loaded VST plugin.
///
/// The only functionality this struct provides is loading plugins, which can be done via the
/// [`load`](#method.load) method.
#[derive(Debug)]
pub struct PluginLoader<T: Host> {
    main: PluginMain,
    lib: Arc<Library>,
    host: Arc<T>,
}

/// What a plugin answered for its MIDI channel counts.
///
/// `None` on a field means the plugin did not answer that opcode — which is
/// the common case, and is **not** the same as answering zero. See
/// [`PluginInstance::read_midi_channels`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiChannelCounts {
    /// `effGetNumMidiInputChannels`, 1..=15 when answered.
    pub inputs: Option<u8>,
    /// `effGetNumMidiOutputChannels`, 1..=15 when answered.
    pub outputs: Option<u8>,
}

/// One sub-plugin of a **shell** plugin: what `effShellGetNextPlugin` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellPlugin {
    /// The sub-plugin's `uniqueID`, which a host answers `audioMasterCurrentId` with to load it.
    pub id: i32,
    /// The name the shell gave it.
    pub name: String,
}

/// A loaded VST plugin, and the main-thread handle on it.
///
/// This is the one owner of the plugin. It sends `effClose` when it drops, and everything
/// that is not a render entry is its method. The other half is the [`RenderHandle`]
/// ([`render_handle`](Self::render_handle)): the audio thread's entries, which share the
/// plugin's core but do not own the plugin, so dropping one never closes it, and an entry
/// called after the close is refused without touching the `AEffect` the plugin freed.
///
/// # Thread rules
///
/// The rule is in the types, as in the other formats (`tutti_plugin_types::MainThreadMarker`):
///
/// - An entry that VST2 puts on the main thread takes a `MainThreadMarker`, the proof that the
///   caller is on it: `effOpen`, `effMainsChanged`, the sample rate and block size, state
///   (`effGetChunk`, `effSetChunk`), programs, bypass, the editor, the MIDI program
///   queries, `effCanDo`, `effGetTailSize`. Nothing is checked at the call.
/// - `getParameter` and `setParameter` are the dispatcher-free entries any thread may call
///   beside `process`: they take no token, no gate and no lock.
/// - **Named exceptions: token-free, beside a render.** `Vst2Loaded`'s parameter reads (and so
///   the in-process node's control surface) ask for a parameter's name, label and display,
///   its properties, whether it can be automated, and parses a string into it
///   (`get_parameter_name`, `get_parameter_label`, `get_parameter_text`,
///   `parameter_properties`, `can_be_automated`, `string_to_parameter`), off the main thread.
///   They are dispatcher calls, and they hold only the **dispatcher lock**, which the audio
///   thread never takes: they are serialised with each other, with the exclusive calls and
///   with the close, and a render that starts meanwhile runs. The 2.4 headers give no
///   exclusion between these opcodes and `process` (authority 3), and a host that serialised
///   them would turn a UI read of a parameter's text into a silent block and lost MIDI;
///   JUCE does not (authority 2). They take no token only because the backend has none;
///   narrowing them to the main thread waits for the session to give it a route there.
/// - The **exclusive** calls (`effSetSampleRate`, `effSetBlockSize`, `effSetProcessPrecision`,
///   `effMainsChanged`) hold the dispatcher lock and the audio gate: they wait for a render in
///   flight and a render that starts meanwhile is refused.
/// - Plain `AEffect` fields (the pin counts, `initialDelay`, `numPrograms`) are read
///   without a token; they are not calls into the plugin.
///
/// The instance is `Send`, so a host can load on one thread and hand it to another; `Send`
/// permits moving it and is not a claim that any thread may call it. It is not `Sync`.
///
/// **A drop has no token**, so the close is guarded another way: the instance records the
/// thread it was made on (the one that held the `MainThreadMarker` it was made with), and drops
/// the plugin properly only there. Dropped anywhere else it hands the close to its owner's
/// [`MainWorkHandle`], to run at the collector's next
/// run on the main thread, with the core alive in it: a plugin that closes on the wrong
/// thread may not survive, and the live [`RenderHandle`] may still be rendering. The host
/// crate that owns the instance drops it on the control side.
///
/// The drop on the right thread is: close the editor if open, `effStopProcess` if
/// processing, `effMainsChanged(0)` if resumed, then `effClose`, in that order and under the
/// dispatcher lock and the audio gate, so a parameter read and an audio entry in flight
/// finish first and later audio entries are refused.
///
/// ```compile_fail,E0061
/// # use tutti_vst2_ffi::PluginInstance;
/// // A main-thread entry needs the proof: there is no token-free `resume`.
/// fn go(instance: &mut PluginInstance) { instance.resume(); }
/// ```
///
/// ```compile_fail,E0277
/// # use tutti_vst2_ffi::PluginInstance;
/// fn needs_sync<T: Sync>() {}
/// needs_sync::<PluginInstance>();
/// ```
#[allow(dead_code)] // To keep `lib` around.
#[derive(Debug)]
pub struct PluginInstance {
    core: Arc<Core>,
    /// The `dlopen` handle, deliberately leaked on drop.
    ///
    /// JUCE-based plugins (and several others) crash inside their static
    /// destructor sequence when the module is unloaded, so hosts do not
    /// `dlclose` them. `ManuallyDrop` is how that is expressed here — note it
    /// covers *only* the unload. `effClose` still runs (see [`Drop`]); the two
    /// are separate events and conflating them is what leaked live plugin
    /// instances and licence seats.
    lib: ManuallyDrop<Arc<Library>>,
    info: Info,
    /// The thread this instance was made on, under a `MainThreadMarker`: the only one it closes on.
    main_thread: ThreadId,
    /// Whether the last `effMainsChanged` dispatched carried `value=1`.
    ///
    /// Tracked because `effMainsChanged` is not documented as idempotent and real
    /// plugins reallocate rate-dependent buffers on every `resume(1)`. The drop reads it so
    /// a plugin that is not resumed is not suspended a second time.
    resumed: bool,
    /// Whether the editor is open (`effEditOpen` answered, no `effEditClose` since).
    editor_open: bool,
    /// `effFlagsCanReplacing` as reported in `AEffect::flags`.
    ///
    /// Mandatory in VST2.4 and set by essentially every plugin, but a plugin
    /// that clears it is telling the host that `processReplacing` is not
    /// installed.
    can_replacing: bool,
    /// `effFlagsCanDoubleReplacing`, mirroring [`Self::can_replacing`] for the
    /// `f64` path.
    can_double_replacing: bool,
    /// `effFlagsHasEditor` in `AEffect::flags` — whether the plugin publishes a
    /// GUI at all.
    has_editor: bool,
    /// `&self` entries with a token are main-thread calls; `!Sync` keeps the handle to the
    /// thread that holds it.
    _not_sync: PhantomData<Cell<()>>,
}

/// An owned `Box<Arc<T>>` with `T` erased, freed on drop.
///
/// `AEffect::reserved1` points into this box, so a plugin must not call the host after
/// `effClose`: the box is freed once the instance drops, and a later callback would read freed
/// memory.
struct HostRef {
    ptr: *mut c_void,
    free: unsafe fn(*mut c_void),
}

impl fmt::Debug for HostRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostRef")
            .field("ptr", &self.ptr)
            .finish_non_exhaustive()
    }
}

// SAFETY: `HostRef::new` only accepts `T: Send + Sync`. Dropping runs `T`'s destructor and
// decrements a shared count, which is sound from any thread for such a `T`.
unsafe impl Send for HostRef {}
// SAFETY: as for `Send`; the pointer is never shared out through `&HostRef`.
unsafe impl Sync for HostRef {}

impl HostRef {
    /// Boxes `host`; the box's address is what `AEffect::reserved1` carries.
    fn new<T: Send + Sync + 'static>(host: Arc<T>) -> HostRef {
        unsafe fn free<T>(ptr: *mut c_void) {
            // SAFETY: `ptr` came from `Box::into_raw` of a `Box<Arc<T>>` in `HostRef::new`.
            drop(unsafe { Box::from_raw(ptr as *mut Arc<T>) });
        }
        HostRef {
            ptr: Box::into_raw(Box::new(host)) as *mut c_void,
            free: free::<T>,
        }
    }
}

impl Drop for HostRef {
    fn drop(&mut self) {
        // SAFETY: `free` is the monomorphization that matches `ptr`, and runs once.
        unsafe { (self.free)(self.ptr) };
    }
}

/// The core of one live plugin instance: its `AEffect`, the host box that
/// `AEffect::reserved1` points at, and the gate between the audio entries and the close.
///
/// The [`PluginInstance`] and the [`RenderHandle`] hold an `Arc` of this. The core owns
/// no plugin: `effClose` is the instance's drop, under [`gate`](Self::gate), which also
/// marks the gate closed. A render handle that outlives the instance therefore holds a
/// core whose `AEffect` pointer must not be followed, and does not: its every entry
/// takes the gate first and finds it closed.
struct Core {
    effect: UnsafeCell<*mut AEffect>,
    /// The `Box<Arc<T>>` that `AEffect::reserved1` points at. A field, so it drops after
    /// the close, the last call that can reach the host.
    _host_ref: Option<HostRef>,
    /// Blocks a render entry refused because the plugin's pin counts grew past the
    /// buffer. Counted on the audio thread through the writer each [`RenderHandle`]
    /// holds, read and reported by the control side (`PluginInstance::pin_refusals`).
    pin_refusals: Tally<u64>,
    /// Audio-thread entries the gate refused: shut by a close or a panic, or held by a close
    /// or an exclusive call. A lost block, event list or parameter write. Counted through
    /// each handle's writer.
    gate_refusals: Tally<u64>,
    /// Calls the plugin has no entry for: a null `processReplacingF64` for an `f64` block,
    /// a render or `process` slot it cleared, a null `setParameter`.
    entry_refusals: Tally<u64>,
    /// Calls that found the plugin's dispatcher cleared: an event list or a process
    /// start or stop that did not reach it.
    dispatcher_cleared: Tally<u64>,
    /// Whether the control side has logged `pin_refusals` going nonzero.
    pin_refusals_logged: AtomicBool,
    /// Whether `effStartProcess` is the plugin's last word. Written under the gate, by the
    /// render handle that starts and stops, and by the instance's drop that stops it when
    /// no one else did; read anywhere.
    processing: AtomicBool,
    /// Held by an audio entry for the whole call (`try_lock`: it never waits), and by the
    /// instance's drop for the close (`lock`: it waits for the call in flight). `true`
    /// inside means the plugin is closed.
    gate: Mutex<bool>,
    /// Serialises the dispatcher calls made off the audio thread against each other and
    /// against the calls that must not meet a render: the parameter reads (name, label,
    /// display, properties, `effCanBeAutomated`, `effString2Parameter`) take only this lock;
    /// the exclusive calls and the close take it and then the [`gate`](Self::gate), in that
    /// order. **The audio thread never takes it**, so a read of a parameter's text cannot
    /// make a block refuse. It is what JUCE's host does with its own
    /// `lock`: dispatches are serialised among themselves and not with `process`.
    dispatcher: Mutex<()>,
    /// The thread the instance was made on: the one the plugin is closed on, and the one a
    /// core the audio side outlives it on is freed on.
    main_thread: ThreadId,
    /// Where a close or a free that finds itself off that thread is handed: the
    /// collector of the instance's owner.
    work: MainWorkHandle,
}

impl fmt::Debug for Core {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core").finish_non_exhaustive()
    }
}

impl Core {
    /// A core over `effect`, which must be a live `AEffect` that the plugin has not
    /// closed. Nothing here closes it; the instance does.
    fn new(
        effect: *mut AEffect,
        host_ref: Option<HostRef>,
        main_thread: ThreadId,
        work: MainWorkHandle,
    ) -> Arc<Self> {
        let core = Arc::new(Core {
            effect: UnsafeCell::new(effect),
            _host_ref: host_ref,
            pin_refusals: tally(),
            gate_refusals: tally(),
            entry_refusals: tally(),
            dispatcher_cleared: tally(),
            pin_refusals_logged: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            gate: Mutex::new(false),
            dispatcher: Mutex::new(()),
            main_thread,
            work,
        });
        // Warm the gate here, on the control side. On macOS (and the other platforms whose
        // `std::sync::Mutex` is a `pthread_mutex_t`) the mutex allocates its 64-byte
        // `pthread_mutex_t` lazily, on the first `lock` or `try_lock`; left alone, that first
        // touch would be the first audio entry's `try_lock`, an allocation on the render path
        // (the no-alloc gate of `a_callback_from_a_render_takes_no_lock_during_a_load` aborts
        // on it). Linux and Windows use a futex or SRW lock that never allocates. One
        // uncontended lock now means no later `lock`/`try_lock` on any thread allocates.
        drop(core.gate.lock_recover());
        core
    }
}

// SAFETY: what the type itself holds is a pointer cell that is written once at
// construction and only read afterwards, atomics, a mutex, and the host box, which is
// `Send + Sync` (`HostRef`). The `AEffect` it points at is the plugin's, live until the
// instance's close, and only followed by a holder of the gate that has seen it open, or
// by the instance itself (the one holder that closes). Moving the core to another thread
// lets a `RenderHandle` there call the audio entries, which is what VST2 expects of the
// audio thread.
unsafe impl Send for Core {}
// SAFETY: as for `Send`; shared access to the cell is reads of an immutable pointer, and the
// rest is atomics and a mutex. `&Core` gives no entry beyond the private dispatch the
// instance and the render handle make under their own rules (the token, the gate).
unsafe impl Sync for Core {}

/// What a close needs of an instance, once the instance itself is gone: the core, and the two
/// flags that say which opcodes the sequence sends. The instance's drop runs it on the spot
/// when it is on the right thread; off it, the sequence is handed to the main thread
/// ([`MainWorkHandle::defer`](tutti_plugin_types::MainWorkHandle::defer)) instead of being skipped.
struct Closer {
    core: Arc<Core>,
    editor_open: bool,
    resumed: bool,
}

impl Closer {
    /// Sends the opcodes of a close, once, on the main thread; see [`PluginInstance`]'s docs.
    fn close(self) {
        use plugin::EffectOpCode as op;

        // The editor first: some plugins read audio state during editor teardown (to flush
        // meter buffers).
        if self.editor_open {
            self.core
                .dispatch(op::EditorClose, 0, 0, ptr::null_mut(), 0.0);
        }
        // Waits for an off-main dispatcher call in flight (a parameter read), and then
        // for an audio entry in flight; after this none starts.
        let _dispatching = self.core.dispatcher.lock_recover();
        let mut closed = self.core.gate.lock_recover();
        // `effStopProcess`, which the SDK puts before the suspend, is the render handle's;
        // when no one stopped it, it is stopped here, once.
        if self.core.processing.swap(false, Ordering::AcqRel) {
            self.core
                .dispatch(op::StopProcess, 0, 0, ptr::null_mut(), 0.0);
        }
        // A plugin already suspended (a rejoin left it so) would see a second
        // `effMainsChanged(0)` with no start before it.
        if self.resumed {
            self.core
                .dispatch(op::StateChanged, 0, 0, ptr::null_mut(), 0.0);
        }
        // `effClose`. The plugin frees its own `AEffect` in response, so this must happen
        // exactly once, and it *must* happen: a licensed plugin releases its session seat
        // here, and skipping it leaks one live instance (and one licence) per A/B of a slot.
        *closed = true;
        self.core.dispatch(op::Shutdown, 0, 0, ptr::null_mut(), 0.0);
        // After `effClose`: the plugin may call the host while closing.
        unadopt(self.core.get_effect());
        // The module is never `dlclose`d: `PluginInstance::lib` is `ManuallyDrop`.
    }
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        let closer = Closer {
            core: Arc::clone(&self.core),
            editor_open: self.editor_open,
            resumed: self.resumed,
        };
        if std::thread::current().id() == self.main_thread {
            closer.close();
            return;
        }
        // A drop has no token. Off the main thread `effClose` and the rest would run on the
        // wrong thread, which a plugin may not survive, and skipping them leaks the plugin and
        // its licence seat. So the sequence goes to the main thread, to run at its next
        // `MainWork::run` (the session's collect tick); the core stays alive in the closure,
        // so a live `RenderHandle` keeps working until then. This is a control thread (never
        // the audio thread, which owns no instance), so a log line is fine.
        warn!(
            "a VST2 plugin instance was dropped off its own thread; its close is handed to the \
             main thread"
        );
        self.core.work.defer_drop(move |_main| closer.close());
    }
}

impl<T: Host + Send + Sync + 'static> PluginLoader<T> {
    /// Load a plugin at the given path with the given host.
    ///
    /// The host is passed as a plain `Arc<T>`, **not** `Arc<Mutex<T>>`.
    /// `callback_wrapper` — the C function pointer the plugin calls — is
    /// invoked from the plugin's *audio* thread for `audioMasterGetTime`
    /// (from inside `processReplacing` in nearly every synth),
    /// `audioMasterAutomate` and `audioMasterProcessEvents`, and from the
    /// *UI* thread for `audioMasterSizeWindow` / `audioMasterUpdateDisplay`.
    /// A `std::sync::Mutex` shared by both is a priority inversion: the audio
    /// thread blocks on the GUI thread and the stream drops out. `Host` takes
    /// `&self` throughout, so the lock bought nothing but the inversion (and a
    /// `PoisonError` unwind across the FFI boundary). Implementors are
    /// responsible for their own interior mutability, which is what a real-time
    /// host wants anyway: `ArcSwap` / atomics / lock-free channels.
    ///
    /// Upon success, this method returns a [`PluginLoader`](.) object which you can use to call
    /// [`instance`](#method.instance) to create a new instance of the plugin.
    ///
    /// # Errors
    ///
    /// [`PluginLoadError::OpenLibrary`] if the library at `path` cannot be
    /// opened, and [`PluginLoadError::MissingEntryPoint`] if it has no `VSTPluginMain`
    /// entry point.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use std::sync::Arc;
    /// # use tutti_vst2_ffi::{Host, PluginLoader};
    /// # let path = Path::new(".");
    /// # struct MyHost;
    /// # impl MyHost { fn new() -> MyHost { MyHost } }
    /// # impl Host for MyHost {
    /// #     fn automate(&self, _: i32, _: f32) {}
    /// #     fn get_plugin_id(&self) -> i32 { 0 }
    /// # }
    /// // ...
    /// let host = Arc::new(MyHost::new());
    ///
    /// let mut plugin = PluginLoader::load(path, host.clone()).unwrap();
    ///
    /// let main = tutti_plugin_types::MainThreadMarker::new().unwrap();
    /// let (_collector, work) = tutti_plugin_types::MainWork::new();
    /// let instance = plugin.instance(main, &work).unwrap();
    /// // ...
    /// ```
    ///
    /// # Linux/Windows
    ///   * This should be a path to the library, typically ending in `.so`/`.dll`.
    ///   * Possible full path: `/home/overdrivenpotato/.vst/u-he/Zebra2.64.so`
    ///   * Possible full path: `C:\Program Files (x86)\VSTPlugins\iZotope Ozone 5.dll`
    ///
    /// # OS X
    ///   * This should point to the mach-o file within the `.vst` bundle.
    ///   * Plugin: `/Library/Audio/Plug-Ins/VST/iZotope Ozone 5.vst`
    ///   * Possible full path:
    ///     `/Library/Audio/Plug-Ins/VST/iZotope Ozone 5.vst/Contents/MacOS/PluginHooksVST`
    pub fn load(path: &Path, host: Arc<T>) -> Result<PluginLoader<T>, PluginLoadError> {
        // Try loading the library at the given path
        //
        // SAFETY: loading a library runs its initialisers, which is inherent to hosting a
        // plugin and trusts the file at `path`. `lib.get` asks for `VSTPluginMain` as the
        // `PluginMain` fn type; that the exported symbol has that signature is the VST2 entry
        // point ABI and cannot be checked. The copied fn pointer stays valid because `lib` is
        // stored beside it in the loader (and `PluginInstance` never unloads it).
        unsafe {
            let lib = Library::new(path).map_err(|source| PluginLoadError::OpenLibrary {
                path: path.to_path_buf(),
                source,
            })?;
            // Search the library for the VSTAPI entry point
            let main = *lib.get::<PluginMain>(b"VSTPluginMain").map_err(|source| {
                PluginLoadError::MissingEntryPoint {
                    path: path.to_path_buf(),
                    source,
                }
            })?;

            Ok(PluginLoader {
                main,
                lib: Arc::new(lib),
                host,
            })
        }
    }

    /// Call the VST entry point and retrieve a (possibly null) pointer.
    ///
    /// While `main` runs, this thread's [`LOAD_HOST`] points at `self.host`, so the plugin's
    /// callbacks made during initialization (on this thread) reach this loader's host.
    fn call_main(&mut self) -> *mut AEffect {
        // Restores the previous value on exit, so a plugin whose `main` loads another plugin on
        // the same thread nests correctly.
        struct Restore(*mut c_void);
        impl Drop for Restore {
            fn drop(&mut self) {
                LOAD_HOST.set(self.0);
            }
        }

        // `&self.host` outlives the call below: `self` is borrowed for all of `call_main`.
        let _restore = Restore(LOAD_HOST.replace(&self.host as *const Arc<T> as *mut c_void));
        (self.main)(callback_wrapper::<T>)
    }

    /// Try to create an instance of this VST plugin.
    ///
    /// If the instance is successfully created, a [`PluginInstance`] is returned, bound to
    /// the thread `main` proves: the one it closes the plugin on.
    ///
    /// # Errors
    ///
    /// [`PluginLoadError::InstanceFailed`] if the plugin's entry point returns
    /// no instance, [`PluginLoadError::InvalidApiVersion`] if the instance
    /// speaks a VST API version this library does not support, and
    /// [`PluginLoadError::NoProcessEntry`] if it installed no render entry.
    pub fn instance(
        &mut self,
        main: MainThreadMarker,
        work: &MainWorkHandle,
    ) -> Result<PluginInstance, PluginLoadError> {
        // A load is in progress from before `main` until this returns, so a callback made
        // by any thread of the plugin in that window is told apart from a steady-state one
        // (`callback_wrapper`).
        let _loading = Loading::begin();
        // Call the plugin main function. This also passes the plugin main function as the closure
        // could not return an error if the symbol wasn't found
        let effect = self.call_main();

        if effect.is_null() {
            return Err(PluginLoadError::InstanceFailed);
        }

        // SAFETY: `effect` is non-null and was just returned by the plugin's `main`, so
        // VST2 has it point at the plugin's `AEffect`; `magic` and `dispatcher` are plain
        // fields read before anything is called through it.
        let (magic, dispatcher) = unsafe { ((*effect).magic, (*effect).dispatcher) };
        if magic != VST_MAGIC || dispatcher.is_none() {
            // Not an `AEffect` the host can drive, so it is not called at all, not even
            // with `effClose`: the pointer is left alone.
            warn!("plugin entry point returned an invalid AEffect (magic {magic:#x})");
            return Err(PluginLoadError::InvalidEffect);
        }

        // The core owns the box, so it is freed after `effClose`; its heap address is stable.
        let host_ref = HostRef::new(Arc::clone(&self.host));
        // Adopted before `reserved1` is written, and un-adopted by the core's drop after
        // `effClose`: while the address is in the registry, `reserved1` is ours.
        adopt(effect);
        // SAFETY: `effect` is non-null and was just returned by the plugin's `main`; nothing
        // else reads `reserved1` until the plugin calls back with this effect pointer, and a
        // callback that does is let through by `adopt` above.
        unsafe { (*effect).reserved1 = host_ref.ptr as isize };

        let instance = PluginInstance::new(
            effect,
            Arc::clone(&self.lib),
            Some(host_ref),
            main,
            work.clone(),
        );

        let api_ver = instance.dispatch(
            plugin::EffectOpCode::GetApiVersion,
            0,
            0,
            ptr::null_mut(),
            0.0,
        );
        if api_ver < 2400 {
            return Err(PluginLoadError::InvalidApiVersion { version: api_ver });
        }
        if !instance.has_process_entry() {
            return Err(PluginLoadError::NoProcessEntry);
        }
        if !instance.can_replacing {
            warn!(
                "plugin '{}' does not support processReplacing; \
                 it renders through the deprecated accumulating process",
                instance.info.name
            );
        }
        Ok(instance)
    }
}

impl PluginInstance {
    /// Whether the plugin installed an entry to render with: the `processReplacing` it
    /// advertises, or the deprecated `process`.
    fn has_process_entry(&self) -> bool {
        // SAFETY: the core's effect is live while the instance is; the slots are read as
        // `Option` fn pointers and not called.
        let (replacing, accumulating) = unsafe {
            let effect = &*self.core.get_effect();
            (effect.processReplacing.is_some(), effect._process.is_some())
        };
        (self.can_replacing && replacing) || accumulating
    }

    /// Read `AEffect::numPrograms` as it stands *now*.
    ///
    /// A **shell** plugin — one bundle exposing many effects — picks which effect it is
    /// during init, and its program count is that effect's, not the shell's, so the count is
    /// read after `effOpen`.
    ///
    /// Same live-field reasoning as
    /// [`read_initial_delay`](Self::read_initial_delay); `numPrograms` is a
    /// plain `i32` on the `AEffect`, not a call into the plugin.
    pub fn read_num_programs(&self) -> i32 {
        // SAFETY: `self.core` owns the `*mut AEffect` for this instance's
        // lifetime; `numPrograms` is a plain `i32` field, so there is no
        // re-entrancy or thread-affinity concern.
        unsafe { (*self.core.get_effect()).numPrograms }
    }

    /// Bracket a preset change with `effBeginSetProgram`(67) /
    /// `effEndSetProgram`(68), running `change` between them.
    ///
    /// A preset switch moves many parameters at once. Unbracketed, each one
    /// reaches the host as an individual `audioMasterAutomate` edit, so a
    /// switch reads as a storm of automation writes rather than one atomic
    /// event — and this host has that callback wired and draining, so the storm
    /// is real rather than hypothetical.
    ///
    /// Both opcodes are optional and their return is not load-bearing: an
    /// unimplemented one falls through the dispatcher and the change still
    /// happens. The bracket is advisory, so there is nothing here to report.
    pub fn with_preset_bracket<R>(
        &mut self,
        _main: MainThreadMarker,
        change: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.opcode(plugin::EffectOpCode::BeginSetPreset);
        let result = change(self);
        self.opcode(plugin::EffectOpCode::EndSetPreset);
        result
    }

    /// Ask the plugin to enter or leave *soft* bypass, via `effSetBypass`(44).
    ///
    /// Returns `false` when the plugin refuses or does not implement the
    /// opcode — the two are indistinguishable, since an unimplemented opcode
    /// falls through the dispatcher returning 0, which is also "no".
    ///
    /// Soft bypass is the plugin's own passthrough, and it is what a host wants
    /// where one exists: the plugin crossfades and flushes its tail instead of
    /// cutting mid-reverb. It is **not** a substitute for the host's own mute,
    /// it is an alternative to it — a refusal means the host must do the
    /// bypass itself, which is why the answer is returned rather than dropped.
    /// A caller that discards it leaves the plugin audibly processing while the
    /// UI shows it bypassed.
    ///
    /// Check [`can_do(CanDo::Bypass)`](Self::can_do) first. A plugin that
    /// does not advertise `"bypass"` may still return non-zero here, and the
    /// advertisement is the documented contract.
    pub fn set_bypass(&mut self, _main: MainThreadMarker, bypass: bool) -> bool {
        self.dispatch(
            plugin::EffectOpCode::SoftBypass,
            0,
            isize::from(bypass),
            ptr::null_mut(),
            0.0,
        ) != 0
    }

    /// Read `AEffect::initialDelay` as it stands *now*.
    ///
    /// Plugins routinely set their latency during `effOpen`, `effSetSampleRate` and
    /// `effMainsChanged` (a linear-phase EQ does not know its filter length until it knows
    /// the sample rate), so a host asks again after the init sequence and after any state
    /// change that lets a plugin re-declare.
    pub fn read_initial_delay(&self) -> i32 {
        // SAFETY: `self.core` owns the `*mut AEffect` for this instance's
        // lifetime; `initialDelay` is a plain `i32` field, not a call into the
        // plugin, so there is no re-entrancy or thread-affinity concern.
        unsafe { (*self.core.get_effect()).initialDelay }
    }

    /// Ask the plugin for its ring-out, via `effGetTailSize`.
    ///
    /// **The raw answer, undecoded**, because VST 2.4 inverts the convention
    /// every other format uses: `0` means "no tail *information*, host
    /// decides", `1` means "no tail at all", and anything larger is a sample
    /// count. Returning the wire value keeps that decision with the caller —
    /// mapping it here would have to pick a vocabulary, and the two candidate
    /// zeros mean opposite things.
    ///
    /// Read live, for the same reason [`read_initial_delay`](Self::read_initial_delay) is: a
    /// plugin may not know its tail until it knows its sample rate.
    pub fn read_tail_size(&self, _main: MainThreadMarker) -> isize {
        self.dispatch(
            plugin::EffectOpCode::GetTailSize,
            0,
            0,
            std::ptr::null_mut(),
            0.0,
        )
    }

    /// Ask the plugin how many MIDI channels it uses, via
    /// `effGetNumMidiInputChannels` / `effGetNumMidiOutputChannels`.
    ///
    /// `None` when the plugin does not implement the opcode. Both opcodes
    /// return their count as the dispatcher's `isize` return value, and an
    /// unimplemented opcode falls through the plugin's dispatcher returning 0 —
    /// so a bare `0` cannot be told apart from "I use no MIDI channels". The
    /// spec bounds a real answer at 1..=15, which is what makes the two
    /// separable at all: anything outside that range is an absent answer, not a
    /// count. A host must therefore treat `None` as "unknown", never as zero.
    ///
    /// Not part of [`Info`]: a plugin may not know its MIDI configuration until it is
    /// initialised, the same trap [`read_initial_delay`](Self::read_initial_delay)
    /// documents for latency.
    pub fn read_midi_channels(&self, _main: MainThreadMarker) -> MidiChannelCounts {
        fn ask(this: &PluginInstance, opcode: plugin::EffectOpCode) -> Option<u8> {
            // VST 2.4 documents the valid answer as 1..=15. Out of that range —
            // including the 0 an unimplemented opcode returns — means the
            // plugin did not answer.
            u8::try_from(this.opcode(opcode))
                .ok()
                .filter(|n| (1..=15).contains(n))
        }

        MidiChannelCounts {
            inputs: ask(self, plugin::EffectOpCode::GetNumMidiInputs),
            outputs: ask(self, plugin::EffectOpCode::GetNumMidiOutputs),
        }
    }

    /// The sub-plugins of a shell (`effGetPlugCategory` is `kPlugCategShell`), by walking
    /// `effShellGetNextPlugin` until it answers 0 (`aeffectx.h`: "[return value]: next
    /// plugin's uniqueID"). The walk is capped at `MAX_SHELL_PLUGINS` and stops at a
    /// repeated id, so a plugin that never answers 0 cannot hold the caller forever. A plugin
    /// that is not a shell answers 0 at once and gives none.
    pub fn shell_plugins(&self, _main: MainThreadMarker) -> Vec<ShellPlugin> {
        let mut found: Vec<ShellPlugin> = Vec::new();
        while found.len() < MAX_SHELL_PLUGINS {
            // As `read_string_param`: the spec's size is a floor, real plugins write more.
            let mut buf = vec![0u8; MIN_STRING_BUFFER];
            let answer = self.dispatch(
                plugin::EffectOpCode::ShellGetNextPlugin,
                0,
                0,
                buf.as_mut_ptr() as *mut c_void,
                0.0,
            );
            let Some(id) = i32::try_from(answer).ok().filter(|id| *id != 0) else {
                break;
            };
            if found.iter().any(|p| p.id == id) {
                break;
            }
            found.push(ShellPlugin {
                id,
                name: read_c_str(&buf),
            });
        }
        found
    }

    /// The audio-thread entries of this plugin, with the pin counts read from the
    /// `AEffect` now: a plugin settles them in `effOpen` and may change them
    /// later, and a block must carry what it has at this moment.
    ///
    /// The handle shares the instance's core and does not own the plugin: dropping it never
    /// closes the plugin, and dropping the instance while the handle lives closes it (on the
    /// instance's thread) and leaves the handle refusing every entry.
    pub fn render_handle(&self, _main: MainThreadMarker) -> RenderHandle {
        let (inputs, outputs) = self.read_pin_counts();
        RenderHandle {
            entry: RenderEntry {
                effect: self.core.get_effect(),
                inputs: inputs.max(0) as usize,
                outputs: outputs.max(0) as usize,
                can_replacing: self.can_replacing,
                can_double_replacing: self.can_double_replacing,
            },
            name: Arc::from(self.info.name.as_str()),
            refusals: Refusals {
                pins: self.core.pin_refusals.writer(),
                gate: self.core.gate_refusals.writer(),
                entries: self.core.entry_refusals.writer(),
                dispatcher: self.core.dispatcher_cleared.writer(),
            },
            core: Arc::clone(&self.core),
        }
    }

    /// Read `AEffect::numInputs` and `numOutputs` as they stand *now*, as
    /// [`read_initial_delay`](Self::read_initial_delay) reads its field.
    pub fn read_pin_counts(&self) -> (i32, i32) {
        // SAFETY: `self.core` owns the `*mut AEffect` for this instance's
        // lifetime; these are plain `i32` fields, not calls into the plugin.
        let effect = unsafe { &*self.core.get_effect() };
        (effect.numInputs, effect.numOutputs)
    }

    /// How many blocks have been rendered as silence because the plugin raised its pin
    /// counts past the buffer it was activated with.
    ///
    /// The plugin renders silence until it is re-activated, which the host does on
    /// `audioMasterIOChanged` (`RestartRules::VST2`); a plugin that grows its pins without
    /// raising it stays silent, and this counter says so. The counter is for the control
    /// side to report once; the audio thread never logs.
    pub fn pin_refusals(&self) -> &Tally<u64> {
        &self.core.pin_refusals
    }

    /// How many audio-thread entries the gate refused: the plugin was closed or dead, or a
    /// close or an exclusive call held the gate.
    pub fn gate_refusals(&self) -> &Tally<u64> {
        &self.core.gate_refusals
    }

    /// How many audio-thread calls found no entry in the plugin: no `processReplacingF64`
    /// for an `f64` block, a render slot it cleared, a null `setParameter`.
    pub fn entry_refusals(&self) -> &Tally<u64> {
        &self.core.entry_refusals
    }

    /// How many audio-thread calls found the plugin's dispatcher cleared.
    pub fn dispatcher_cleared(&self) -> &Tally<u64> {
        &self.core.dispatcher_cleared
    }

    /// The first time [`pin_refusals`](Self::pin_refusals) is nonzero, one warning. For the
    /// control side's tick; never call it from the audio thread.
    pub fn report_pin_refusals(&self) {
        let refused = self.core.pin_refusals.get();
        if refused > 0 && !self.core.pin_refusals_logged.swap(true, Ordering::Relaxed) {
            warn!(
                "plugin '{}' now declares more pins than it was activated with; it renders \
                 silence until it is re-activated ({refused} blocks so far)",
                self.info.name
            );
        }
    }

    /// Read `AEffect::numParams` as it stands *now*.
    pub fn read_num_params(&self) -> i32 {
        // SAFETY: as for `read_pin_counts`.
        unsafe { (*self.core.get_effect()).numParams }
    }

    fn new(
        effect: *mut AEffect,
        lib: Arc<Library>,
        host_ref: Option<HostRef>,
        _main: MainThreadMarker,
        work: MainWorkHandle,
    ) -> PluginInstance {
        use plugin::EffectOpCode as op;

        let main_thread = std::thread::current().id();
        let core = Core::new(effect, host_ref, main_thread, work);
        let mut plug = PluginInstance {
            core,
            lib: ManuallyDrop::new(lib),
            // Replaced below, once the plugin has been asked.
            info: Info {
                name: String::new(),
                vendor: String::new(),
                unique_id: 0,
                version: 0,
                category: Ok(Category::Unknown),
                preset_chunks: false,
                f64_precision: false,
            },
            main_thread,
            resumed: false,
            editor_open: false,
            can_replacing: false,
            can_double_replacing: false,
            has_editor: false,
            _not_sync: PhantomData,
        };

        // SAFETY: `effect` is the non-null `AEffect*` just returned by the plugin's `main`
        // (`PluginLoader::instance` rejects null before calling `new`), and the plugin keeps
        // it live until `effClose`, which only runs when the core (`plug.params`) drops.
        unsafe {
            let effect: &AEffect = &*effect;
            let flags = PluginFlags::from_bits_truncate(effect.flags);

            plug.can_replacing = flags.intersects(PluginFlags::CAN_REPLACING);
            plug.can_double_replacing = flags.intersects(PluginFlags::CAN_DOUBLE_REPLACING);
            plug.has_editor = flags.intersects(PluginFlags::HAS_EDITOR);

            // `dispatch` returns `isize`, but `effGetPlugCategory`'s return is
            // an `i32` in the ABI, so the upper half of a 64-bit answer is not
            // the plugin's. Saturating rather than truncating: a value that does
            // not fit was never a category code, and truncation could land it on
            // a real one.
            let answer = i32::try_from(plug.opcode(op::GetCategory)).unwrap_or(i32::MAX);
            let category = Category::try_from(answer as isize);

            // `effGetEffectName` is the plugin's own name for itself;
            // `effGetProductString` is the product it ships in, which for a
            // bundled suite is one name shared by every plugin in it. Asking
            // only for the product string therefore collapses a whole suite to
            // a single label.
            //
            // Both opcodes are optional and neither reports failure — an
            // unimplemented one falls through the dispatcher without touching
            // the buffer, which stays zero-filled. So an empty string is the
            // only available "did not answer", and the fallback is driven by
            // it. A caller that gets neither is left with an empty name and
            // supplies its own (the file stem); that decision is not this
            // layer's to make.
            let effect_name = plug.read_string(op::GetEffectName, MAX_EFFECT_NAME_LEN);
            let name = if effect_name.is_empty() {
                plug.read_string(op::GetProductName, MAX_PRODUCT_STR_LEN)
            } else {
                effect_name
            };

            plug.info = Info {
                name,
                vendor: plug.read_string(op::GetVendorName, MAX_VENDOR_STR_LEN),
                unique_id: effect.uniqueId,
                version: effect.version,
                category,
                preset_chunks: flags.intersects(PluginFlags::PROGRAM_CHUNKS),
                f64_precision: flags.intersects(PluginFlags::CAN_DOUBLE_REPLACING),
            };
        }

        plug
    }

    /// Query `effGetParameterProperties` for one parameter.
    ///
    /// `None` when the plugin does not implement the opcode — which is the
    /// common case, not an error: a host must treat absence as normal and fall
    /// back to the name/label pair.
    ///
    /// Absence is detected by the *return value*, never by inspecting the
    /// buffer. An unimplemented opcode falls through the plugin's dispatcher
    /// without writing anything, so the buffer still holds the zeros we put
    /// there — indistinguishable from a plugin that really means
    /// "range 0..0, no category".
    pub fn parameter_properties(&self, index: i32) -> Option<api::ParameterProperties> {
        // SAFETY: `ParameterProperties` is `#[repr(C)]` POD — three `f32`s,
        // `i32`/`i16` scalars and byte arrays. Every field's all-zero bit
        // pattern is a valid value of its type (no enums, no references, no
        // `NonZero`), so a zeroed struct is fully initialised before the plugin
        // sees it and remains valid whether or not the plugin writes.
        let mut props: MaybeUninit<api::ParameterProperties> = MaybeUninit::zeroed();

        let supported = self.concurrent(|| {
            self.dispatch(
                plugin::EffectOpCode::GetParamInfo,
                index,
                0,
                props.as_mut_ptr() as *mut c_void,
                0.0,
            )
        });

        // VST 2.4 specifies "1 if supported". Compare against 1 rather than
        // testing `!= 0`: plugins do return negative values from these optional
        // opcodes (a measured plugin answers `-1` to `effGetCurrentMidiProgram`),
        // and `-1` means "no", not "yes".
        if supported != 1 {
            return None;
        }

        // SAFETY: zeroed above, so initialised regardless of what the plugin
        // did; the plugin may only have overwritten bytes in place.
        Some(unsafe { props.assume_init() })
    }

    /// Query `effGetMidiProgramName` for the program at `program_index` on
    /// `channel`.
    ///
    /// Returns the filled struct plus the plugin's return value, which for this
    /// opcode is the *number of programs it services* — not a boolean. `0`
    /// means unsupported, so `None` covers both "no such opcode" and "no
    /// programs".
    ///
    /// The host writes `this_program_index` before dispatch: it is the question,
    /// not part of the answer. Leaving it zero asks about program 0 every time,
    /// which reads as "the plugin reports the same name for every program".
    pub fn midi_program_name(
        &self,
        _main: MainThreadMarker,
        channel: i32,
        program_index: i32,
    ) -> Option<(api::MidiProgramName, i32)> {
        // SAFETY: as `parameter_properties` — `#[repr(C)]` POD whose all-zero
        // bit pattern is valid for every field.
        let mut name: MaybeUninit<api::MidiProgramName> = MaybeUninit::zeroed();

        // Write the query into the struct before handing it over.
        //
        // SAFETY: `name` is zeroed and therefore initialised; the offset comes
        // from `addr_of_mut!` on that same allocation.
        unsafe {
            ptr::addr_of_mut!((*name.as_mut_ptr()).this_program_index).write(program_index);
        }

        let serviced = self.dispatch(
            plugin::EffectOpCode::GetMidiProgramName,
            channel,
            0,
            name.as_mut_ptr() as *mut c_void,
            0.0,
        );

        // "number of used programs, 0 = unsupported". A negative count is a
        // malformed answer, not a small one — `as usize` on it would be
        // catastrophic upstream, so reject it here.
        if serviced <= 0 {
            return None;
        }

        // SAFETY: zeroed above, hence initialised either way.
        Some((unsafe { name.assume_init() }, serviced as i32))
    }

    /// Query `effGetCurrentMidiProgram` — which program `channel` is on now.
    ///
    /// `None` on a negative answer, which is a refusal: all three measured
    /// plugins return `-1` here. Reading the struct on a `-1` return would
    /// report program 0 with an empty name as though it were real.
    ///
    /// **A `0` return is ambiguous and this method cannot resolve it.** The
    /// value is a program *index*, so `0` is both a valid answer and what an
    /// unimplemented opcode returns after falling through the dispatcher. A
    /// caller must disambiguate with an opcode whose zero is unambiguous —
    /// [`Self::midi_program_name`] reports a serviced count. The host wrapper in
    /// `tutti-vst2-host` does exactly that; see its `current_midi_program`.
    pub fn current_midi_program(
        &self,
        _main: MainThreadMarker,
        channel: i32,
    ) -> Option<(api::MidiProgramName, i32)> {
        // SAFETY: as above — `#[repr(C)]` POD, all-zero is valid.
        let mut name: MaybeUninit<api::MidiProgramName> = MaybeUninit::zeroed();

        let current = self.dispatch(
            plugin::EffectOpCode::GetCurrentMidiProgram,
            channel,
            0,
            name.as_mut_ptr() as *mut c_void,
            0.0,
        );

        if current < 0 {
            return None;
        }

        // SAFETY: zeroed above.
        Some((unsafe { name.assume_init() }, current as i32))
    }

    /// Query `effGetMidiProgramCategory` for `category_index` on `channel`.
    ///
    /// Returns the struct plus the plugin's count of used categories; `None`
    /// when unsupported or when the count is not positive.
    pub fn midi_program_category(
        &self,
        _main: MainThreadMarker,
        channel: i32,
        category_index: i32,
    ) -> Option<(api::MidiProgramCategory, i32)> {
        // SAFETY: as above — `#[repr(C)]` POD, all-zero is valid.
        let mut cat: MaybeUninit<api::MidiProgramCategory> = MaybeUninit::zeroed();

        // SAFETY: zeroed and therefore initialised; offset from the same
        // allocation.
        unsafe {
            ptr::addr_of_mut!((*cat.as_mut_ptr()).this_category_index).write(category_index);
        }

        let serviced = self.dispatch(
            plugin::EffectOpCode::GetMidiProgramCategory,
            channel,
            0,
            cat.as_mut_ptr() as *mut c_void,
            0.0,
        );

        if serviced <= 0 {
            return None;
        }

        // SAFETY: zeroed above.
        Some((unsafe { cat.assume_init() }, serviced as i32))
    }

    /// Query `effHasMidiProgramsChanged` — whether the program or key names on
    /// `channel` have changed since the host last read them.
    ///
    /// `true` only on an exact `1`. This is a cache-invalidation signal, so a
    /// garbage non-zero answer must not be read as "changed" forever.
    pub fn midi_programs_changed(&self, _main: MainThreadMarker, channel: i32) -> bool {
        self.dispatch(
            plugin::EffectOpCode::HasMidiProgramsChanged,
            channel,
            0,
            ptr::null_mut(),
            0.0,
        ) == 1
    }

    /// Query `effGetMidiKeyName` — the plugin's name for one MIDI note.
    ///
    /// Drum kits use this to label pads ("Kick", "Snare") instead of note
    /// numbers. `None` when unsupported.
    ///
    /// Both `program_index` and `key_number` are written into the struct before
    /// dispatch: they are the question. VST 2.4 specifies "1 = supported,
    /// 0 = not", so this compares against 1.
    pub fn midi_key_name(
        &self,
        _main: MainThreadMarker,
        channel: i32,
        program_index: i32,
        key_number: i32,
    ) -> Option<api::MidiKeyName> {
        // SAFETY: as above — `#[repr(C)]` POD, all-zero is valid.
        let mut key: MaybeUninit<api::MidiKeyName> = MaybeUninit::zeroed();

        // SAFETY: zeroed and therefore initialised; offsets from the same
        // allocation.
        unsafe {
            ptr::addr_of_mut!((*key.as_mut_ptr()).this_program_index).write(program_index);
            ptr::addr_of_mut!((*key.as_mut_ptr()).this_key_number).write(key_number);
        }

        let supported = self.dispatch(
            plugin::EffectOpCode::GetMidiKeyName,
            channel,
            0,
            key.as_mut_ptr() as *mut c_void,
            0.0,
        );

        if supported != 1 {
            return None;
        }

        // SAFETY: zeroed above.
        Some(unsafe { key.assume_init() })
    }
}

/// Silence every output channel of `buffer`.
///
/// Used both as the "there is nothing safe to call" result and as the required
/// pre-clear for the deprecated accumulating `process` entry point, which adds
/// into the outputs rather than overwriting them.
fn zero_outputs<T: Float>(buffer: &mut AudioBuffer<'_, T>) {
    let (_, mut outputs) = buffer.split();
    for i in 0..outputs.len() {
        for sample in outputs.get_mut(i).iter_mut() {
            *sample = T::zero();
        }
    }
}

/// The least room a string the plugin writes gets, whatever the spec's length is.
const MIN_STRING_BUFFER: usize = 256;

/// The most sub-plugins [`PluginInstance::shell_plugins`] reads from one shell.
const MAX_SHELL_PLUGINS: usize = 4096;

trait Dispatch {
    fn get_effect(&self) -> *mut AEffect;

    /// Send a dispatch message to the plugin.
    fn dispatch(
        &self,
        opcode: plugin::EffectOpCode,
        index: i32,
        value: isize,
        ptr: *mut c_void,
        opt: f32,
    ) -> isize {
        // SAFETY: `get_effect` is the plugin's non-null `AEffect`, live until `effClose` on drop
        // of the core, so reading the `dispatcher` field is in bounds and initialised.
        let Some(dispatcher) = (unsafe { (*self.get_effect()).dispatcher }) else {
            // `PluginLoader::instance` refuses a null dispatcher, so only a plugin that
            // clears the slot later gets here; the opcode is unanswered.
            error!("plugin cleared its dispatcher; opcode {opcode:?} unanswered");
            return 0;
        };
        // SAFETY: `dispatcher` came out of the `AEffect` this instance owns, so
        // the effect pointer it is handed is the one it belongs to and is live
        // for the call. `ptr`/`value` are the caller's to get right — that is
        // the obligation `DispatcherProc` being `unsafe` states.
        unsafe { dispatcher(self.get_effect(), opcode.into(), index, value, ptr, opt) }
    }

    /// Send a lone opcode with no parameters.
    fn opcode(&self, opcode: plugin::EffectOpCode) -> isize {
        self.dispatch(opcode, 0, 0, ptr::null_mut(), 0.0)
    }

    /// Like `dispatch`, except takes a `&str` to send via `ptr`, cut at its first NUL
    /// (a C string cannot hold one) and at `max_len` bytes, a char boundary at or below
    /// it. `max_len` is the room the opcode's buffer has without its NUL: the plugin
    /// `strcpy`s into a fixed buffer for some opcodes (`kVstMaxProgNameLen` for a
    /// program name), and a longer string would run past it.
    fn write_string(
        &self,
        opcode: plugin::EffectOpCode,
        index: i32,
        value: isize,
        string: &str,
        max_len: usize,
        opt: f32,
    ) -> isize {
        let string = CString::new(c_str_prefix(string, max_len.saturating_add(1)))
            .expect("`c_str_prefix` holds no interior NUL");
        self.dispatch(opcode, index, value, string.as_ptr() as *mut c_void, opt)
    }

    fn read_string(&self, opcode: plugin::EffectOpCode, max: usize) -> String {
        self.read_string_param(opcode, 0, 0, 0.0, max)
    }

    fn read_string_param(
        &self,
        opcode: plugin::EffectOpCode,
        index: i32,
        value: isize,
        opt: f32,
        max: usize,
    ) -> String {
        // The spec's `max` is the size a plugin should write within; real plugins write
        // more, and hosts commonly offer 256, so the buffer is never smaller than that.
        let mut buf = vec![0; max.max(MIN_STRING_BUFFER)];
        self.dispatch(opcode, index, value, buf.as_mut_ptr() as *mut c_void, opt);
        read_c_str(&buf)
    }
}

impl Dispatch for PluginInstance {
    fn get_effect(&self) -> *mut AEffect {
        self.core.get_effect()
    }
}

impl Dispatch for Core {
    fn get_effect(&self) -> *mut AEffect {
        // SAFETY: the cell is written once, in `PluginInstance::new`, before it is shared, and
        // never afterwards, so this read cannot race a write.
        unsafe { *self.effect.get() }
    }
}

impl Core {
    /// Send an `effSetChunk` blob (`index == 1` for a preset, `0` for the bank).
    ///
    /// **The plugin's return value is not read.** `aeffect.h` documents `[ptr]`, `[value]` and
    /// `[index]` for `effSetChunk` and no `[return value]` (authority 3), unlike `effGetChunk`
    /// and `effCanDo`, which state theirs; a JUCE plugin returns 0 after loading
    /// successfully (authority 2). The call returning is all the host can know.
    ///
    /// The plugin gets a copy: the opcode's pointer is `void*`, and a plugin that
    /// writes through it (some do while parsing) would write into the caller's memory
    /// if it were handed `data` itself.
    fn set_chunk(&self, index: i32, data: &[u8]) {
        let mut copy = data.to_vec();
        self.dispatch(
            plugin::EffectOpCode::SetData,
            index,
            copy.len() as isize,
            copy.as_mut_ptr() as *mut c_void,
            0.0,
        );
    }

    /// Read an `effGetChunk` blob (`index == 1` for the current preset,
    /// `index == 0` for the whole bank).
    ///
    /// `Ok(vec)` — possibly empty, meaning the plugin genuinely has nothing
    /// saved. `Err` — the plugin *attempted* the save and failed; see
    /// [`GetChunkError`] for why the two must not be conflated.
    ///
    /// Three plugin behaviours must not become UB on the project-save path:
    ///
    /// * `effFlagsProgramChunks` set but no preset loaded yet — the plugin
    ///   returns `0` and never writes the out-pointer, leaving it null.
    ///   `slice::from_raw_parts(null, 0)` is UB even at length zero, because
    ///   the pointer must always be non-null and aligned.
    /// * an error return of `-1`, which `as usize` turned into `usize::MAX`
    ///   and then a `Vec` allocation of 16 exbibytes (or a wild read).
    /// * a positive length with a null pointer, from a plugin that computed a
    ///   size but failed to hand back the buffer.
    /// * a length past [`MAX_STATE_BYTES`], which no state holds.
    fn get_chunk(&self, index: i32) -> Result<Vec<u8>, GetChunkError> {
        // Create a pointer that can be updated from the plugin.
        let mut ptr: *mut u8 = ptr::null_mut();
        let len = self.dispatch(
            plugin::EffectOpCode::GetData,
            index,
            0,
            &mut ptr as *mut *mut u8 as *mut c_void,
            0.0,
        );

        // SAFETY: `copy_chunk` rejects every pointer/length pair that is not a
        // readable buffer before dereferencing. Where it does read, the plugin
        // owns the buffer and the VST2.4 contract keeps it valid until the next
        // dispatch — longer than this copy.
        unsafe { copy_chunk(ptr, len) }
    }
}

/// Copy an `effGetChunk` result, rejecting the pointer/length pairs a plugin is
/// allowed to produce but that are not a readable buffer.
///
/// Split out from [`Core::get_chunk`] so the validation is
/// testable without a loaded plugin.
///
/// `len == 0` is `Ok(empty)` — a normal state for an untouched chunk-capable
/// plugin. The failure shapes (a negative length, a length past
/// [`MAX_STATE_BYTES`], a null buffer) get an `Err`, so a caller cannot read a failed
/// save as "nothing saved".
///
/// # Safety
/// If `len > 0` and `ptr` is non-null, `ptr` must be valid for reads of `len`
/// bytes.
unsafe fn copy_chunk(ptr: *mut u8, len: isize) -> Result<Vec<u8>, GetChunkError> {
    if len < 0 {
        warn!("plugin returned a negative effGetChunk length ({len})");
        return Err(GetChunkError::Failed(len));
    }
    if len == 0 {
        // Genuinely nothing saved — not an error.
        return Ok(Vec::new());
    }
    if len as usize > MAX_STATE_BYTES {
        warn!("plugin reported {len} bytes of chunk data, past the state cap");
        return Err(GetChunkError::TooLarge(len));
    }
    if ptr.is_null() {
        warn!("plugin reported {len} bytes of chunk data but left the pointer null");
        return Err(GetChunkError::NullBuffer(len));
    }
    // SAFETY: `len > 0` and `ptr` is non-null here (both checked above), and the caller's
    // contract (this fn's `# Safety`) is that `ptr` is readable for `len` bytes; `u8` has no
    // alignment or validity requirement.
    Ok(unsafe { slice::from_raw_parts(ptr, len as usize) }.to_vec())
}

/// How a render entry ended, for the handle to count.
enum Block {
    /// The plugin rendered the block.
    Rendered,
    /// The plugin's pin counts grew past the buffer: not called, outputs silent.
    PinsGrew,
    /// The plugin has no entry for the block: not called, outputs silent.
    NoEntry,
}

/// The writers a [`RenderHandle`] counts its refusals through, minted from the core's tallies
/// on the control side. The audio thread adds through them and never drops the last handle
/// on a cell, which the core's tallies keep.
struct Refusals {
    pins: TallyWriter<u64>,
    gate: TallyWriter<u64>,
    entries: TallyWriter<u64>,
    dispatcher: TallyWriter<u64>,
}

/// What a block needs of a plugin: its `AEffect` and the flags and pin counts
/// the render entries check. `Copy`, and carrying no ownership: whoever holds
/// one keeps the plugin's core alive (`RenderHandle`, `PluginInstance`).
#[derive(Clone, Copy)]
struct RenderEntry {
    effect: *mut AEffect,
    inputs: usize,
    outputs: usize,
    can_replacing: bool,
    can_double_replacing: bool,
}

impl RenderEntry {
    fn dispatch(
        &self,
        opcode: plugin::EffectOpCode,
        index: i32,
        value: isize,
        ptr: *mut c_void,
        opt: f32,
    ) -> Option<isize> {
        let _rendering = Rendering::enter(self.effect);
        // SAFETY: `effect` is the plugin's `AEffect`, live for as long as the
        // holder keeps the core (see the type's docs).
        let dispatcher = unsafe { (*self.effect).dispatcher }?;
        // SAFETY: as for `Dispatch::dispatch`: the effect is the dispatcher's
        // own, and `ptr` and `value` are the caller's to get right.
        Some(unsafe { dispatcher(self.effect, opcode.into(), index, value, ptr, opt) })
    }

    /// Whether the plugin's pin counts, read from its `AEffect` now, still fit the
    /// buffer.
    ///
    /// A plugin may raise `numInputs` or `numOutputs` after activation
    /// (`audioMasterIOChanged`) without the host having re-activated it; its render
    /// entries then index the channel tables past the end of the buffer the host sized
    /// for the old counts. `inputs` and `outputs` here are those old counts, and a
    /// plugin that asks for more than the buffer holds is not called.
    fn pins_fit(&self, inputs: usize, outputs: usize) -> bool {
        // SAFETY: `effect` is live while the core is (see the type's docs); the counts are
        // plain `i32` fields, not calls into the plugin.
        let (live_inputs, live_outputs) =
            unsafe { ((*self.effect).numInputs, (*self.effect).numOutputs) };
        let fits = |live: i32, held: usize| usize::try_from(live).unwrap_or(0) <= held;
        fits(live_inputs, inputs) && fits(live_outputs, outputs)
    }

    /// Render one block.
    ///
    /// A plugin whose pin counts have grown past the buffer is not called and the
    /// outputs are silent (see [`pins_fit`](Self::pins_fit)): [`Block::PinsGrew`]. A plugin
    /// with no entry left to call renders silence: [`Block::NoEntry`]. Every other path is
    /// [`Block::Rendered`].
    ///
    /// Prefers `processReplacing`, but only when the plugin both advertises
    /// `effFlagsCanReplacing` *and* actually installed the pointer. VST2.4
    /// makes the replacing path mandatory, so the check looks redundant — it is
    /// not: an unguarded call to a null `processReplacing` is a jump to address
    /// 0 in-process, which takes the whole DAW down, and a plugin can report
    /// api version >= 2400 while leaving the slot null. When replacing is
    /// unavailable we fall back to the deprecated *accumulating* `process` the
    /// same way JUCE does — that entry point adds into the output buffers, so
    /// they must be zeroed first. [`PluginLoader::instance`] refuses a plugin
    /// with neither pointer, so only one that clears them later reaches the
    /// silent branch: the outputs are left silent rather than filled with
    /// whatever the caller's scratch held.
    ///
    /// The presence check is `Option`, not `(p as *const u8).is_null()`. That
    /// idiom silently did nothing: a bare `extern "C" fn` is non-nullable, so the
    /// compiler folded the comparison to `false` under `-O` and the guard shipped
    /// only in debug builds.
    ///
    /// Nothing here logs: this runs every block on the audio thread.
    fn process(&self, buffer: &mut AudioBuffer<f32>) -> Block {
        let _rendering = Rendering::enter(self.effect);
        if buffer.input_count() < self.inputs {
            panic!("Too few inputs in AudioBuffer");
        }
        if buffer.output_count() < self.outputs {
            panic!("Too few outputs in AudioBuffer");
        }
        if !self.pins_fit(buffer.input_count(), buffer.output_count()) {
            zero_outputs(buffer);
            return Block::PinsGrew;
        }
        let effect = self.effect;
        let samples = buffer.samples() as i32;
        // SAFETY: `effect` is live for the holder's lifetime (type docs). The slot is read as
        // an `Option` fn pointer and only called when `Some`. The buffer has at least the
        // plugin's declared pin counts (checked above), each channel pointer addressing
        // `samples` frames (`AudioBuffer` invariant), and the plugin's own ABI is trusted.
        unsafe {
            if let Some(replacing) = (*effect).processReplacing.filter(|_| self.can_replacing) {
                replacing(
                    effect,
                    buffer.raw_inputs().as_ptr(),
                    buffer.raw_outputs().as_mut_ptr(),
                    samples,
                );
                return Block::Rendered;
            }

            let Some(accumulating) = (*effect)._process else {
                zero_outputs(buffer);
                return Block::NoEntry;
            };

            // Accumulating mode *adds* to the outputs, so they must start at 0.
            zero_outputs(buffer);
            accumulating(
                effect,
                buffer.raw_inputs().as_ptr(),
                buffer.raw_outputs().as_mut_ptr(),
                samples,
            );
        }
        Block::Rendered
    }

    /// Render one block in `f64`.
    ///
    /// There is no accumulating counterpart for double precision in VST2.4 —
    /// `processDoubleReplacing` is the only `f64` entry point — so an absent
    /// `effFlagsCanDoubleReplacing` or a null pointer leaves the caller to
    /// re-render through the `f32` path. Silence is returned ([`Block::NoEntry`])
    /// rather than jumping to a null pointer; callers should consult
    /// `get_info().f64_precision` before choosing this path.
    fn process_f64(&self, buffer: &mut AudioBuffer<f64>) -> Block {
        let _rendering = Rendering::enter(self.effect);
        if buffer.input_count() < self.inputs {
            panic!("Too few inputs in AudioBuffer");
        }
        if buffer.output_count() < self.outputs {
            panic!("Too few outputs in AudioBuffer");
        }
        if !self.pins_fit(buffer.input_count(), buffer.output_count()) {
            zero_outputs(buffer);
            return Block::PinsGrew;
        }
        let effect = self.effect;
        // SAFETY: as for `process`: live `effect`, the `Option` slot is called only when `Some`,
        // and the buffer has the declared pin counts with `samples` frames per channel.
        unsafe {
            let replacing = (*effect)
                .processReplacingF64
                .filter(|_| self.can_double_replacing);
            let Some(replacing) = replacing else {
                zero_outputs(buffer);
                return Block::NoEntry;
            };
            replacing(
                effect,
                buffer.raw_inputs().as_ptr(),
                buffer.raw_outputs().as_mut_ptr(),
                buffer.samples() as i32,
            );
        }
        Block::Rendered
    }

    fn process_events(&self, events: &api::Events) -> Option<isize> {
        self.dispatch(
            plugin::EffectOpCode::ProcessEvents,
            0,
            0,
            events as *const _ as *mut _,
            0.0,
        )
    }
}

/// The audio thread's handle on a loaded plugin, held apart from its [`PluginInstance`]: a
/// host that renders on one thread while another runs the plugin's dispatcher keeps the
/// instance on the control side and this on the audio side, so neither holds a Rust
/// reference the other's calls alias.
///
/// It offers the render entries only: `processReplacing`, `processReplacingF64`,
/// `effProcessEvents`, `effStartProcess`, `effStopProcess` and `setParameter`. None of the
/// main thread's opcodes is a method here, so a Processor cannot make one by construction.
///
/// It shares the instance's core and **does not own the plugin**. Dropping it, even as the
/// last holder, does nothing: `effClose` is the [`PluginInstance`]'s drop, on its thread.
/// Every entry takes the core's gate first (`try_lock`: it never waits on the audio thread)
/// and is refused, without touching the `AEffect` the plugin freed, once the instance has
/// closed the plugin: `process` renders silence, the rest do nothing. An entry that finds
/// the gate held (the instance's close is waiting on it, or running) is refused the same
/// way.
///
/// ```compile_fail,E0599
/// # use tutti_vst2_ffi::RenderHandle;
/// // A main-thread opcode is not an audio-thread entry.
/// fn go(render: &mut RenderHandle) { render.resume(); }
/// ```
pub struct RenderHandle {
    entry: RenderEntry,
    name: Arc<str>,
    refusals: Refusals,
    /// What `entry.effect` points into: the gate says whether it is still the plugin's.
    core: Arc<Core>,
}

impl fmt::Debug for RenderHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderHandle")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

// SAFETY: the handle is a pointer to the plugin's `AEffect`, plain flags and an `Arc` of
// the core, which is `Send + Sync`. VST 2.4 lets `process`, `processReplacing`,
// `effProcessEvents`, `effStartProcess` and `effStopProcess` run on the audio thread, so
// moving the handle to one is what the format expects; the holder calls it from one
// thread at a time (every method takes `&mut self`), and the gate keeps every call off a
// closed plugin.
unsafe impl Send for RenderHandle {}

impl Drop for RenderHandle {
    /// If this is the last holder of the core (the instance closed and went first), the core
    /// frees the host box and may free the host state it keeps alive. That must not happen on
    /// the audio thread, which is where a Processor could be dropped. The graph keeps it off
    /// that thread (it retires a plugin node's unit, and so its `Processor`, to the control
    /// side: see `tutti_plugin_types::MainWorkHandle::defer`, "Never from an audio thread", and
    /// `tutti-plugin-host/tests/node/vst2_in_process_retire.rs`), so this branch runs on a control or
    /// pool thread. **It must not run on an audio thread:** it takes a lock and allocates.
    /// Off the instance's thread, the last holder's share goes to the main thread to be freed
    /// there.
    fn drop(&mut self) {
        if Arc::strong_count(&self.core) == 1
            && std::thread::current().id() != self.core.main_thread
        {
            let core = Arc::clone(&self.core);
            core.work.clone().defer_drop(move |_main| drop(core));
        }
    }
}

/// Runs `f` on the entry with the gate held and open, or answers `refused` (and counts it in
/// `refusals`) when the plugin is closed or dead, or the instance's close (or an exclusive
/// call) holds the gate.
///
/// **A panic under the gate kills the core.** The mutex is then poisoned: the plugin may be in
/// any state (the panic came out of an entry mid-call), so the next entry marks the gate
/// closed and refuses, and every later one finds it so: the handle renders silence from then
/// on, and the plugin is closed by the instance's drop as ever. Free (not a method), so the
/// caller can keep borrowing the handle's other fields (the name) across the call.
fn gated<R>(
    core: &Core,
    entry: &RenderEntry,
    refusals: &Refusals,
    refused: R,
    f: impl FnOnce(&RenderEntry, &Core) -> R,
) -> R {
    let gate = match core.gate.try_lock() {
        Ok(gate) => gate,
        Err(TryLockError::Poisoned(poisoned)) => {
            *poisoned.into_inner() = true;
            refusals.gate.add(1);
            return refused;
        }
        Err(TryLockError::WouldBlock) => {
            refusals.gate.add(1);
            return refused;
        }
    };
    if *gate {
        refusals.gate.add(1);
        return refused;
    }
    // The guard is held to the end of this function, so for the whole call; a panic out of `f`
    // unwinds through it and poisons the mutex.
    f(entry, core)
}

impl RenderHandle {
    /// Whether this handle and `instance` are on the same plugin.
    pub fn is_of(&self, instance: &PluginInstance) -> bool {
        Arc::ptr_eq(&self.core, &instance.core)
    }

    /// Render one block in `f32`: `processReplacing`, or the deprecated accumulating
    /// `process` for a plugin that lacks it, or silence when it has neither or the plugin
    /// is closed.
    ///
    /// # Panics
    ///
    /// If the buffer has fewer channels than the plugin had pins when the handle
    /// was made. A plugin that raised its pin counts since is not called: the
    /// outputs are silent.
    pub fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        let rendered = gated(&self.core, &self.entry, &self.refusals, None, |entry, _| {
            Some(entry.process(buffer))
        });
        self.count_block(rendered, buffer);
    }

    /// Render one block in `f64`: `processReplacingF64`, or silence when the plugin lacks it
    /// or is closed.
    ///
    /// # Panics
    ///
    /// As [`process`](Self::process).
    pub fn process_f64(&mut self, buffer: &mut AudioBuffer<f64>) {
        let rendered = gated(&self.core, &self.entry, &self.refusals, None, |entry, _| {
            Some(entry.process_f64(buffer))
        });
        self.count_block(rendered, buffer);
    }

    /// Counts a block that did not reach the plugin and silences its outputs when the gate
    /// refused it (`None`, counted by [`gated`]).
    fn count_block<S: Float>(&self, rendered: Option<Block>, buffer: &mut AudioBuffer<S>) {
        match rendered {
            Some(Block::Rendered) => {}
            Some(Block::PinsGrew) => self.refusals.pins.add(1),
            Some(Block::NoEntry) => self.refusals.entries.add(1),
            None => zero_outputs(buffer),
        }
    }

    /// Delivers MIDI events, `effProcessEvents`.
    pub fn process_events(&mut self, events: &api::Events) {
        gated(&self.core, &self.entry, &self.refusals, (), |entry, _| {
            if entry.process_events(events).is_none() {
                self.refusals.dispatcher.add(1);
            }
        });
    }

    /// `effStartProcess`, if processing is not on. Only legal while the plugin is resumed.
    pub fn start_process(&mut self) {
        gated(
            &self.core,
            &self.entry,
            &self.refusals,
            (),
            |entry, core| {
                if !core.processing.swap(true, Ordering::AcqRel)
                    && entry
                        .dispatch(
                            plugin::EffectOpCode::StartProcess,
                            0,
                            0,
                            ptr::null_mut(),
                            0.0,
                        )
                        .is_none()
                {
                    self.refusals.dispatcher.add(1);
                }
            },
        );
    }

    /// `effStopProcess`, if processing is on. Only legal while the plugin is resumed.
    pub fn stop_process(&mut self) {
        gated(
            &self.core,
            &self.entry,
            &self.refusals,
            (),
            |entry, core| {
                if core.processing.swap(false, Ordering::AcqRel)
                    && entry
                        .dispatch(
                            plugin::EffectOpCode::StopProcess,
                            0,
                            0,
                            ptr::null_mut(),
                            0.0,
                        )
                        .is_none()
                {
                    self.refusals.dispatcher.add(1);
                }
            },
        );
    }

    /// Whether `effStartProcess` is the plugin's last word.
    pub fn is_processing(&self) -> bool {
        self.core.processing.load(Ordering::Acquire)
    }

    /// `setParameter`, which VST 2.4 allows from either thread. `false` when the plugin left
    /// the slot null, or is closed.
    pub fn set_parameter(&mut self, index: i32, value: f32) -> bool {
        gated(
            &self.core,
            &self.entry,
            &self.refusals,
            false,
            |entry, _| {
                let effect = entry.effect;
                // SAFETY: the gate is held open, so the plugin is not closed and `effect` is its
                // live `AEffect`. The slot is read as an `Option` fn pointer and only called when
                // `Some`; `index` is the caller's to bound, as for the instance's
                // `set_parameter`.
                let Some(set) = (unsafe { (*effect).setParameter }) else {
                    self.refusals.entries.add(1);
                    return false;
                };
                // SAFETY: the setter belongs to this effect, which is live (above).
                unsafe { set(effect, index, value) };
                true
            },
        )
    }
}

// SAFETY: the instance is a pointer to the plugin's `AEffect` (through the core), an `Arc` of
// that core (which is `Send + Sync`) and a library handle. `Send` is what lets a host load on
// one thread and own the plugin on another. It permits moving only: the entries that VST2
// puts on the main thread take a `MainThreadMarker`, which only that thread holds, and a drop off
// the thread the instance was made on defers the close to the owner's main-thread collector
// (see the type's docs). It is not `Sync` (`_not_sync`).
unsafe impl Send for PluginInstance {}

/// The entries of the main thread. See the type's docs for which take a token and why.
impl PluginInstance {
    /// What the plugin declared about itself when it was loaded: [`Info`]. The pin, parameter
    /// and program counts and the latency are read live
    /// ([`read_pin_counts`](Self::read_pin_counts), [`read_num_params`](Self::read_num_params),
    /// [`read_num_programs`](Self::read_num_programs),
    /// [`read_initial_delay`](Self::read_initial_delay)).
    pub fn get_info(&self) -> Info {
        self.info.clone()
    }

    /// Dispatch `effOpen`. A plugin settles its pin and parameter counts, program count
    /// and `initialDelay` here, so those are read from the `AEffect` after it
    /// ([`read_pin_counts`](Self::read_pin_counts), [`read_num_params`](Self::read_num_params),
    /// [`read_num_programs`](Self::read_num_programs),
    /// [`read_initial_delay`](Self::read_initial_delay)).
    pub fn init(&mut self, _main: MainThreadMarker) {
        self.opcode(plugin::EffectOpCode::Initialize);
    }

    /// Runs one of the **exclusive** calls: the ones a plugin cannot take while it is
    /// processing (`effSetSampleRate`, `effSetBlockSize`, `effSetProcessPrecision`,
    /// `effMainsChanged`). They hold the audio gate for the call, so they wait for an audio
    /// entry in flight and refuse (to silence) any that starts meanwhile: the plugin
    /// never sees one beside an audio entry, whoever calls it. The host crate also keeps them to
    /// the activation and the rejoin, where no audio handle is live, so the wait is never
    /// taken there; the gate is what makes the serialisation true for any other caller.
    /// They take the dispatcher lock first, so they do not meet a parameter read either.
    ///
    /// **Why these four and not the parameter reads:** a plugin that reallocates its buffers
    /// on a rate, a block size, a precision or a power change cannot be rendered into
    /// meanwhile. That is the convention of the SDK's `suspend`/`resume` bracket; the header
    /// itself states no exclusion for any opcode (authority 3, `aeffect.h` is silent).
    fn exclusive<R>(&self, call: impl FnOnce() -> R) -> R {
        let _dispatching = self.core.dispatcher.lock_recover();
        let _gate = self.core.gate.lock_recover();
        call()
    }

    /// Runs a dispatcher call that may run **beside a render**: it takes the dispatcher
    /// lock, which serialises it against the other off-main dispatcher calls, the exclusive
    /// calls and the close, and not the audio gate, so it never makes a block refuse.
    ///
    /// This is what a host does in practice (JUCE's `getTextForOpcode` runs from the message
    /// thread with no lock shared with `processBlock`, authority 2); the 2.4 headers state
    /// no exclusion between these opcodes and `process` (authority 3). A plugin whose display
    /// text races its own `process` is the plugin's to make safe, as it is under every other
    /// host.
    fn concurrent<R>(&self, call: impl FnOnce() -> R) -> R {
        let _dispatching = self.core.dispatcher.lock_recover();
        call()
    }

    /// `effSetSampleRate`; exclusive (see [`PluginInstance`]'s docs).
    pub fn set_sample_rate(&mut self, _main: MainThreadMarker, rate: f32) {
        self.exclusive(|| {
            self.dispatch(
                plugin::EffectOpCode::SetSampleRate,
                0,
                0,
                ptr::null_mut(),
                rate,
            );
        });
    }

    /// `effSetBlockSize`; exclusive.
    pub fn set_block_size(&mut self, _main: MainThreadMarker, size: i64) {
        self.exclusive(|| {
            self.dispatch(
                plugin::EffectOpCode::SetBlockSize,
                0,
                size as isize,
                ptr::null_mut(),
                0.0,
            );
        });
    }

    /// `effMainsChanged(1)`; exclusive.
    pub fn resume(&mut self, _main: MainThreadMarker) {
        self.exclusive(|| {
            self.dispatch(
                plugin::EffectOpCode::StateChanged,
                0,
                1,
                ptr::null_mut(),
                0.0,
            );
        });
        self.resumed = true;
    }

    /// `effMainsChanged(0)`, if the plugin is resumed: a plugin already suspended would see a
    /// second one with no start before it. Exclusive.
    pub fn suspend(&mut self, _main: MainThreadMarker) {
        if self.resumed {
            self.exclusive(|| {
                self.dispatch(
                    plugin::EffectOpCode::StateChanged,
                    0,
                    0,
                    ptr::null_mut(),
                    0.0,
                );
            });
            self.resumed = false;
        }
    }

    /// Whether the plugin is resumed (`effMainsChanged(1)` and no suspend since).
    pub fn is_resumed(&self) -> bool {
        self.resumed
    }

    /// Dispatch `effSetProcessPrecision`, carrying the width in `value`:
    /// `0` = 32-bit, `1` = 64-bit. Exclusive.
    pub fn set_precision(&mut self, _main: MainThreadMarker, double: bool) {
        self.exclusive(|| {
            self.dispatch(
                plugin::EffectOpCode::SetPrecision,
                0,
                double as isize,
                ptr::null_mut(),
                0.0,
            );
        });
    }

    /// `effSetSpeakerArrangement`: proposes `input` and `output`, whether the plugin keeps
    /// them. The input travels in `value` and the output in `ptr`, both as pointers to a
    /// `VstSpeakerArrangement`. Legal only while the plugin is suspended, so it is
    /// exclusive and answers `false` without a call once the plugin is resumed.
    ///
    /// `true` is the plugin's `1`; what it kept is [`speaker_arrangement`](Self::speaker_arrangement).
    pub fn set_speaker_arrangement(
        &mut self,
        _main: MainThreadMarker,
        input: &SpeakerArrangement,
        output: &SpeakerArrangement,
    ) -> bool {
        if self.resumed {
            return false;
        }
        let mut input = input.encode();
        let mut output = output.encode();
        self.exclusive(|| {
            self.dispatch(
                plugin::EffectOpCode::SetSpeakerArrangement,
                0,
                input.as_mut_ptr() as isize,
                output.as_mut_ptr().cast::<c_void>(),
                0.0,
            ) == 1
        })
    }

    /// `effGetSpeakerArrangement`: the input and output arrangements the plugin holds.
    ///
    /// The host passes the addresses of two null `VstSpeakerArrangement` pointers and the
    /// plugin points them at arrangements it owns; both are copied before the call's lock is
    /// released, so neither outlives the plugin's storage. A suspended plugin is read
    /// exclusively; a resumed one under the dispatcher lock alone, which excludes other
    /// dispatches but not `process`.
    ///
    /// # Errors
    ///
    /// [`ArrangementRead::Unanswered`] when the plugin returns 0, and
    /// [`ArrangementRead::Malformed`] when it returns 1 with a null pointer, or with a type,
    /// speaker or channel count the format does not have.
    pub fn speaker_arrangement(
        &self,
        _main: MainThreadMarker,
    ) -> Result<(SpeakerArrangement, SpeakerArrangement), ArrangementRead> {
        let mut input: *const c_void = ptr::null();
        let mut output: *const c_void = ptr::null();
        let read = || {
            let answered = self.dispatch(
                plugin::EffectOpCode::GetSpeakerArrangement,
                0,
                ptr::addr_of_mut!(input) as isize,
                ptr::addr_of_mut!(output).cast::<c_void>(),
                0.0,
            );
            if answered != 1 {
                return Err(ArrangementRead::Unanswered);
            }
            // SAFETY: effGetSpeakerArrangement has the plugin point each slot at a
            // `VstSpeakerArrangement` it owns, readable for its channel count; `from_ptr`
            // refuses a null, misaligned or over-wide one. The calling lock (see below)
            // keeps every other dispatch, and for a suspended plugin the audio entries,
            // from running meanwhile; the plugin's own threads are its to keep from
            // rewriting an arrangement it has just reported.
            let (input, output) = unsafe {
                (
                    SpeakerArrangement::from_ptr(input),
                    SpeakerArrangement::from_ptr(output),
                )
            };
            input.zip(output).ok_or(ArrangementRead::Malformed)
        };
        // Suspended: exclusive, so no render is in flight either. Resumed: only the
        // dispatcher lock, which a read must not stall a render for, so the plugin keeps
        // its arrangement stable while it is resumed (the SDK has it set only when
        // suspended).
        if self.resumed {
            self.concurrent(read)
        } else {
            self.exclusive(read)
        }
    }

    /// `effEditKeyDown` for `key`, to the open editor: whether the plugin used the key.
    /// `false` with no call when no editor is open.
    pub fn editor_key_down(&mut self, _main: MainThreadMarker, key: KeyCode) -> bool {
        self.editor_key(plugin::EffectOpCode::EditorKeyDown, key)
    }

    /// `effEditKeyUp`, as [`editor_key_down`](Self::editor_key_down).
    pub fn editor_key_up(&mut self, _main: MainThreadMarker, key: KeyCode) -> bool {
        self.editor_key(plugin::EffectOpCode::EditorKeyUp, key)
    }

    /// The key opcodes carry the ASCII character in `index`, the virtual key in `value` and
    /// the modifier mask in `opt`, whose `f32` bits are the mask (the receiver reads
    /// `opt.to_bits()`).
    fn editor_key(&self, opcode: plugin::EffectOpCode, key: KeyCode) -> bool {
        if !self.editor_open {
            return false;
        }
        self.dispatch(
            opcode,
            // The ASCII character, or 0 for a key with none.
            if key.character.is_ascii() {
                key.character as i32
            } else {
                0
            },
            isize::from(key.key),
            ptr::null_mut(),
            f32::from_bits(u32::from(key.modifier)),
        ) == 1
    }

    /// `effCanDo`.
    pub fn can_do(&self, _main: MainThreadMarker, can_do: plugin::CanDo) -> Supported {
        let s: String = can_do.into();
        // `Supported::from` is total: undocumented `effCanDo` returns (a string
        // length, an uninitialised stack slot) become `Supported::Custom`.
        // Panicking here would abort during *load*, since the host queries
        // canDo while probing every plugin.
        Supported::from(self.write_string(plugin::EffectOpCode::CanDo, 0, 0, &s, usize::MAX, 0.0))
    }

    /// Whether the plugin publishes a GUI at all: `effFlagsHasEditor`.
    ///
    /// **Asks the plugin's flag, not the call count.** The flag is the one VST 2.4 defines
    /// for this query, and the one `vst::main` sets plugin-side when it has an editor.
    /// Deliberately *not* also requiring a successful `effEditGetRect`: the SDK lets a
    /// plugin compute its rect only once the editor is open, so requiring it would turn a
    /// plugin that has an editor into one that has not.
    pub fn has_editor(&self) -> bool {
        self.has_editor
    }

    /// The editor's size, `effEditGetRect`; `(0, 0)` when the plugin gives no rect.
    pub fn editor_size(&self, _main: MainThreadMarker) -> (i32, i32) {
        // Assuming coordinate origins from top-left
        match self.editor_rect() {
            None => (0, 0),
            Some(rect) => (
                i32::from(rect.right) - i32::from(rect.left),
                i32::from(rect.bottom) - i32::from(rect.top),
            ),
        }
    }

    fn editor_rect(&self) -> Option<Rect> {
        let mut rect: *mut Rect = std::ptr::null_mut();
        let rect_ptr: *mut *mut Rect = &mut rect;

        let result = self.dispatch(
            plugin::EffectOpCode::EditorGetRect,
            0,
            0,
            rect_ptr as *mut c_void,
            0.0,
        );

        if result == 0 || rect.is_null() {
            return None;
        }
        // SAFETY: `rect` is non-null (checked above) and `effEditGetRect` returned non-zero, so
        // per the VST2 contract the plugin set it to its own valid, aligned `ERect`; `Rect` is
        // `Copy`, so this copies it out. Validity is the plugin's word and is not checked, and
        // the plugin owns the memory (never freed here).
        Some(unsafe { *rect })
    }

    /// Dispatch `effEditOpen` into `parent`, reporting whether the plugin embedded its editor.
    ///
    /// Two corrections over the original:
    ///
    /// * **`> 0`, not `== 1`.** Shipping plugins return other positive values
    ///   (some the window handle), and `== 1` reported those successful opens as
    ///   refusals. `> 0` matches every other boolean opcode in this file.
    /// * **Refuse a second open.** VST 2.4 requires an `effEditClose` between
    ///   two `effEditOpen`s; opening twice leaks the first window in practice.
    ///   The already-open case answers `true`, since the editor *is* open, so a
    ///   caller does not read an idempotent open as a failure.
    pub fn open_editor(&mut self, _main: MainThreadMarker, parent: WindowHandle) -> bool {
        if self.editor_open {
            warn!("effEditOpen called on an already-open editor; ignoring the second open");
            return true;
        }

        let result = self.dispatch(plugin::EffectOpCode::EditorOpen, 0, 0, parent.as_ptr(), 0.0);

        let opened = result > 0;
        if opened {
            self.editor_open = true;
        }

        opened
    }

    /// Dispatch `effEditClose`, but only when an editor is actually open.
    ///
    /// The guard is load-bearing: hosts routinely have two closing paths (an
    /// explicit close and the drop), and a second `effEditClose` has no
    /// defined meaning in VST 2.4 — several real plugins double-free their
    /// window resources on it. Idempotence lives here, where `editor_open` lives.
    pub fn close_editor(&mut self, _main: MainThreadMarker) {
        if !self.editor_open {
            return;
        }
        self.dispatch(
            plugin::EffectOpCode::EditorClose,
            0,
            0,
            ptr::null_mut(),
            0.0,
        );
        self.editor_open = false;
    }

    /// Dispatch `effEditIdle` while the editor is open.
    ///
    /// The host calls this on a timer (about 30 Hz) from its main thread;
    /// editors that animate or flush UI work do it here. Never before
    /// `effEditOpen` or after `effEditClose`: the opcode has no meaning for a
    /// window that does not exist.
    pub fn idle_editor(&mut self, _main: MainThreadMarker) {
        if !self.editor_open {
            return;
        }
        self.dispatch(plugin::EffectOpCode::EditorIdle, 0, 0, ptr::null_mut(), 0.0);
    }

    /// `effChangeProgram`.
    pub fn change_preset(&self, _main: MainThreadMarker, preset: i32) {
        self.dispatch(
            plugin::EffectOpCode::ChangePreset,
            0,
            preset as isize,
            ptr::null_mut(),
            0.0,
        );
    }

    /// `effGetProgram`.
    pub fn get_preset_num(&self, _main: MainThreadMarker) -> i32 {
        self.opcode(plugin::EffectOpCode::GetCurrentPresetNum) as i32
    }

    /// The name of preset `preset` (`effGetProgramNameIndexed`), empty when the plugin gives none.
    pub fn get_preset_name(&self, _main: MainThreadMarker, preset: i32) -> String {
        self.read_string_param(
            plugin::EffectOpCode::GetPresetName,
            preset,
            0,
            0.0,
            MAX_PRESET_NAME_LEN,
        )
    }

    /// The real `effGetChunk` answer for the current preset, with a failed save
    /// distinguishable from an empty one — what the [`GetChunkError`] split exists for.
    ///
    /// # Errors
    ///
    /// [`GetChunkError`] when the plugin attempted the save and failed.
    pub fn try_get_preset_data(&self, _main: MainThreadMarker) -> Result<Vec<u8>, GetChunkError> {
        self.core.get_chunk(1 /*preset*/)
    }

    /// Hand a preset chunk to the plugin (`effSetChunk`, index 1: the current program's).
    ///
    /// Nothing is reported back: the header gives the opcode no return value, so there is no
    /// refusal to read (see `Core::set_chunk`). Set the program first
    /// ([`change_preset`](Self::change_preset)): a program chunk is the current program's.
    pub fn load_preset_data(&self, _main: MainThreadMarker, data: &[u8]) {
        self.core.set_chunk(1, data);
    }
}

/// The parameter surface hosts call from either thread; see the type's docs.
impl PluginInstance {
    /// `getParameter`. `None` when the plugin left `AEffect::getParameter` null, which VST 2.4
    /// permits for a plugin declaring `numParams == 0`.
    pub fn get_parameter(&self, index: i32) -> Option<f32> {
        let effect = self.core.get_effect();
        // SAFETY: `get_effect` returns the live `AEffect` this instance wraps: the instance is
        // the only closer, and it is borrowed here.
        let get = unsafe { (*effect).getParameter }?;
        // SAFETY: `get` came out of that same `AEffect`, so it is the effect's
        // own accessor being handed the effect it belongs to. `index` is the
        // caller's to bound — VST 2.4 gives the callee no way to report a bad
        // one, which is the obligation `GetParameterProc` being `unsafe` states.
        Some(unsafe { get(effect, index) })
    }

    /// Whether the plugin filled `AEffect::setParameter`: without it no write reaches the plugin.
    pub fn exposes_set_parameter(&self) -> bool {
        let effect = self.core.get_effect();
        // SAFETY: as above.
        unsafe { (*effect).setParameter }.is_some()
    }

    /// `setParameter`. `false` when the plugin left `AEffect::setParameter` null — the value
    /// went nowhere.
    pub fn set_parameter(&self, index: i32, value: f32) -> bool {
        let effect = self.core.get_effect();
        // SAFETY: as above.
        let Some(set) = (unsafe { (*effect).setParameter }) else {
            return false;
        };
        // SAFETY: as in `get_parameter` — the setter belongs to this effect,
        // and `index` is the caller's obligation.
        unsafe { set(effect, index, value) };
        true
    }

    /// `effGetParamLabel`.
    pub fn get_parameter_label(&self, index: i32) -> String {
        self.concurrent(|| {
            self.read_string_param(
                plugin::EffectOpCode::GetParameterLabel,
                index,
                0,
                0.0,
                MAX_PARAM_STR_LEN,
            )
        })
    }

    /// `effGetParamDisplay`.
    pub fn get_parameter_text(&self, index: i32) -> String {
        self.concurrent(|| {
            self.read_string_param(
                plugin::EffectOpCode::GetParameterDisplay,
                index,
                0,
                0.0,
                MAX_PARAM_STR_LEN,
            )
        })
    }

    /// `effGetParamName`.
    pub fn get_parameter_name(&self, index: i32) -> String {
        self.concurrent(|| {
            self.read_string_param(
                plugin::EffectOpCode::GetParameterName,
                index,
                0,
                0.0,
                MAX_PARAM_STR_LEN,
            )
        })
    }

    /// `effCanBeAutomated`.
    pub fn can_be_automated(&self, index: i32) -> bool {
        self.concurrent(|| {
            self.dispatch(
                plugin::EffectOpCode::CanBeAutomated,
                index,
                0,
                ptr::null_mut(),
                0.0,
            ) > 0
        })
    }

    /// `effString2Parameter`: a setter, with no parse-only counterpart.
    pub fn string_to_parameter(&self, index: i32, text: String) -> bool {
        self.concurrent(|| {
            self.write_string(
                plugin::EffectOpCode::StringToParameter,
                index,
                0,
                &text,
                usize::MAX,
                0.0,
            ) > 0
        })
    }
}

/// How many `PluginLoader::instance` calls are between the start of `main` and the
/// adoption (or refusal) of the effect it returned, on any thread.
static LOADS_IN_PROGRESS: AtomicUsize = AtomicUsize::new(0);

/// The `AEffect` addresses whose `reserved1` is a host box this crate wrote: from adoption
/// in `PluginLoader::instance` until the core's drop, after `effClose`.
static ADOPTED: Mutex<BTreeSet<usize>> = Mutex::new(BTreeSet::new());

thread_local! {
    /// The `AEffect` this thread is inside a render entry of (`RenderEntry::process`,
    /// `process_f64`, `dispatch`), null otherwise. A host callback the plugin makes from
    /// inside such a call names that effect, which the host adopted before it could
    /// render, so `callback_wrapper` needs no registry lookup (and no lock) for it.
    static RENDERING: Cell<*mut AEffect> = const { Cell::new(ptr::null_mut()) };
}

/// Sets [`RENDERING`] for its lifetime and restores the previous value (a render entry
/// can nest: `process_events` is dispatched from inside a block's work).
struct Rendering(*mut AEffect);

impl Rendering {
    fn enter(effect: *mut AEffect) -> Rendering {
        Rendering(RENDERING.replace(effect))
    }
}

impl Drop for Rendering {
    fn drop(&mut self) {
        RENDERING.set(self.0);
    }
}

/// Marks a load in progress for its lifetime.
struct Loading;

impl Loading {
    fn begin() -> Loading {
        // `SeqCst`: the increment happens before `main` runs, so every thread `main`
        // spawns observes it.
        LOADS_IN_PROGRESS.fetch_add(1, Ordering::SeqCst);
        Loading
    }
}

impl Drop for Loading {
    fn drop(&mut self) {
        LOADS_IN_PROGRESS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Registry lookups, to show in a test that the render path takes none.
#[cfg(test)]
static REGISTRY_LOCKS: AtomicUsize = AtomicUsize::new(0);

fn adopted() -> std::sync::MutexGuard<'static, BTreeSet<usize>> {
    #[cfg(test)]
    REGISTRY_LOCKS.fetch_add(1, Ordering::SeqCst);
    ADOPTED.lock_recover()
}

fn adopt(effect: *mut AEffect) {
    adopted().insert(effect as usize);
}

fn unadopt(effect: *mut AEffect) {
    adopted().remove(&(effect as usize));
}

thread_local! {
    /// The `*const Arc<T>` of the loader running `VSTPluginMain` on this thread, null otherwise.
    ///
    /// A plugin calls the host callback during `main` on the loading thread, before the host
    /// can store its pointer in `AEffect::reserved1`. Thread-local, so concurrent loads on
    /// different threads each see their own host without a lock. Once `main` returns the
    /// instance's `reserved1` carries the host, and this is null again.
    ///
    /// No process-wide load lock is needed for the host pointer. A callback during `main` from
    /// a thread the plugin spawned finds null and answers `0`.
    static LOAD_HOST: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
}

/// Function passed to plugin to handle dispatching host opcodes.
///
/// This is a C function pointer the plugin calls, including from its audio
/// thread inside `processReplacing`. Two properties are load-bearing:
///
/// * **No lock.** The host is reached through a plain `Arc<T>` (see
///   [`PluginLoader::load`]); a shared mutex here would make the audio thread
///   block on the GUI thread.
/// * **No unwinding.** Unwinding out of an `extern "C"` frame into the
///   plugin's C++ stack is undefined behaviour, so the whole dispatch is
///   run under `ffi_guard` and a panic degrades to `0` — the VST2 "not
///   handled / not supported" answer for every host opcode.
extern "C" fn callback_wrapper<T: Host>(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    opt: f32,
) -> isize {
    // Unwind safety is asserted by `ffi_guard`: on a panic we return 0 and touch none of the
    // captured state again — the host is behind a shared reference and the raw pointers
    // are not re-read.
    //
    // SAFETY: which host a call reaches is decided without trusting `effect` until it is
    // known to be ours.
    //
    // 1. `LOAD_HOST` first. While this thread is inside `main` it points at the loader's
    //    `Arc<T>`, which outlives `main` (see `call_main`), and is the host for every call
    //    made on this thread: the `effect` a plugin passes then is not yet the host's
    //    (`reserved1` is "reserved for host", and a plugin that does not zero its `AEffect`
    //    leaves garbage there), so it is not read.
    // 2. No load in progress anywhere (`LOADS_IN_PROGRESS == 0`): every live effect has been
    //    adopted, so a non-null `effect` is readable (VST2 ABI) and its `reserved1`, when
    //    non-zero, is the `Box<Arc<T>>` set by `PluginLoader::instance`, kept alive by the
    //    core until after `effClose`; this callback is monomorphised for the same `T`. This
    //    is the steady-state path and takes no lock.
    // 3. A load is in progress and this thread has no `LOAD_HOST`:
    //    a. the call comes from inside a render entry of `effect` itself (`RENDERING`, set
    //       by `RenderEntry` for the duration of the plugin call, per thread): the effect
    //       was adopted before it could be rendered, so it is ours by construction. No
    //       registry, no lock: an audio thread playing one plugin is not made to contend
    //       while another loads.
    //    b. otherwise (a thread the plugin spawned, during `main` or not; a UI thread):
    //       `reserved1` is dereferenced only if `effect` is in the registry of adopted
    //       effects, else the call gets the load-time default answer, `0`. The registry
    //       is a mutex, taken only by those threads and only in this window. The count covers `main` through adoption
    //    (`instance` holds `Loading` across both), so an effect cannot be both unadopted
    //    and called after the count has dropped, except by a plugin that calls the host
    //    after its load ended with an `AEffect` the host refused or never adopted; that is
    //    a plugin outside the ABI and stays unchecked.
    //
    // A plugin that overwrites `reserved1` or calls the host after `effClose` breaks (2)
    // too, which is unchecked.
    ffi_guard(0, || unsafe {
        let load_ptr = LOAD_HOST.get() as *const Arc<T>;
        let host: &T = if !load_ptr.is_null() {
            // Used only during the plugin initialization.
            &*load_ptr
        } else if effect.is_null()
            || (LOADS_IN_PROGRESS.load(Ordering::SeqCst) != 0
                && effect != RENDERING.get()
                && !adopted().contains(&(effect as usize)))
        {
            return 0;
        } else if (*effect).reserved1 != 0 {
            // The plugin has already been initialized.
            &*((*effect).reserved1 as *const Arc<T>)
        } else {
            return 0;
        };
        // SAFETY: `ptr` is what the plugin passed `audioMasterCallback` for `opcode`, which is
        // `host_dispatch`'s contract.
        interfaces::host_dispatch(host, effect, opcode, index, value, ptr, opt)
    })
}

#[cfg(test)]
mod tests;
