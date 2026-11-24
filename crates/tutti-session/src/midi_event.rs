//! What the session's MIDI reports from [`Session::update`](crate::Session::update):
//! [`MidiEvent`], nested under [`SessionEvent::Midi`](crate::SessionEvent::Midi).

use tutti_units::Losses;

/// One thing that happened in the session's MIDI, reported once.
///
/// A variant about one device carries its `MidiDeviceId`
/// as a field; a count over every input or the output has no device, and is its
/// own variant. What happens to a port (its device gone or back, what it
/// learned) is a [`ResourceChanged`](crate::PeerEvent::ResourceChanged) of
/// its handle. Reported in the order they were seen, from `Phase::Poll` and
/// `Phase::MidiOut`.
#[derive(Debug)]
#[non_exhaustive]
pub enum MidiEvent {
    /// A MIDI device was added to the list, in the order the OS reported.
    /// The session reads the devices in `Phase::Poll`; the
    /// list at any time is [`Midi::devices`](crate::Midi::devices).
    #[cfg(feature = "midi-hardware")]
    Added {
        /// What is listed of it; `info.id` is the device.
        info: tutti_midi_device::DeviceInfo,
    },
    /// A MIDI device left the list; `info` is its last.
    #[cfg(feature = "midi-hardware")]
    Removed {
        /// Its last listing; `info.id` is the device.
        info: tutti_midi_device::DeviceInfo,
    },
    /// A listed device's name or direction changed; its id did not.
    #[cfg(feature = "midi-hardware")]
    Changed {
        /// Its listing now; `info.id` is the device.
        info: tutti_midi_device::DeviceInfo,
    },
    /// A MIDI-CI peer's Profiles became known or changed, or the peer's Profiles
    /// were dropped with it (M2-101 section 7): the
    /// session is the Initiator with every peer that declares Profile
    /// Configuration, and reports each change once, from `Phase::Poll`. The
    /// Profiles now are the input's
    /// [`ListedPeer::profiles`](crate::ListedPeer::profiles), and a change
    /// is also a [`ResourceChanged`](crate::PeerEvent::ResourceChanged) of its
    /// input.
    #[cfg(feature = "midi-hardware")]
    CiProfiles {
        /// The device.
        device: tutti_midi_device::MidiDeviceId,
        /// The peer.
        peer: tutti_midi_hardware::PeerId,
        /// What happened.
        change: tutti_midi_hardware::ProfilesChange,
    },
    /// A MIDI-CI peer answered Process Inquiry, or the peer's Process Inquiry was
    /// dropped with it (M2-101 section 9): the session is the Initiator with every
    /// peer that declares Process Inquiry, and reports each change once, from
    /// `Phase::Poll`. The peers that can be asked for a MIDI Message Report are the
    /// input's
    /// [`ListedPeer::report`](crate::ListedPeer::report).
    #[cfg(feature = "midi-hardware")]
    CiProcess {
        /// The device.
        device: tutti_midi_device::MidiDeviceId,
        /// The peer.
        peer: tutti_midi_hardware::PeerId,
        /// What happened.
        change: tutti_midi_hardware::ProcessChange,
    },
    /// A tempo the session follows ([`Midi::follow_tempo`](crate::Midi::follow_tempo))
    /// was published: a Set Tempo of the followed Group, as a step in the
    /// project's tempo map at the start of the next block. A source that
    /// repeats the tempo already in force reports nothing.
    TempoFollowed {
        /// The tempo.
        tempo: tutti_units::Bpm,
        /// The beat the step was placed at, for a host to draw it. A publish
        /// that lands late takes effect from the block that picks it up, so
        /// the change is heard after this beat.
        beat: tutti_units::Beat,
    },
    /// A meter the session follows was published: a Set Time Signature of the
    /// followed Group, from the next bar line at or after the beat the transport
    /// was at.
    MeterFollowed {
        /// The time signature.
        signature: tutti_music::TimeSignature,
        /// The bar line it takes effect on.
        beat: tutti_units::Beat,
    },
    /// The session locked to the MIDI clock it follows
    /// ([`Midi::follow_clock`](crate::Midi::follow_clock)): Beat Clock after a
    /// quarter note of steady ticks, MTC after three consistent cycles of
    /// Quarter Frames.
    ClockLocked {
        /// The clock followed.
        kind: crate::ClockKind,
    },
    /// The session lost the MIDI clock it follows: a gap in Beat Clock's ticks,
    /// or silence past the dropout (half a second of Beat Clock, a quarter of a
    /// second of MTC), which also stops a transport the source had started.
    ClockUnlocked {
        /// The clock followed.
        kind: crate::ClockKind,
    },
    /// The transport was further from the clock the session follows than
    /// `BeatClockFollower::SLIP` (a quarter of a beat, Beat Clock) or
    /// `MtcFollower::SLIP_FRAMES` (two frames, MTC), more than a tempo trim
    /// is asked to close or a source's speed leaves: the transport locates to
    /// the source's position.
    ClockSlipped {
        /// The clock followed.
        kind: crate::ClockKind,
        /// The source's position less the transport's, when found: beats for
        /// Beat Clock, seconds for MTC.
        error: crate::ClockOffset,
    },
    /// The MTC the session follows runs backward: its Quarter Frames arrive in
    /// descending piece order. The transport has no negative rate, so the
    /// session stops it and unlocks until the Quarter Frames ascend again,
    /// when it locks afresh.
    ClockReversed {
        /// The clock followed.
        kind: crate::ClockKind,
    },
    /// The transport [refused](Losses::refused) commands a
    /// followed MIDI clock or Machine Control asked of it, this many in one
    /// update: its list of timed commands was full. Nothing was
    /// applied for them (the source says its position again with its next
    /// message), and each is logged.
    ClockCommandsRefused(Losses),
    /// A Machine Control command addressed to the session that it does not act
    /// on ([`Midi::follow_machine_control`](crate::Midi::follow_machine_control)):
    /// record, shuttle, eject, a READ, a speed other than play speed and the
    /// rest the transport has no counterpart for.
    MachineControlIgnored(tutti_midi_protocol::MmcCommand),
    /// A COMMAND SEGMENT addressed to the session that did not belong to its
    /// string, or a string that was not whole commands (RP-013 p.40): the string
    /// in progress was dropped and nothing of it was applied. No COMMAND ERROR
    /// is sent.
    MachineControlSegmentation(tutti_midi_runtime::SegmentFault),
    /// A follower the session follows with stopped: an `unfollow_*` call
    /// ([`UnfollowCause::Requested`](crate::UnfollowCause::Requested)) or the
    /// removal of the input node it read, whose tap can never speak again
    /// ([`UnfollowCause::InputGone`](crate::UnfollowCause::InputGone)). The
    /// follower is dropped, the project's own tempo and meter are published
    /// again for the maps it stepped (from the next block, at the beat the
    /// transport is then at, so the playhead does not jump), the transport keeps
    /// the motion it had, and the next `follow_tempo`, `follow_clock` or
    /// `follow_machine_control` is accepted.
    Unfollowed {
        /// The follower that ended.
        follower: crate::FollowKind,
        /// Why it ended.
        cause: crate::UnfollowCause,
    },
    /// A network endpoint's peer was invited, joined, left or failed to
    /// authenticate ([`Midi::open_network_client`](crate::Midi::open_network_client) and
    /// [`open_network_host`](crate::Midi::open_network_host)). There is none
    /// on a target with no UDP sockets.
    #[cfg(feature = "midi-network")]
    Network(crate::NetworkEvent),
    /// The OS watch for MIDI devices ended on its own (the reason is the
    /// message): the device list is frozen at what it last saw and no further
    /// hot-plug is reported. Reported once.
    #[cfg(feature = "midi-hardware")]
    WatchFailed(tutti_midi_hardware::WatchFailure),
}

#[cfg(feature = "midi-hardware")]
impl MidiEvent {
    /// The event a change of the device list is.
    pub(crate) fn of_device(event: tutti_midi_device::DeviceEvent) -> Self {
        use tutti_midi_device::DeviceEvent;
        match event {
            DeviceEvent::Added(info) => Self::Added { info },
            DeviceEvent::Removed(info) => Self::Removed { info },
            DeviceEvent::Changed(info) => Self::Changed { info },
        }
    }
}

impl From<MidiEvent> for crate::SessionEvent {
    fn from(event: MidiEvent) -> Self {
        Self::Midi(event)
    }
}
