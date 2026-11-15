//! The OS MIDI devices through the session's [`Midi`] view (feature
//! `midi-hardware`): list them, and run MIDI-CI. A device is opened by a
//! `midi_input` or `midi_output` entry of the pool ([`midi_input`](crate::midi_input),
//! [`midi_output`](crate::midi_output)); what it says of itself is its
//! handle's [`Peer::info`](crate::Peer::info).

use tutti_midi_hardware::DeviceInfo;
use tutti_ump::DeviceIdentity;

use crate::MidiError;
use crate::midi::Midi;

type Result<T> = std::result::Result<T, MidiError>;

impl Midi<'_> {
    /// The MIDI devices present now, inputs and outputs, ordered by id. Empty on
    /// a session with no OS MIDI layer
    /// ([`SessionBuilder::midi_hardware`](crate::SessionBuilder::midi_hardware)).
    /// A device that came or went since is reported as a
    /// [`MidiEvent`](crate::MidiEvent) from `Session::update`.
    #[must_use]
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.io
            .as_deref()
            .map_or_else(Vec::new, |io| io.devices().list())
    }

    /// Turns MIDI-CI on, announcing `identity` in Discovery. **It is off by
    /// default**: a Discovery carries a System Exclusive manufacturer id, tutti
    /// has none, and MIDI 1.0's `7D` is "not to be used on any product released
    /// to the public", so the app gives the id it registered (an identity
    /// under `ManufacturerId::NON_COMMERCIAL` is for development and research
    /// only). It reaches the inputs already open (at the next update, after any
    /// Identity Request in progress) and later ones. Called again with the
    /// identity in use it changes nothing; with another, the running devices end
    /// as [`disable_ci`](Self::disable_ci) ends them and start again under a new
    /// MUID. Found peers are
    /// [`MidiInputInfo::peers`](crate::MidiInputInfo::peers).
    ///
    /// # Errors
    ///
    /// [`MidiPortError::NoHardware`](crate::MidiPortError::NoHardware) (as [`MidiError::Port`]) on a session with no OS MIDI layer.
    pub fn enable_ci(&mut self, identity: DeviceIdentity) -> Result<()> {
        self.hardware_mut()?.enable_ci(identity);
        Ok(())
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
