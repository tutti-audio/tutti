//! Values: what the session's three verbs read and write.
//!
//! A value is named by a [`Target`], a `Copy` reference taken from a key
//! (`filter.cutoff()`) that carries its value type. [`Session::set`] takes
//! assignments built from targets (`target.to(v)`, `target.modify(f)`) and
//! [`Session::get`] takes targets, each as one item, a tuple, an array, a
//! `Vec` or an `Option`, and answers in the argument's shape.
//!
//! Every call takes the session's value lock, a short control-side lock the
//! audio thread never touches, so a call from any thread sees a whole batch or
//! none of it:
//!
//! - **Order.** The items of a `set` run in argument order, each `modify`
//!   seeing the value the call has built so far for its target:
//!   `(n.modify(|x| x + 1), n.modify(|x| x + 1))` adds two, and
//!   `(a.to(1), a.modify(|x| x + 1))` gives 2.
//! - **Landing.** A value the audio thread reads through a published cell (the
//!   tempo map, the meter, the loop region, a node's clip or changes) is
//!   staged by the write, a later write replacing it, and published with the
//!   others staged by the next [`Session::update`] in one fixed order; a read
//!   answers the staged value, else the one in force. A value in an atomic
//!   cell (a param, the click, a route's depth) lands at once. A control the
//!   graph's editor holds (a node's mode and mix) is stored at once, a read
//!   answering it, and handed to the editor by the next update, which the
//!   node reads from its next block with no commit; a range is stored at
//!   once and reaches the lane, as a store into its range, with the update.
//! - **Atomic.** Every item is checked and every `modify` has run before
//!   anything lands, and the error's `at` is the first failing item. A batch is
//!   never one cut for the audio thread: two stores can straddle a block.
//! - **Reentrant.** The closure of a `modify` runs under the lock, on the
//!   caller's thread; a value call from inside it is
//!   [`ValueErrorKind::Reentrant`]. A panic in it lands nothing and leaves the
//!   lock usable.
//!
//! [`Session::set`]: crate::Session::set
//! [`Session::get`]: crate::Session::get
//! [`Session::update`]: crate::Session::update

mod base;
mod batch;
mod controls;
mod depth;
mod edit_log;
mod error;
mod gesture;
mod param;
#[cfg(feature = "plugin")]
mod plugin;
mod program;
mod range;
mod saved;
mod slot;
mod state;
mod target;
mod verbs;
mod watcher;

#[cfg(feature = "midi")]
pub(crate) use base::Steps;
pub(crate) use base::{BaseSlot, Bases};
pub use batch::{IntoAssign, IntoRead};
pub(crate) use controls::Write;
pub use controls::{GainTarget, GainTargetError, MixTarget, ModeTarget};
pub(crate) use depth::DepthSlot;
pub use depth::DepthTarget;
pub use edit_log::{EditLog, EditMark};
#[cfg(feature = "plugin")]
pub use error::{PluginParamRefusal, UnitError};
pub use error::{SetRefusal, ValueError, ValueErrorKind};
pub use gesture::Gesture;
pub use param::{ParamTarget, TargetInfo};
#[cfg(feature = "plugin")]
pub(crate) use plugin::{
    AutomationModeSlot, CarriedParam, MidiMapSlot, PluginEntry, TrackInfoSlot,
};
#[cfg(feature = "plugin")]
pub use plugin::{
    MidiMap, PluginAutomationModeTarget, PluginMidiMapTarget, PluginParamTarget,
    PluginTrackInfoTarget, PluginUnit,
};
pub(crate) use program::{
    Automation, AutomationCell, AutomationField, Declared, Edited, Field, MacroField, RateField,
};
pub use program::{AutomationTarget, CurveTarget, MacroTarget, PolarityTarget, RateTarget};
pub use range::RangeTarget;
pub(crate) use range::{Bounds, RangeSlot};
pub use saved::SavedValues;
#[cfg(feature = "midi-hardware")]
pub use slot::MidiPolicyTarget;
pub(crate) use slot::{
    Cell, ClickVolumeSlot, LoopCell, PublishedSlot, SettingSlot, SettingWord, SlotCell, SlotId,
    TempoCell, WordSlot,
};
pub use slot::{
    ClickTargets, ClickVolumeTarget, LoopTarget, MeterTarget, OutputPolicyTarget, SettingTarget,
    TempoTarget,
};
#[cfg(feature = "midi")]
pub use slot::{ClipEventsTarget, HarmonyTarget, MpeModeTarget, OutputLatencyTarget};
#[cfg(feature = "sampler")]
pub use slot::{DirectionTarget, LoopingTarget, SpeedTarget, WindowTarget};
pub use state::Values;
pub(crate) use state::{Outgoing, ValueState, ValueTable};
pub(crate) use target::target_builders;
pub use target::{
    AnyAssign, AnyTarget, Assign, Effective, Info, Modify, Modulatable, ScalarTarget, Target,
};
pub use watcher::{Change, ChangedBy, GestureId, GestureState, IntoTargets, Watchable, Watcher};
