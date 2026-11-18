//! The OS MIDI devices through the session's [`Midi`] view (feature
//! `midi-hardware`): list them. A device is opened by a
//! `midi_input` or `midi_output` entry of the pool ([`midi_input`](crate::midi_input),
//! [`midi_output`](crate::midi_output)); what it says of itself is its
//! handle's [`Peer::info`](crate::Peer::info).

use tutti_midi_device::DeviceInfo;
use tutti_midi_device::MidiIdentity;
use tutti_midi_hardware::Presented;
use tutti_midi_runtime::MidiInputPorts;

use crate::midi::Midi;
use crate::{Clock, Policy, Session};

/// Who the session is on the wire: the identity it presents to every MIDI
/// device with an input and an output, set through
/// [`Session::policy`](crate::Session::policy).
///
/// `None`, the default, presents nothing: no MIDI-CI Device runs and a peer's
/// UMP Endpoint Discovery goes unanswered. `Some` runs MIDI-CI as that
/// identity on every input port with an output (a Discovery from each Function
/// Block of the device, under a MUID of its own) and answers a peer's UMP
/// Endpoint Discovery, Function Block Discovery and Stream Configuration
/// Request as that Endpoint (M2-104 §7.1), with one Function Block over the
/// Groups the session hears
/// ([`midi_groups`](crate::SessionBuilder::midi_groups)). The rest of the
/// Endpoint is the session's: UMP 1.1, the MIDI 1.0 and MIDI 2.0 Protocols, JR
/// Timestamps sent (a device output JR-stamps when its configuration has the
/// device receiving them) and received only when the session's input timing is
/// [`JitterReduction`](tutti_midi_hardware::InputTiming::JitterReduction), the
/// Function Block static. The Protocol and JR Timestamps the Endpoint reports
/// are the ones in force with the device, and a Stream Configuration Request is
/// granted within what it declares and moves them, JR asked off included.
///
/// A change applies in the next [`Session::update`](crate::Session::update):
/// the MIDI-CI Devices of the old identity send Invalidate MUID for their MUIDs
/// and the new identity starts Discovery under fresh ones; a change of the name
/// or the Product Instance Id alone announces the Endpoint again.
///
/// ```
/// use tutti_midi_device::EndpointName;
/// use tutti_midi_device::MidiIdentity;
/// use tutti_session::MidiPolicy;
/// use tutti_ump::{DeviceIdentity, ManufacturerId, SoftwareRevision, U7};
///
/// // A development identity: the manufacturer id MIDI 1.0 sets aside for
/// // non-commercial use; a released product registers its own.
/// let device = DeviceIdentity {
///     manufacturer: ManufacturerId::NON_COMMERCIAL,
///     family: [U7::of(1); 2],
///     model: [U7::of(2); 2],
///     revision: env!("CARGO_PKG_VERSION").parse::<SoftwareRevision>().unwrap(),
/// };
/// let policy = MidiPolicy {
///     identity: Some(MidiIdentity {
///         device,
///         name: EndpointName::new("My App").unwrap(),
///         instance: None,
///     }),
/// };
/// assert!(policy.identity.is_some());
/// assert_eq!(MidiPolicy::default().identity, None);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MidiPolicy {
    /// Who the session is; `None` presents nothing.
    pub identity: Option<MidiIdentity>,
}

/// Why a [`MidiPolicy`] was refused when it was set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum MidiPolicyError {
    /// The session has no OS MIDI layer, so there is no device to present an
    /// identity to.
    #[error(transparent)]
    NoHardware(#[from] crate::NoHardware),
}

impl Policy for MidiPolicy {
    type Handle<'a, F: tutti_buffers::Float> = MidiPolicyHandle<'a>;

    fn handle<K: crate::AppKey, C: Clock>(session: &mut Session<K, C>) -> MidiPolicyHandle<'_> {
        MidiPolicyHandle {
            attached: session.midi_io.is_some(),
            policy: &mut session.midi_policy,
        }
    }
}

/// Reads and changes a session's [`MidiPolicy`].
#[derive(Debug)]
pub struct MidiPolicyHandle<'a> {
    attached: bool,
    policy: &'a mut MidiPolicy,
}

impl MidiPolicyHandle<'_> {
    /// The policy in force, or the one set and not yet applied.
    #[must_use]
    pub fn get(&self) -> &MidiPolicy {
        self.policy
    }

    /// Sets the policy, applied by the next [`Session::update`](crate::Session::update).
    ///
    /// # Errors
    ///
    /// [`MidiPolicyError::NoHardware`] for an identity on a session with no OS
    /// MIDI layer; the policy in force is kept. Presenting nothing is always
    /// accepted.
    pub fn set(&mut self, policy: MidiPolicy) -> Result<(), MidiPolicyError> {
        if policy.identity.is_some() && !self.attached {
            return Err(crate::NoHardware.into());
        }
        *self.policy = policy;
        Ok(())
    }
}

impl<K: crate::AppKey, C: Clock<Float = f32>> Session<K, C> {
    /// Makes the OS MIDI layer present what the policy says, in
    /// [`Phase::Poll`](crate::Phase::Poll), before the devices are stepped.
    pub(crate) fn apply_midi_policy(&mut self) {
        let groups = self
            .midi
            .as_ref()
            .map_or(MidiInputPorts::FIRST_GROUP, |nodes| nodes.input_ports)
            .groups();
        let wanted = self
            .midi_policy
            .identity
            .map(|identity| Presented { identity, groups });
        if let Some(io) = &mut self.midi_io
            && io.presented() != wanted
        {
            io.present(wanted);
        }
    }
}

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
