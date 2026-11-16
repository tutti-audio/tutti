//! The OS MIDI devices through the session's [`Midi`] view (feature
//! `midi-hardware`): list them, and end MIDI-CI. A device
//! `midi_input` or `midi_output` entry of the pool ([`midi_input`](crate::midi_input),
//! [`midi_output`](crate::midi_output)); what it says of itself is its
//! handle's [`Peer::info`](crate::Peer::info).

use tutti_midi_device::DeviceInfo;

use crate::MidiError;
use crate::midi::Midi;

type Result<T> = std::result::Result<T, MidiError>;

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

    /// Turns MIDI-CI off: every running device sends an Invalidate MUID for its
    /// own MUID at the next update (M2-101 §5.9: a shutdown is a reason), and
    /// its peers are reported lost.
    ///
    /// # Errors
    ///
    /// [`MidiPortError::NoHardware`](crate::MidiPortError::NoHardware) (as [`MidiError::Port`]) on a session with no OS MIDI layer.
    pub fn disable_ci(&mut self) -> Result<()> {
        self.hardware_mut()?.disable_ci();
        Ok(())
    }

    fn hardware_mut(&mut self) -> Result<&mut tutti_midi_hardware::MidiSession> {
        self.io
            .as_deref_mut()
            .ok_or(crate::MidiPortError::NoHardware.into())
    }
}
