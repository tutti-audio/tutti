//! What a failed `requestMIDIAccess` is, from the DOMException name the promise rejects with.
//!
//! The W3C Web MIDI API's `requestMIDIAccess()` steps name the failure: NotAllowedError
//! when the user or their security settings denied the options requested or the document
//! may not use the feature, AbortError when the page is going away, InvalidStateError
//! when the underlying system raised an error, and NotSupportedError otherwise. The
//! Permissions API's SecurityError (a context that may not ask) is a denial too.

use crate::error::MidiHardwareError;

/// The error a request that failed with the DOMException named `name` is.
pub(in crate::backend) fn from_dom_exception(name: &str) -> MidiHardwareError {
    match name {
        "NotAllowedError" | "SecurityError" => MidiHardwareError::WebMidiDenied,
        "NotSupportedError" => MidiHardwareError::WebMidiUnavailable,
        other => MidiHardwareError::WebMidiFailed {
            name: other.to_owned(),
        },
    }
}
