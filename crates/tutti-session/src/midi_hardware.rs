//! The OS MIDI devices through the session's [`Midi`] view (feature
//! `midi-hardware`): list them. A port is begun by
//! [`Session::open_midi_input`](crate::Session::open_midi_input) or
//! [`Session::open_midi_output`](crate::Session::open_midi_output) and installed by
//! [`Session::apply`](crate::Session::apply); what it says
//! of itself is its handle's [`Peer::info`](crate::Peer::info).

use std::sync::Arc;

use tutti_buffers::Float;
use tutti_midi_ci::{ProfileOffer, ResourceStore};
use tutti_midi_device::DeviceInfo;
use tutti_midi_device::MidiIdentity;

use crate::Policy;
use crate::midi::Midi;

/// Who the session is on the wire and what it serves there: the identity it
/// presents to every MIDI device with an input and an output, and the Property
/// Exchange Resources it answers peers with, set through
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
/// `properties` is the store the MIDI-CI Devices serve as Property Exchange
/// Responders (M2-103) on every input port with an output; `None`, the default,
/// serves an empty store. A policy holds the store by `Arc`, and two policies
/// are equal when they hold the same `Arc` ([`Arc::ptr_eq`]), never by content:
/// writing the same store again is not a change, and a store that changed is a
/// new `Arc`, served afresh: a new `Arc` of equal content discards what peers'
/// Sets changed. A peer's Set changes the copy the devices serve, which
/// [`MidiSession::served_properties`](tutti_midi_hardware::MidiSession::served_properties)
/// reads, not the store in the policy.
///
/// `profiles` is the Profiles the MIDI-CI Devices offer as Profile Configuration
/// Responders (M2-101 §7, M2-102 §2.4 to §2.7) on the same ports; the empty
/// [`ProfileOffer`], the default, offers none. It is plain data compared by
/// content: a change is applied entry by entry, so an entry left as it was keeps
/// what a peer switched, and one the app changed takes the app's value and
/// drops the peer's switch ([`MidiSession::offer_profiles`](tutti_midi_hardware::MidiSession::offer_profiles)).
/// A peer's Set Profile On or Off is answered at once and reaches the app as a
/// [`ProfilesChange::Switched`](tutti_midi_hardware::ProfilesChange::Switched); it never
/// rewrites the policy. To force its own value back after a peer switched an entry,
/// the app changes that entry (any field) or toggles
/// [`fixed`](tutti_midi_ci::ProfileOfferEntry::fixed).
///
/// A change applies in the next [`Session::update`](crate::Session::update):
/// the MIDI-CI Devices of the old identity send Invalidate MUID for their MUIDs
/// and the new identity starts Discovery under fresh ones; a change of the name
/// or the Product Instance Id alone announces the Endpoint again.
///
/// ```
/// use std::sync::Arc;
/// use tutti_midi_ci::{ProfileOffer, ResourceStore};
/// use tutti_ump::EndpointName;
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
///     // The Resources peers can Get and Set; `ResourceStore::default()` has none.
///     properties: Some(Arc::new(ResourceStore::default())),
///     // The Profiles peers can switch; the empty offer has none.
///     profiles: ProfileOffer::default(),
/// };
/// assert!(policy.identity.is_some());
/// assert_eq!(policy.clone(), policy);
/// assert_eq!(MidiPolicy::default().identity, None);
/// assert!(MidiPolicy::default().properties.is_none());
/// assert!(MidiPolicy::default().profiles.is_empty());
/// ```
#[derive(Debug, Clone, Default)]
pub struct MidiPolicy {
    /// Who the session is; `None` presents nothing.
    pub identity: Option<MidiIdentity>,
    /// The Resources the session serves; `None` serves none.
    pub properties: Option<Arc<ResourceStore>>,
    /// The Profiles the session offers; empty offers none.
    pub profiles: ProfileOffer,
}

impl PartialEq for MidiPolicy {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
            && same_store(&self.properties, &other.properties)
            && self.profiles == other.profiles
    }
}

/// Whether `a` and `b` are the same store: both none, or the same `Arc`.
pub(crate) fn same_store(a: &Option<Arc<ResourceStore>>, b: &Option<Arc<ResourceStore>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

impl Eq for MidiPolicy {}

/// Why a [`MidiPolicy`] was refused when it was set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum MidiPolicyError {
    /// The session has no OS MIDI layer, so there is no device to present an
    /// identity to or serve Resources and Profiles on.
    #[error(transparent)]
    NoHardware(#[from] crate::NoHardware),
}

impl Policy for MidiPolicy {
    type Target = crate::MidiPolicyTarget;

    fn target() -> crate::MidiPolicyTarget {
        crate::MidiPolicyTarget::new()
    }
}

/// The cell of the session's MIDI policy: an identity, a store or an offer of
/// Profiles is refused when the session has no OS MIDI layer (`attached` is false).
pub(crate) fn policy_cell(attached: bool) -> std::sync::Arc<crate::value::Cell<MidiPolicy>> {
    crate::value::Cell::new(MidiPolicy::default(), move |policy| {
        let serves =
            policy.identity.is_some() || policy.properties.is_some() || !policy.profiles.is_empty();
        if serves && !attached {
            return Err(MidiPolicyError::from(crate::NoHardware).into());
        }
        Ok(())
    })
}

impl<F: Float> Midi<'_, F> {
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
}
