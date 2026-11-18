//! IComponentHandler (v1/v2/v3) + IComponentHandlerBusActivation + IProgress +
//! IUnitHandler (v1/v2) — multi-vtable host-side dispatcher for every plugin
//! callback.
//!
//! # Where the callbacks go, and why nothing here allocates
//!
//! Plugins call `performEdit`, `beginEdit`, `endEdit` and `restartComponent`
//! from their *process* thread. The spec says not to; many do anyway. So the
//! receiving half must be safe to run on an audio thread, which an unbounded
//! channel is not: its `send` allocates a block whenever the current one fills.
//!
//! - **Restart requests** are cumulative — the host only ever acts on the union
//!   of the flags since its last look — so they live in one `AtomicI32`,
//!   `fetch_or`'d by the plugin and swapped to zero by
//!   [`ComponentHandler::take_restart`], which returns them as
//!   [`RestartFlags`]. No queue, no capacity, nothing to lose.
//! - **The same flags are also raised as [`Request`]s** on a [`RequestWord`]
//!   ([`ComponentHandler::requests`]), the form the shared core takes:
//!   [`RestartFlags::raise_into`] says which flag is which request. A host that
//!   drives a [`Lifecycle`](tutti_plugin_types::Lifecycle) reads that word; the
//!   coalesced flags stay for a plugin held without one.
//! - **Everything else** (edits, progress, unit news) goes on one bounded
//!   [`ArrayQueue`] of [`PluginNotification`], preallocated at construction.
//!   A push past [`NOTIFICATION_CAPACITY`] is dropped and counted
//!   ([`ComponentHandler::dropped_notifications`]), never logged and never
//!   grown; a host that sees the count move re-reads every parameter value.
//!
//! A restart therefore carries no position relative to the queued events. No
//! consumer relied on one: the only reader always folded every restart into a
//! single coalesced outcome, whatever edits surrounded it.
//!
//! `IProgress::start`'s description is inline too ([`ProgressDescription`],
//! truncated at [`PROGRESS_DESCRIPTION_CAPACITY`] bytes), so no callback
//! allocates, and dropping a queued notification frees nothing.

use super::Lost;
use crate::guard::Out;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use tutti_plugin_types::ffi_guard;

use tutti_plugin_types::{Request, RescanScope};
use tutti_rt::RequestWord;
use tutti_units::Normalized;

use arrayvec::ArrayString;
use bitflags::bitflags;
use crossbeam_queue::ArrayQueue;
use vst3::Steinberg::Vst::RestartFlags_;
use vst3::Steinberg::{
    FIDString, IPlugView, TBool,
    Vst::{
        BusDirection, IComponentHandler, IComponentHandler2, IComponentHandler2Trait,
        IComponentHandler3, IComponentHandler3Trait, IComponentHandlerBusActivation,
        IComponentHandlerBusActivationTrait, IComponentHandlerTrait, IContextMenu, IProgress,
        IProgress_::{ID, ProgressType},
        IProgressTrait, IUnitHandler, IUnitHandler2, IUnitHandler2Trait, IUnitHandlerTrait,
        MediaType, ParamID, ParamValue, ProgramListID, UnitID,
    },
    kResultOk, tresult,
};
use vst3::{Class, ComWrapper};

/// What an `IProgress` operation is, by the SDK's `ProgressType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressKind {
    /// The plugin is restoring state asynchronously.
    AsyncStateRestoration,
    /// A background task the UI reports.
    UiBackgroundTask,
    /// A `ProgressType` the SDK does not define, as the plugin sent it.
    Other(u32),
}

impl ProgressKind {
    fn from_raw(raw: u32) -> Self {
        use vst3::Steinberg::Vst::IProgress_::ProgressType_;
        match raw {
            ProgressType_::AsyncStateRestoration => Self::AsyncStateRestoration,
            ProgressType_::UIBackgroundTask => Self::UiBackgroundTask,
            other => Self::Other(other),
        }
    }
}

/// Long-running progress notifications emitted by plugins (sample loading,
/// offline rendering, etc.). Delivered via
/// the host's notification poll.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A new progress operation has begun. `id` uniquely identifies this
    /// operation across its lifetime; `progress_type` is what kind of operation
    /// it is; `description` is a human-readable label.
    Started {
        /// Identifies this operation across its lifetime; matches the `id` of
        /// the later `Updated` and `Finished` events.
        id: u64,
        /// What kind of operation this is.
        progress_type: ProgressKind,
        /// Human-readable label supplied by the plugin, truncated to
        /// [`PROGRESS_DESCRIPTION_CAPACITY`] bytes.
        description: ProgressDescription,
    },
    /// Progress update, normalized to `0.0..=1.0`.
    Updated {
        /// The operation this update belongs to.
        id: u64,
        /// Completion fraction, `0.0..=1.0`.
        progress: f64,
    },
    /// The operation with this id has finished.
    Finished {
        /// The operation that ended. No further events carry this id.
        id: u64,
    },
}

