//! A device's state as futures: the `fullState` of its State resource (M2-111),
//! got and set through the Property Exchange calls of its listed Resources.
//!
//! A call is queued when it is made, sent by the session's MIDI-CI task, and
//! resolved by that task when the device answers, with no
//! [`update`](crate::Session::update).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tutti_midi_ci::{
    CallError, MediaType, OpaqueBody, PeData, PeDataError, PeDataFault, PeResource, PeState, Place,
    RequestRefused, ResourceFailure, StateId,
};
use tutti_midi_device::SavedDevice;

use super::{GetProperty, ListedPeer, ListedResource, PeFailure, PeerReply, SetProperty};
use crate::{OtherPeer, PeerError, PeerState, SavedPeer};

/// Resolves when the device answers, on the session's MIDI-CI task, with no
/// [`update`](crate::Session::update); the session's view of the peer changes at
/// the next `update`, so the future may resolve before it.
///
/// A save of a device's state in flight: it resolves to the state the device
/// sent, or to why it did not. The task sends the call and reads the device's
/// answer, which resolves it. Dropping it drops the answer; a device or session
/// that goes away resolves it to [`PeerError::Disconnected`].
#[derive(Debug)]
#[must_use = "a save is answered only when it is awaited"]
pub struct SavePeerState {
    reply: GetProperty,
    /// The device the state is of: taken when the future resolves.
    device: Option<SavedDevice>,
}

/// # Panics
///
/// When polled again after it returned `Ready`.
impl Future for SavePeerState {
    type Output = Result<PeerState, PeerError<PeFailure>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Poll::Ready(got) = Pin::new(&mut self.reply).poll(cx) else {
            return Poll::Pending;
        };
        let device = self
            .device
            .take()
            .expect("a future is not polled once ready");
        Poll::Ready(got.and_then(|data| {
            let state = PeState::try_from(data).map_err(|other| {
                PeerError::Refused(PeFailure::Unreadable(ResourceFailure::Data(PeDataError {
                    place: Place::of("State"),
                    fault: PeDataFault::OfResource(other.resource()),
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

/// Resolves when the device answers, on the session's MIDI-CI task, with no
/// [`update`](crate::Session::update); the session's view of the peer changes at
/// the next `update`, so the future may resolve before it.
///
/// A restore of a device's state in flight: it resolves once the device has
/// answered its Set, or to why it did not. The task sends the call and reads
/// the answer, as for [`SavePeerState`].
#[derive(Debug)]
#[must_use = "a restore is answered only when it is awaited"]
pub struct RestorePeerState {
    reply: SetProperty,
}

impl Future for RestorePeerState {
    type Output = Result<(), PeerError<PeFailure>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.reply).poll(cx).map(|set| set.map(drop))
    }
}

impl ListedPeer {
    /// The State resource the peer's ResourceList names, or why there is none.
    fn state_resource(&self) -> Result<ListedResource, PeerError<PeFailure>> {
        self.resources()
            .and_then(|peer| peer.listed().find(|l| *l.resource() == PeResource::State))
            .ok_or(not_sent(RequestRefused::NotListed(PeResource::State)))
    }

    /// Saves the device's state: a Get of its State resource's `fullState`
    /// (M2-111 §3.2), resolving to the body as it came with the media type the
    /// device declared.
    ///
    /// The Get is queued as any peer call is, sent by the session's MIDI-CI task
    /// and resolved by it when the device answers, with no
    /// [`update`](crate::Session::update). A peer whose
    /// ResourceList has no State resource resolves to [`PeFailure::NotSent`], and
    /// one that is no longer listed to [`PeerError::Disconnected`].
    #[must_use = "a save is answered only when it is awaited"]
    pub fn save_state(&self) -> SavePeerState {
        let reply = self.state_resource().map_or_else(
            |refused| GetProperty::new(PeerReply::failed(refused)),
            |resource| {
                resource
                    .with_res_id(StateId::FULL_STATE.resource_id())
                    .fetch()
            },
        );
        SavePeerState {
            reply,
            device: Some(self.device.clone()),
        }
    }

    /// Restores `state` to the device: a Set of its State resource's
    /// `fullState` with the body and media type `state` holds, resolving once the
    /// device answers it. It is sent and resolved by the session's MIDI-CI task, as
    /// [`save_state`](Self::save_state) is.
    ///
    /// # Errors
    ///
    /// [`OtherPeer`] when `state` was saved from another device than this
    /// (by [`SavedPeer::is_same_peer`]): nothing is sent.
    #[must_use = "a restore is answered only when it is awaited"]
    pub fn restore_state(&self, state: PeerState) -> Result<RestorePeerState, OtherPeer> {
        state.check_for(&SavedPeer::Device(self.device.clone()))?;
        let reply = match self.state_resource() {
            Ok(resource) => {
                let body =
                    PeState::from_wire(Some(state.media_type()), state.bytes()).map_err(|e| {
                        PeerError::Refused(PeFailure::Unreadable(ResourceFailure::from(e)))
                    });
                match body {
                    Ok(body) => resource
                        .with_res_id(StateId::FULL_STATE.resource_id())
                        .write(PeData::State(body)),
                    Err(failed) => SetProperty::new(PeerReply::failed(failed)),
                }
            }
            Err(refused) => SetProperty::new(PeerReply::failed(refused)),
        };
        Ok(RestorePeerState { reply })
    }
}

/// A call that was not sent because the rules of the Resource refuse it.
fn not_sent(refused: RequestRefused) -> PeerError<PeFailure> {
    PeerError::Refused(PeFailure::NotSent(CallError::Refused(refused)))
}
