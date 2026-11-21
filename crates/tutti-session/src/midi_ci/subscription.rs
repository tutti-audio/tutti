//! A Subscription to a peer's Resource (M2-103 §11): the lease an app holds and
//! the record the session keeps current under it.

use std::sync::{Arc, Mutex, Weak};

use tutti_midi_ci::{PeData, PeResource, SubscribeId};
use tutti_midi_hardware::{PeerId, PropertyAsk, PropertyCall, PropertyPeer};
use tutti_rt::LockExt;

use super::reply::peer_future;
use super::{PeFailure, PortLink, ResourcePeer, listed_peer};
use crate::midi_port::InputRequest;
use crate::peer::sealed::Sealed;
use crate::{Presence, Resource, ResourceFact, ResourceId, SessionEvent};

peer_future!(
    /// A Subscription to a peer's Resource in flight: it resolves to the lease
    /// once the peer accepts. The first Property Data follows it into
    /// [`ResourcePeer::property`](crate::ResourcePeer::property).
    Subscribe => Subscription, PeFailure, "a Subscription is answered only when it is awaited"
);

/// Why a [`Subscription`] is not live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SubscriptionCause {
    /// The peer ended it (M2-103 §11.3), or went itself: it ended MIDI-CI, which
    /// takes its Subscriptions with it.
    EndedByPeer,
    /// The port it was made on closed: the device was unplugged, or the entry was
    /// released.
    PortLost,
}

/// What the lease and the session's record of it share.
#[derive(Debug)]
pub(crate) struct Lease {
    id: ResourceId,
    presence: Mutex<Presence<SubscriptionCause>>,
}

impl Lease {
    fn presence(&self) -> Presence<SubscriptionCause> {
        *self.presence.lock_recover()
    }

    /// Reads [`Presence::Lost`] with `cause`, once: whether it changed.
    fn lose(&self, cause: SubscriptionCause) -> bool {
        let mut presence = self.presence.lock_recover();
        let live = presence.is_live();
        if live {
            *presence = Presence::Lost(cause);
        }
        live
    }
}

/// A Subscription to one Resource of a peer, held by the app (M2-103 §11).
///
/// While it is [`Live`](Presence::Live) the session keeps the Resource's Property
/// Data current in the peer's record
/// ([`ResourcePeer::property`](crate::ResourcePeer::property)): a `full` update
/// replaces it, a `partial` one is merged into it, and a `notify`, or an update
/// whose Property Data does not decode, makes the session Get the Resource. Each
/// change is a [`ResourceChanged`](SessionEvent::ResourceChanged) of the port with
/// [`ResourceFact::Info`].
///
/// Dropping it ends the Subscription with the peer: the request goes through the
/// port's queue like any other, so the drop never waits.
pub struct Subscription {
    lease: Arc<Lease>,
    link: PortLink,
    peer: PropertyPeer,
    resource: PeResource,
    subscribe_id: SubscribeId,
}

impl Subscription {
    /// Whether the peer is still sending updates: [`Presence::Live`], or
    /// [`Presence::Lost`] with why. Its end is a
    /// [`ResourceChanged`](SessionEvent::ResourceChanged) with this lease's
    /// [`id`](Resource::id) and [`ResourceFact::Presence`]. Never blocks.
    #[must_use]
    pub fn presence(&self) -> Presence<SubscriptionCause> {
        self.lease.presence()
    }

    /// The Resource subscribed to.
    #[must_use]
    pub const fn resource(&self) -> &PeResource {
        &self.resource
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("id", &self.lease.id)
            .field("resource", &self.resource)
            .field("presence", &self.presence())
            .finish_non_exhaustive()
    }
}

impl Sealed for Subscription {}

impl Resource for Subscription {
    fn id(&self) -> ResourceId {
        self.lease.id
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if self.lease.presence().is_live() {
            let ask = self
                .peer
                .ask(PropertyCall::Unsubscribe(self.subscribe_id.clone()));
            // Nothing waits for the peer's reply.
            self.link
                .send(ask, listed_peer, |ask, ()| InputRequest::Unsubscribe(ask));
        }
    }
}

impl Subscription {
    pub(crate) fn new(
        id: ResourceId,
        link: PortLink,
        ask: &PropertyAsk,
        resource: PeResource,
        subscribe_id: SubscribeId,
    ) -> (Self, Weak<Lease>) {
        let lease = Arc::new(Lease {
            id,
            presence: Mutex::new(Presence::Live),
        });
        let weak = Arc::downgrade(&lease);
        (
            Self {
                lease,
                link,
                peer: ask.peer().clone(),
                resource,
                subscribe_id,
            },
            weak,
        )
    }
}