/// How many UTF-8 bytes of an `IProgress::start` description are kept.
/// Longer descriptions are cut at the last whole character that fits.
pub const PROGRESS_DESCRIPTION_CAPACITY: usize = 128;

/// The label a plugin gave an `IProgress` operation, held inline so that
/// `IProgress::start` allocates nothing on the plugin's thread and dropping
/// the event frees nothing on the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressDescription(ArrayString<PROGRESS_DESCRIPTION_CAPACITY>);

impl ProgressDescription {
    /// The description as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Decodes a NUL-terminated UTF-16 string, replacing unpaired surrogates
    /// with U+FFFD and stopping at the first character that does not fit.
    /// Reads no further than the terminator or the first character past the
    /// capacity.
    ///
    /// # Safety
    ///
    /// `ptr` is null or points at a NUL-terminated UTF-16 string.
    unsafe fn from_utf16_nul(ptr: *const u16) -> Self {
        let mut out = ArrayString::new();
        if ptr.is_null() {
            return Self(out);
        }
        let mut cursor = ptr;
        let units = std::iter::from_fn(|| {
            // SAFETY: the caller guarantees a terminator; reading stops at it.
            let unit = unsafe { *cursor };
            (unit != 0).then(|| {
                // SAFETY: `unit` was not the terminator, so the next unit is still
                // inside the string (at worst the terminator itself).
                cursor = unsafe { cursor.add(1) };
                unit
            })
        });
        for c in char::decode_utf16(units) {
            if out
                .try_push(c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .is_err()
            {
                break;
            }
        }
        Self(out)
    }
}

impl std::ops::Deref for ProgressDescription {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for ProgressDescription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Unit / program-list change notifications from the plugin. Delivered via
/// the host's notification poll.
#[derive(Debug, Clone)]
pub enum UnitEvent {
    /// Plugin has selected a different unit (preset category / voice).
    UnitSelected(i32),
    /// Program *information* in a list went stale — a rename, a preset load, or
    /// a PitchName change (`ivstunits.h:88-92`). Not a selection change: the
    /// plugin is saying what it holds is no longer what the host cached, so the
    /// response is to re-read the list, not to move a cursor.
    ///
    /// `program_index` is `-1` (`kAllProgramInvalid`) when *every* program in
    /// the list is invalid, and only otherwise names a single one. That is a
    /// sentinel, not an index — spending it as one reads before the start of
    /// whatever array holds the list.
    ProgramListChanged {
        /// The program list to re-read.
        list_id: i32,
        /// A single stale program, or `-1` for "all of them" — see above.
        program_index: i32,
    },
    /// The unit ↔ bus mapping has changed (IUnitHandler2).
    UnitByBusChanged,
}

bitflags! {
    /// The `RestartFlags` bitmask a plugin passes to
    /// `IComponentHandler::restartComponent`: each bit is one "the host must
    /// re-sync X" signal. The handler holds the union of every mask since the
    /// host's last look ([`ComponentHandler::take_restart`]).
    ///
    /// Spec reference: `Steinberg::Vst::RestartFlags`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct RestartFlags: i32 {
        /// `kReloadComponent`: the plugin must be fully reloaded (deactivate and
        /// reactivate). The heaviest signal; usually emitted after an in-plugin
        /// preset load that changes the component structure.
        const RELOAD_COMPONENT = RestartFlags_::kReloadComponent;
        /// `kIoChanged`: input/output bus configuration changed; re-run bus
        /// enumeration and arrangement negotiation.
        const IO_CHANGED = RestartFlags_::kIoChanged;
        /// `kParamValuesChanged`: one or more parameter values changed (an
        /// in-plugin preset load); re-read parameter values into host state.
        const PARAM_VALUES_CHANGED = RestartFlags_::kParamValuesChanged;
        /// `kLatencyChanged`: processing latency changed; re-read
        /// `getLatencySamples`.
        const LATENCY_CHANGED = RestartFlags_::kLatencyChanged;
        /// `kParamTitlesChanged`: parameter titles, units or flags changed;
        /// re-read parameter info.
        const PARAM_TITLES_CHANGED = RestartFlags_::kParamTitlesChanged;
        /// `kMidiCCAssignmentChanged`: the `IMidiMapping` CC to parameter table
        /// changed; re-query it.
        const MIDI_CC_ASSIGNMENT_CHANGED = RestartFlags_::kMidiCCAssignmentChanged;
        /// `kNoteExpressionChanged`: note-expression support changed.
        const NOTE_EXPRESSION_CHANGED = RestartFlags_::kNoteExpressionChanged;
        /// `kIoTitlesChanged`: bus titles changed (cosmetic).
        const IO_TITLES_CHANGED = RestartFlags_::kIoTitlesChanged;
        /// `kPrefetchableSupportChanged`: offline prefetch support changed.
        const PREFETCHABLE_SUPPORT_CHANGED = RestartFlags_::kPrefetchableSupportChanged;
        /// `kRoutingInfoChanged`: channel routing info changed.
        const ROUTING_INFO_CHANGED = RestartFlags_::kRoutingInfoChanged;
        /// `kKeyswitchChanged`: keyswitch info changed.
        const KEYSWITCH_CHANGED = RestartFlags_::kKeyswitchChanged;
        /// `kParamIDMappingChanged`: the parameter id mapping changed.
        const PARAM_ID_MAPPING_CHANGED = RestartFlags_::kParamIDMappingChanged;
    }
}

impl RestartFlags {
    /// Raises each flag as the core's [`Request`] it is.
    ///
    /// A latency or I/O change and a reload are the core's own; a parameter
    /// remap is a rescan of the parameter info, as a title change is;
    /// `kPrefetchableSupportChanged` asks for the deactivate and activate the
    /// header names, which is a restart. The three that nothing acts on (bus
    /// titles, routing, keyswitches) raise nothing: they stay in the flags of
    /// [`ComponentHandler::take_restart`].
    pub fn raise_into(self, word: &RequestWord) {
        let requests = [
            (self.contains(Self::RELOAD_COMPONENT), Request::Reload),
            (self.contains(Self::IO_CHANGED), Request::IoChanged),
            (
                self.contains(Self::LATENCY_CHANGED),
                Request::LatencyChanged,
            ),
            (
                self.contains(Self::PARAM_VALUES_CHANGED),
                Request::RescanParams(RescanScope::Values),
            ),
            (
                self.intersects(Self::PARAM_TITLES_CHANGED | Self::PARAM_ID_MAPPING_CHANGED),
                Request::RescanParams(RescanScope::Info),
            ),
            (
                self.contains(Self::MIDI_CC_ASSIGNMENT_CHANGED),
                Request::MidiCcAssignmentChanged,
            ),
            (
                self.contains(Self::NOTE_EXPRESSION_CHANGED),
                Request::NoteExpressionChanged,
            ),
            (
                self.contains(Self::PREFETCHABLE_SUPPORT_CHANGED),
                Request::Restart,
            ),
        ];
        for (raised, request) in requests {
            if raised {
                let _ = request.raise(word);
            }
        }
    }
}

/// Notifications the plugin's editor pushes through `IComponentHandler` and
/// its v2/v3/bus-activation extensions, delivered via
/// the host's notification poll.
///
/// `restartComponent` is not among them: restart requests are cumulative, so
/// they are coalesced into the [`RestartFlags`] that
/// [`ComponentHandler::take_restart`] returns rather than queued one per call.
///
/// Typical usage: a user drags a knob in the plugin UI → the plugin calls
/// `beginEdit` / `performEdit` / `endEdit` → the host receives the matching
/// [`ParameterEditEvent`]s and updates its own automation state.
#[derive(Debug, Clone)]
pub enum ParameterEditEvent {
    /// Plugin is about to start editing the parameter (mouse-down on a knob).
    BeginEdit(u32),
    /// Plugin is reporting a new normalized (0.0 – 1.0) value for the
    /// parameter.
    PerformEdit {
        /// The parameter being edited.
        param_id: u32,
        /// Normalized value, `0.0..=1.0`.
        value: f64,
    },
    /// Plugin has finished editing the parameter (mouse-up).
    EndEdit(u32),
    /// Plugin has marked itself dirty — the host should treat project state
    /// as modified.
    SetDirty(bool),
    /// Plugin requests the host open its editor (e.g. from a context menu).
    RequestOpenEditor,
    /// Plugin signals the start of a coalescable group of edits.
    StartGroupEdit,
    /// Plugin signals the end of a coalescable group of edits.
    FinishGroupEdit,
    /// Plugin requests that a bus be activated or deactivated.
    RequestBusActivation {
        /// Audio or event.
        media_type: crate::MediaType,
        /// Input or output.
        direction: crate::BusDirection,
        /// Bus index within that media type and direction.
        index: i32,
        /// `true` to activate the bus, `false` to deactivate it.
        state: bool,
    },
}

/// One queued plugin callback, as
/// the host's notification poll
/// hands it over, in arrival order across all three kinds.
#[derive(Debug, Clone)]
pub enum PluginNotification {
    /// An `IComponentHandler`(2/bus-activation) call: an edit gesture, dirty
    /// state, an editor request.
    Edit(ParameterEditEvent),
    /// An `IProgress` report.
    Progress(ProgressEvent),
    /// An `IUnitHandler`(2) report.
    Unit(UnitEvent),
}

/// How many [`PluginNotification`]s the handler holds between two polls.
///
/// A UI that polls once a frame drains a few dozen at most, even mid-gesture.
/// The bound exists for the poller that stalls: the queue then stays at this
/// size instead of growing memory.
///
/// **Overflow drops the newest notification**, and counts it. Keeping the
/// oldest means a gesture can lose its last `PerformEdit` and its `EndEdit`,
/// so the host's view of a parameter may be stale. A host that sees
/// [`ComponentHandler::dropped_notifications`] move re-reads every parameter
/// value, as for `kParamValuesChanged`. (Dropping the oldest instead would lose the
/// `BeginEdit` and break a gesture just the same; there is no order-safe drop.)
pub const NOTIFICATION_CAPACITY: usize = 512;

/// The host's `IComponentHandler` (and friends). See the module docs for where
/// each callback lands.
pub struct ComponentHandler {
    /// Edits, progress and unit news, in arrival order.
    notifications: ArrayQueue<PluginNotification>,
    /// Pushes refused because `notifications` was full.
    dropped: Lost,
    /// The union of every `restartComponent` flag word since the last take:
    /// [`RestartFlags`] bits.
    restart: AtomicI32,
    /// The same flags as the core's requests, for a host that drives a
    /// `Lifecycle`.
    requests: RequestWord,
    next_progress_id: AtomicU64,
}

impl Class for ComponentHandler {
    type Interfaces = (
        IComponentHandler,
        IComponentHandler2,
        IComponentHandler3,
        IComponentHandlerBusActivation,
        IProgress,
        IUnitHandler,
        IUnitHandler2,
    );
}

impl ComponentHandler {
    /// A handler with an empty, preallocated notification queue.
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            notifications: ArrayQueue::new(NOTIFICATION_CAPACITY),
            dropped: Lost::new(),
            restart: AtomicI32::new(0),
            requests: RequestWord::new(),
            next_progress_id: AtomicU64::new(1),
        })
    }

    /// What the plugin has asked of the host as the core's requests, raised by
    /// every `restartComponent`. Taken with
    /// [`taken`](tutti_plugin_types::taken): wait-free and allocation-free.
    pub fn requests(&self) -> &RequestWord {
        &self.requests
    }

    /// Takes the union of every `restartComponent` flag word received since
    /// the last take, leaving none behind. Wait-free and allocation-free, so
    /// it may run on an audio thread.
    pub fn take_restart(&self) -> RestartFlags {
        // Acquire pairs with the plugin's Release `fetch_or`: whatever the
        // plugin wrote before asking for the restart (a new latency, a new bus
        // layout) is visible to the host once it sees the bit.
        RestartFlags::from_bits_truncate(self.restart.swap(0, Ordering::Acquire))
    }

    /// The oldest queued notification, if any. Lock-free and allocation-free,
    /// and dropping what it returns frees nothing.
    pub fn pop_notification(&self) -> Option<PluginNotification> {
        self.notifications.pop()
    }

    /// How many notifications are queued, unread.
    pub fn pending_notifications(&self) -> usize {
        self.notifications.len()
    }

    /// Notifications refused because the queue held
    /// [`NOTIFICATION_CAPACITY`] unread ones. Cumulative over the handler's
    /// life; a host polls it to learn that it read too rarely.
    pub fn dropped_notifications(&self) -> u64 {
        self.dropped.get()
    }

    /// Queues `n`, or counts it if the queue is full. Never allocates and
    /// never blocks: the plugin may be calling from its process thread.
    fn push(&self, n: PluginNotification) {
        if self.notifications.push(n).is_err() {
            self.dropped.add(1);
        }
    }

    fn edit(&self, e: ParameterEditEvent) {
        self.push(PluginNotification::Edit(e));
    }
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, id: ParamID) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.edit(ParameterEditEvent::BeginEdit(id));
            kResultOk
        })
    }

    unsafe fn performEdit(&self, id: ParamID, value_normalized: ParamValue) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.edit(ParameterEditEvent::PerformEdit {
                param_id: id,
                // The plugin's `f64` is untrusted, as `addPoint`'s is.
                value: Normalized::clamped(value_normalized).get(),
            });
            kResultOk
        })
    }

    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.edit(ParameterEditEvent::EndEdit(id));
            kResultOk
        })
    }

    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            // Release pairs with `take_restart`' Acquire; see there.
            let flags = RestartFlags::from_bits_truncate(flags);
            self.restart.fetch_or(flags.bits(), Ordering::Release);
            flags.raise_into(&self.requests);
            kResultOk
        })
    }
}

