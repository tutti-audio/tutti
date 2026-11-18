//! The VST3 value vocabulary: events, the info records a plugin reports,
//! the process context and the sample-format tag.

mod audio;
mod events;

mod info;
mod transport;

pub use audio::{K_SAMPLE_32_INT, K_SAMPLE_64_INT, ProcessMode, SampleWidth};
pub use events::{
    Chord, Data, Event, EventHeader, EventKind, ExpressionInt, ExpressionText, ExpressionValue,
    K_DATA_TYPE_MIDI_SYSEX, LegacyCcOut, MAX_EVENT_TEXT_LEN, NoteOff, NoteOn, Payload,
    PolyPressure, Scale, TextRef, UnknownEventKind,
};
// The text arena is the list's own storage; the conformance tests read its room.
#[cfg(feature = "conformance")]
#[doc(hidden)]
pub use events::TextArena;
#[cfg(not(feature = "conformance"))]
pub(crate) use events::TextArena;
pub use info::{
    AutomationStates, BusInfo, BusType, KeyswitchKind, NoteExpressionFlags, ParameterFlags,
    PhysicalUiType, PrefetchSupport, UnitId, Vst3KeyswitchInfo, Vst3NoteExpressionInfo,
    Vst3ParameterInfo, Vst3ProgramListInfo, Vst3UnitInfo,
};
pub use transport::{ProcessContextRequirements, to_process_context};
