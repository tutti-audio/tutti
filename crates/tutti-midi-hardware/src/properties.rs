//! The Property Exchange of a MIDI-CI peer, as the session drives it: the peers
//! that can be asked ([`PropertyPeer`]), the request ([`PropertyAsk`], of one
//! [`PropertyCall`]) and what happened ([`PropertyChange`]).
//!
//! The session is both ends (M2-103). As the Initiator it asks every peer that
//! declared Property Exchange for its capabilities when it is found, then for
//! its ResourceList, and lists those that answered. A request is made only from
//! such a handle, so an app cannot ask a peer that never declared the category
//! or never answered. The handle is a snapshot: the peer may be lost before the
//! request is sent, and that is refused ([`PeerRequestError::NoSuchPeer`]).
//!
//! As the Responder it serves a [`ResourceStore`](tutti_midi_ci::ResourceStore)
//! ([`MidiSession::serve_properties`](crate::MidiSession::serve_properties)),
//! empty until the app gives one: it answers a peer's Capabilities Inquiry,
//! Get, Set and Subscription, and tells what a peer changed
//! ([`PropertyChange::Served`]). The app does not publish updates through the
//! session.
//!
//! A call's end is an [`Answer`](crate::Answer) ([`Answer::Got`](crate::Answer::Got),
//! [`Answer::Set`](crate::Answer::Set),
//! [`Answer::Subscribed`](crate::Answer::Subscribed)); a call the Initiator refuses
//! or a peer lost before it starts is [`Answer::PropertyNotSent`](crate::Answer::PropertyNotSent) or
//! [`Answer::PropertyLost`](crate::Answer::PropertyLost).

use std::num::NonZeroU32;
use std::sync::Arc;

use tutti_midi_ci::Muid;
use tutti_midi_ci::{
    PeResource, PeResourceList, ResourceEvent, ResourceGet, ResourceSet, ResponderEvent,
    SubscribeId,
};
use tutti_midi_device::MidiDeviceId;
use tutti_ump::MidiGroup;

use crate::peer::{PeerRef, PeerRequestError};

/// A peer that declared Property Exchange and answered its Capabilities Inquiry:
/// the handle a [`PropertyAsk`] is made from.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyPeer {
    pub(crate) at: PeerRef,
    pub(crate) resources: Arc<PeResourceList>,
}

impl PropertyPeer {
    /// The Resources the peer's ResourceList names, with how each is read, set
    /// and subscribed to; empty until the list has come.
    #[must_use]
    pub fn resources(&self) -> &PeResourceList {
        &self.resources
    }

    /// The input device the peer was found on.
    #[must_use]
    pub const fn device(&self) -> &MidiDeviceId {
        self.at.device()
    }

    /// The Group of the connection the peer answered on.
    #[must_use]
    pub const fn group(&self) -> MidiGroup {
        self.at.peer().group
    }

    /// The peer's MUID.
    #[must_use]
    pub const fn muid(&self) -> Muid {
        self.at.peer().muid
    }

    /// The peer, as every handle of MIDI-CI names one: the ResourceList the
    /// handle was read with does not matter to it.
    #[must_use]
    pub const fn at(&self) -> &PeerRef {
        &self.at
    }

    /// A request that the peer take `call`
    /// ([`MidiSession::ask_property`](crate::MidiSession::ask_property)).
    #[must_use]
    pub fn ask(&self, call: PropertyCall) -> PropertyAsk {
        PropertyAsk {
            peer: self.clone(),
            call,
        }
    }
}

/// What is asked of a peer's Property Exchange.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PropertyCall {
    /// A Get ([`ResourceInitiator::get`](tutti_midi_ci::ResourceInitiator::get)).
    Get(ResourceGet),
    /// A Get of a whole List Resource, `page_size` entries at a time
    /// ([`ResourceInitiator::get_list`](tutti_midi_ci::ResourceInitiator::get_list)).
    GetList {
        /// The Resource.
        get: ResourceGet,
        /// How many entries a page asks for.
        page_size: NonZeroU32,
    },
    /// A Set ([`ResourceInitiator::set`](tutti_midi_ci::ResourceInitiator::set)).
    Set(ResourceSet),
    /// A Subscription to a Resource
    /// ([`ResourceInitiator::subscribe`](tutti_midi_ci::ResourceInitiator::subscribe)).
    Subscribe(ResourceGet),
    /// The end of a Subscription
    /// ([`ResourceInitiator::unsubscribe`](tutti_midi_ci::ResourceInitiator::unsubscribe)).
    Unsubscribe(SubscribeId),
}

impl PropertyCall {
    /// The Resource the call is of, `None` for the end of a Subscription, which
    /// names the Subscription.
    #[must_use]
    pub fn resource(&self) -> Option<&PeResource> {
        match self {
            Self::Get(get) | Self::Subscribe(get) | Self::GetList { get, .. } => {
                Some(get.target().resource())
            }
            Self::Set(set) => Some(set.target().resource()),
            Self::Unsubscribe(_) => None,
        }
    }
}

/// A request to one peer's Property Exchange. Made only from a [`PropertyPeer`],
/// and it names the input device that handle was read from, so it cannot be sent
/// to another.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyAsk {
    pub(crate) peer: PropertyPeer,
    pub(crate) call: PropertyCall,
}

impl PropertyAsk {
    /// The peer asked.
    #[must_use]
    pub const fn peer(&self) -> &PropertyPeer {
        &self.peer
    }

    /// What is asked of it.
    #[must_use]
    pub const fn call(&self) -> &PropertyCall {
        &self.call
    }

    /// Checks the peer is one of `peers`, the input's peers that answered their
    /// Capabilities.
    ///
    /// # Errors
    ///
    /// [`PeerRequestError::NoSuchPeer`] when it is not.
    pub(crate) fn check<'a>(
        &self,
        peers: impl IntoIterator<Item = &'a PropertyPeer>,
    ) -> Result<(), PeerRequestError> {
        self.peer.at.find(peers, |peer| &peer.at).map(|_| ())
    }
}

/// The peers of one input that can be asked.
pub(crate) type Snapshot = Arc<[PropertyPeer]>;

/// A change in the Property Exchange of one peer, once each from
/// [`MidiSession::poll`](crate::MidiSession::poll); the [`PeerId`](crate::PeerId)
/// beside it in [`Polled::Property`](crate::Polled::Property) names the peer.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PropertyChange {
    /// The Initiator reports: the peer's Capabilities arrived (and with them the
    /// peer is in [`MidiSession::property_peers`](crate::MidiSession::property_peers)),
    /// its ResourceList, or what a Subscription holds (an update, the end the peer
    /// made of it). A call's end is an
    /// [`Answer`](crate::Answer), not an event.
    Event(Box<ResourceEvent>),
    /// The peer did something to what this device serves: a Set changed the
    /// store, or a Subscription began or ended.
    Served(ResponderEvent),
    /// The peer is gone (lost, or MIDI-CI ended): its Initiator and its
    /// Subscriptions went with it.
    Dropped,
}