impl IComponentHandler2Trait for ComponentHandler {
    unsafe fn setDirty(&self, state: TBool) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.edit(ParameterEditEvent::SetDirty(state != 0));
            kResultOk
        })
    }

    unsafe fn requestOpenEditor(&self, _name: FIDString) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.edit(ParameterEditEvent::RequestOpenEditor);
            kResultOk
        })
    }

    unsafe fn startGroupEdit(&self) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.edit(ParameterEditEvent::StartGroupEdit);
            kResultOk
        })
    }

    unsafe fn finishGroupEdit(&self) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.edit(ParameterEditEvent::FinishGroupEdit);
            kResultOk
        })
    }
}

impl IComponentHandler3Trait for ComponentHandler {
    /// Declines to build a host context menu: `null` is the spec's "the host
    /// offers no menu here", and the plugin falls back to its own. Nothing in
    /// the host contributes menu items or consumes an `IContextMenu`.
    unsafe fn createContextMenu(
        &self,
        _plug_view: *mut IPlugView,
        _param_id: *const ParamID,
    ) -> *mut IContextMenu {
        std::ptr::null_mut()
    }
}

impl IComponentHandlerBusActivationTrait for ComponentHandler {
    unsafe fn requestBusActivation(
        &self,
        media_type: MediaType,
        dir: BusDirection,
        index: i32,
        state: TBool,
    ) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            // A media type or direction the SDK does not define names no bus.
            let (Some(media_type), Some(direction)) = (
                crate::MediaType::from_raw(media_type),
                crate::BusDirection::from_raw(dir),
            ) else {
                return vst3::Steinberg::kInvalidArgument;
            };
            self.edit(ParameterEditEvent::RequestBusActivation {
                media_type,
                direction,
                index,
                state: state != 0,
            });
            kResultOk
        })
    }
}

