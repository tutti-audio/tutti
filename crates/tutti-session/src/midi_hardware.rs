//! The OS MIDI devices through the session's [`Midi`] view (feature
//! `midi-hardware`): list them. A device is opened by a
//! `midi_input` or `midi_output` entry of the pool ([`midi_input`](crate::midi_input),
//! [`midi_output`](crate::midi_output)); what it says of itself is its
//! handle's [`Peer::info`](crate::Peer::info).

use tutti_midi_device::DeviceInfo;

use crate::midi::Midi;

impl Midi<'_> {
    /// The MIDI devices present now, inputs and outputs, ordered by id. Empty on
    /// a session with no OS MIDI layer
    /// ([`SessionBuilder::midi_hardware`](crate::SessionBuilder::midi_hardware)).
    /// A device that came or went since is reported as a
    /// [`MidiReport`](crate::MidiReport) from `Session::update`.
    #[must_use]
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.io
            .as_deref()
            .map_or_else(Vec::new, |io| io.devices().list())
    }
}
