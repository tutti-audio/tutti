//! A device's state as jobs: the `fullState` of its State resource (M2-111),
//! got and set through the Property Exchange calls of its listed Resources.

use tutti_midi_ci::{
    CallError, MediaType, OpaqueBody, PeData, PeEncoding, PeResource, PeState, RequestRefused,
    ResourceFailure, ResourceGet, ResourceSet, ResourceTarget, StateId,
};
use tutti_midi_hardware::PropertyCall;
use tutti_plugin_types::StateUse;

use super::{InputRequest, ListedPeer, ListedResource, PeFailure, SetProperty, listing};
use crate::{
    Job, OtherPeer, PeerError, PeerState, RestoreState, SaveState, SavedPeer, StateJobFailed,
};

/// The id of the state that holds the whole of a device's settings (M2-111
/// Appendix A).
const FULL_STATE: &str = "fullState";

impl ListedPeer {
    /// The State resource the peer's ResourceList names, if it does.
    fn state_resource(&self) -> Result<ListedResource, StateJobFailed> {
        self.resources()
            .and_then(|peer| peer.listed().find(|l| *l.resource() == PeResource::State))
            .ok_or(not_sent(RequestRefused::NotListed(PeResource::State)))
    }

    fn full_state() -> Result<ResourceTarget, RequestRefused> {
        let id = StateId::new(FULL_STATE).unwrap_or_else(|| unreachable!("a state id"));
        ResourceTarget::entry(PeResource::State, id.resource_id())
    }

    /// Saves the device's state: a Get of its State resource's `fullState`
    /// (M2-111 §3.2), landing the body as it came with the media type the device
    /// declared. A device's state is the device's alone, so `state_use` does not
    /// change it. The Get is queued as any peer call is and lands in a later
    /// [`update`](crate::Session::update); nothing waits on the reply.
    ///
    /// A peer whose ResourceList has no State resource ends the job as
    /// [`StateJobFailed::Device`] with [`PeFailure::NotSent`].
    pub fn save_state(&self, state_use: StateUse) -> Job<SaveState> {
        // A device has one state; the use selects among a plugin's.
        let _ = state_use;
        let resource = match self.state_resource() {
            Ok(resource) => resource,
            Err(failed) => return self.link.failed(failed),
        };
        let device = self.device.clone();
        let encoding = encoding_of(&resource);
        let get = Self::full_state().map(|target| ResourceGet::new(target).encoded(encoding));
        let get = match get {
            Ok(get) => get,
            Err(refused) => return self.link.failed(not_sent(refused)),
        };
        resource.link.submit_mapped(
            resource.peer.ask(PropertyCall::Get(get)),
            listing(&resource.entry),
            InputRequest::Get,
            move |got: Result<PeData, PeerError<PeFailure>>| match got {
                Ok(PeData::State(state)) => Ok(match state.body() {
                    OpaqueBody::Binary { media_type, bytes } => {
                        PeerState::new(SavedPeer::Device(device), media_type.clone(), bytes.clone())
                    }
                    OpaqueBody::Json(json) => {
                        PeerState::new(SavedPeer::Device(device), MediaType::JSON, json.encode())
                    }
                }),
                Ok(_) => unreachable!("a Get of State answers PeData::State"),
                Err(failed) => Err(StateJobFailed::Device(failed)),
            },
        )
    }

    /// Restores `state` to the device: a Set of its State resource's
    /// `fullState` with the body and media type `state` holds. The job lands
    /// once the device answers its Set, as
    /// [`save_state`](Self::save_state)'s does.
    ///
    /// # Errors
    ///
    /// [`OtherPeer`] when `state` was saved from another device than this
    /// (by [`SavedPeer::is_same_peer`]): nothing is sent.
    pub fn restore_state(&self, state: &PeerState) -> Result<Job<RestoreState>, OtherPeer> {
        state.check_for(&SavedPeer::Device(self.device.clone()))?;
        let resource = match self.state_resource() {
            Ok(resource) => resource,
            Err(failed) => return Ok(self.link.failed(failed)),
        };
        let encoding = encoding_of(&resource);
        let set = PeState::from_wire(Some(state.media_type()), state.bytes())
            .map_err(|e| PeFailure::Unreadable(ResourceFailure::from(e)))
            .map_err(|failure| StateJobFailed::Device(PeerError::Refused(failure)))
            .and_then(|body| {
                Self::full_state()
                    .and_then(|target| ResourceSet::full(target, PeData::State(body)))
                    .map(|set| set.encoded(encoding))
                    .map_err(not_sent)
            });
        let set = match set {
            Ok(set) => set,
            Err(failed) => return Ok(self.link.failed(failed)),
        };
        Ok(resource.link.submit_mapped(
            resource.peer.ask(PropertyCall::Set(set)),
            listing(&resource.entry),
            InputRequest::Set,
            |set: Result<<SetProperty as crate::JobKind>::Output, PeerError<PeFailure>>| match set {
                Ok(_) => Ok(()),
                Err(failed) => Err(StateJobFailed::Device(failed)),
            },
        ))
    }
}

/// The encoding a call of the State resource is made in: ASCII where its entry
/// takes it, the first it lists otherwise.
fn encoding_of(resource: &ListedResource) -> PeEncoding {
    let listed = resource.entry().encodings();
    if listed.contains(&PeEncoding::Ascii) {
        PeEncoding::Ascii
    } else {
        listed.first().copied().unwrap_or(PeEncoding::Ascii)
    }
}

/// A call that was not sent because the rules of the Resource refuse it.
fn not_sent(refused: RequestRefused) -> StateJobFailed {
    StateJobFailed::Device(PeerError::Refused(PeFailure::NotSent(CallError::Refused(
        refused,
    ))))
}