impl IProgressTrait for ComponentHandler {
    unsafe fn start(
        &self,
        r#type: ProgressType,
        optional_description: *const u16,
        out_id: *mut ID,
    ) -> tresult {
        // SAFETY: a COM out-parameter: null or valid for this call.
        let out_id = unsafe { Out::new(out_id) };
        out_id.set(0);
        ffi_guard(vst3::Steinberg::kInternalError, || {
            let id = self.next_progress_id.fetch_add(1, Ordering::SeqCst);
            // SAFETY: the interface passes null or a NUL-terminated string.
            let desc = unsafe { ProgressDescription::from_utf16_nul(optional_description) };
            self.push(PluginNotification::Progress(ProgressEvent::Started {
                id,
                progress_type: ProgressKind::from_raw(r#type),
                description: desc,
            }));
            out_id.set(id);
            kResultOk
        })
    }

    unsafe fn update(&self, id: ID, norm_value: ParamValue) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.push(PluginNotification::Progress(ProgressEvent::Updated {
                id,
                progress: Normalized::clamped(norm_value).get(),
            }));
            kResultOk
        })
    }

    unsafe fn finish(&self, id: ID) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.push(PluginNotification::Progress(ProgressEvent::Finished { id }));
            kResultOk
        })
    }
}

impl IUnitHandlerTrait for ComponentHandler {
    unsafe fn notifyUnitSelection(&self, unit_id: UnitID) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.push(PluginNotification::Unit(UnitEvent::UnitSelected(unit_id)));
            kResultOk
        })
    }

    unsafe fn notifyProgramListChange(
        &self,
        list_id: ProgramListID,
        program_index: i32,
    ) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.push(PluginNotification::Unit(UnitEvent::ProgramListChanged {
                list_id,
                program_index,
            }));
            kResultOk
        })
    }
}

impl IUnitHandler2Trait for ComponentHandler {
    unsafe fn notifyUnitByBusChange(&self) -> tresult {
        ffi_guard(vst3::Steinberg::kInternalError, || {
            self.push(PluginNotification::Unit(UnitEvent::UnitByBusChanged));
            kResultOk
        })
    }
}

impl std::fmt::Debug for ComponentHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentHandler")
            .field("pending", &self.notifications.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