/// One Subscription as the session records it: the lease it answers to and the
/// Property Data it holds.
struct Held {
    lease: Weak<Lease>,
    peer: PeerId,
    resource: PeResource,
    subscribe_id: SubscribeId,
    data: Option<PeData>,
}

/// The Property Data a port's Subscriptions hold, kept current from the peers'
/// updates.
#[derive(Default)]
pub(crate) struct Subscriptions {
    held: Vec<Held>,
    /// The notices of leases that ended, for the session to report.
    notices: Vec<SessionEvent>,
    /// Whether the record changed since it was last read for the port's info.
    dirty: bool,
}

/// The Property Data of a peer's Resources, as its record reads them.
pub(crate) type Bodies = Arc<[(PeResource, PeData)]>;

impl Subscriptions {
    pub(crate) fn started(
        &mut self,
        peer: PeerId,
        lease: Weak<Lease>,
        resource: PeResource,
        subscribe_id: SubscribeId,
    ) {
        self.held.push(Held {
            lease,
            peer,
            resource,
            subscribe_id,
            data: None,
        });
    }

    fn find(&mut self, peer: PeerId, id: &SubscribeId) -> Option<&mut Held> {
        self.held
            .iter_mut()
            .find(|h| h.peer == peer && h.subscribe_id == *id)
    }

    /// The Property Data of the Subscription `id` is `data` now.
    pub(crate) fn update(&mut self, peer: PeerId, id: &SubscribeId, data: Option<PeData>) {
        if let Some(held) = self.find(peer, id) {
            held.data = data;
            self.dirty = true;
        }
    }

    /// The Subscription `id` ended with the peer's end of it, or by this side's own.
    pub(crate) fn remove(&mut self, peer: PeerId, id: &SubscribeId) -> Option<Arc<Lease>> {
        let at = self
            .held
            .iter()
            .position(|h| h.peer == peer && h.subscribe_id == *id)?;
        self.dirty = true;
        self.held.remove(at).lease.upgrade()
    }

    /// The Subscription `id` was ended by the peer: its lease reads it, once.
    pub(crate) fn ended_by_peer(&mut self, peer: PeerId, id: &SubscribeId) {
        if let Some(lease) = self.remove(peer, id) {
            lose(&lease, SubscriptionCause::EndedByPeer, &mut self.notices);
        }
    }

    /// Every Subscription of `peer` (all when `None`) ends as `cause`.
    pub(crate) fn lose(&mut self, peer: Option<PeerId>, cause: SubscriptionCause) {
        let (gone, kept) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|h| peer.is_none_or(|p| h.peer == p));
        self.held = kept;
        self.dirty |= !gone.is_empty();
        for held in gone {
            if let Some(lease) = held.lease.upgrade() {
                lose(&lease, cause, &mut self.notices);
            }
        }
    }

    /// The notices of the leases that ended since the last call.
    pub(crate) fn take_notices(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.notices)
    }

    /// Forgets what the dropped leases held.
    pub(crate) fn prune(&mut self) {
        let before = self.held.len();
        self.held.retain(|h| h.lease.strong_count() > 0);
        self.dirty |= self.held.len() != before;
    }

    /// Whether the record changed since the last call.
    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// What `peer`'s Resources hold.
    pub(crate) fn bodies(&self, peer: PeerId) -> Bodies {
        self.held
            .iter()
            .filter(|h| h.peer == peer && h.lease.strong_count() > 0)
            .filter_map(|h| Some((h.resource.clone(), h.data.clone()?)))
            .collect()
    }
}

fn lose(lease: &Lease, cause: SubscriptionCause, out: &mut Vec<SessionEvent>) {
    if lease.lose(cause) {
        out.push(SessionEvent::ResourceChanged {
            id: lease.id,
            fact: ResourceFact::Presence,
        });
    }
}

impl ResourcePeer {
    /// The Property Data of `resource` as a live [`Subscription`] keeps it, or
    /// `None` while none holds it (before its first Get, after it ended).
    ///
    /// After a `notify`, or an update that does not decode, the session Gets the
    /// Resource, and this reads the data held before until that Get resolves or
    /// times out (an `Updated` replaces it, a failed Get clears it): the window is
    /// bounded by the Get's timeout.
    #[must_use]
    pub fn property(&self, resource: &super::ListedResource) -> Option<&PeData> {
        self.held
            .iter()
            .rev()
            .find(|(held, _)| held == resource.resource())
            .map(|(_, data)| data)
    }
}
