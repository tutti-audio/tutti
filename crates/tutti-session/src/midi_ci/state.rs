//! A device's state as requests: the `fullState` of its State resource (M2-111),
//! got and set through the Property Exchange calls of its listed Resources.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tutti_midi_ci::{
    CallError, MediaType, OpaqueBody, PeData, PeDataError, PeDataFault, PeEncoding, PeResource,
    PeState, PeStatus, Place, RequestRefused, ResourceFailure, ResourceGet, ResourceSet,
    ResourceTarget, StateId,
};
use tutti_midi_device::SavedDevice;
use tutti_midi_hardware::PropertyCall;

use super::{
    GetProperty, InputRequest, ListedPeer, ListedResource, PeFailure, Request, SetProperty, listing,
};
use crate::{OtherPeer, PeerError, PeerState, SavedPeer};

/// A save of a device's state in flight: it resolves to the state the device
/// sent, or to why it did not.
///
/// The device answers on tutti's MIDI thread. Dropping the future drops the
/// answer; a device or session that goes away resolves it to
/// [`PeerError::Disconnected`].
#[derive(Debug)]
#[must_use = "a save is answered only when it is awaited"]
pub struct SavePeerState {
    request: Request<PeData>,
    device: SavedDevice,
}

impl Future for SavePeerState {
    type Output = Result<PeerState, PeerError<PeFailure>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Poll::Ready(got) = Pin::new(&mut self.request).poll(cx) else {
            return Poll::Pending;
        };
        let device = self.device.clone();
        Poll::Ready(got.and_then(|data| {
            let state = PeState::try_from(data).map_err(|_| {
                PeerError::Refused(PeFailure::Unreadable(ResourceFailure::Data(PeDataError {
                    place: Place::of("State"),
                    fault: PeDataFault::WrongType("State"),
                })))
            })?;
            let (media_type, bytes) = match state.body() {
                OpaqueBody::Binary { media_type, bytes } => (media_type.clone(), bytes.clone()),
                OpaqueBody::Json(json) => (MediaType::JSON, json.encode()),
            };
            Ok(PeerState::new(SavedPeer::Device(device), media_type, bytes))
        }))
    }
}

/// A restore of a device's state in flight: it resolves once the device has
/// answered its Set, or to why it did not.
///
/// The device answers on tutti's MIDI thread, as for [`SavePeerState`].
#[derive(Debug)]
#[must_use = "a restore is answered only when it is awaited"]
pub struct RestorePeerState {
    request: Request<PeStatus>,
}

impl Future for RestorePeerState {
    type Output = Result<(), PeerError<PeFailure>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.request)
            .poll(cx)
            .map(|set| set.map(drop))
    }
}

impl ListedPeer {
    /// The State resource the peer's ResourceList names, if it does.
    fn state_resource(&self) -> Result<ListedResource, PeerError<PeFailure>> {
        self.resources()
            .and_then(|peer| peer.listed().find(|l| *l.resource() == PeResource::State))
            .ok_or(not_sent(RequestRefused::NotListed(PeResource::State)))
    }

    fn full_state() -> Result<ResourceTarget, RequestRefused> {
        ResourceTarget::entry(PeResource::State, StateId::FULL_STATE.resource_id())
    }

    /// Saves the device's state: a Get of its State resource's `fullState`
    /// (M2-111 §3.2), resolving to the body as it came with the media type the
    /// device declared.
    ///
    /// The Get is queued as any peer call is and the device answers on tutti's
    /// MIDI thread. A peer whose ResourceList has no State resource resolves to
    /// [`PeFailure::NotSent`].
    #[must_use = "a save is answered only when it is awaited"]
    pub fn save_state(&self) -> SavePeerState {
        let device = self.device.clone();
        let request = self
            .state_resource()
            .and_then(|resource| {
                let get = Self::full_state()
                    .map(|target| ResourceGet::new(target).encoded(encoding_of(&resource)))
                    .map_err(not_sent)?;
                Ok(resource.link.request::<GetProperty, _>(
                    resource.peer.ask(PropertyCall::Get(get)),
                    listing(&resource.entry),
                    InputRequest::Get,
                ))
            })
            .unwrap_or_else(Request::failed);
        SavePeerState { request, device }
    }

    /// Restores `state` to the device: a Set of its State resource's
    /// `fullState` with the body and media type `state` holds, resolving once the
    /// device answers it. The device answers on tutti's MIDI thread, as for
    /// [`save_state`](Self::save_state).
    ///
    /// # Errors
    ///
    /// [`OtherPeer`] when `state` was saved from another device than this
    /// (by [`SavedPeer::is_same_peer`]): nothing is sent.
    #[must_use = "a restore is answered only when it is awaited"]
    pub fn restore_state(&self, state: PeerState) -> Result<RestorePeerState, OtherPeer> {
        state.check_for(&SavedPeer::Device(self.device.clone()))?;
        let request = self
            .state_resource()
            .and_then(|resource| {
                let body =
                    PeState::from_wire(Some(state.media_type()), state.bytes()).map_err(|e| {
                        PeerError::Refused(PeFailure::Unreadable(ResourceFailure::from(e)))
                    })?;
                let set = Self::full_state()
                    .and_then(|target| ResourceSet::full(target, PeData::State(body)))
                    .map(|set| set.encoded(encoding_of(&resource)))
                    .map_err(not_sent)?;
                Ok(resource.link.request::<SetProperty, _>(
                    resource.peer.ask(PropertyCall::Set(set)),
                    listing(&resource.entry),
                    InputRequest::Set,
                ))
            })
            .unwrap_or_else(Request::failed);
        Ok(RestorePeerState { request })
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
fn not_sent(refused: RequestRefused) -> PeerError<PeFailure> {
    PeerError::Refused(PeFailure::NotSent(CallError::Refused(refused)))
}
