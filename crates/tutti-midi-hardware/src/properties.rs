//! The Property Exchange of a MIDI-CI peer, as the session drives it: the peers
//! that can be asked ([`PropertyPeer`]), the request ([`PropertyAsk`], of one
//! [`PropertyCall`]) and what happened ([`PropertyChange`]).
//!
//! The session is both ends (M2-103). As the Initiator it asks every peer that
//! declared Property Exchange for its capabilities when it is found, then for
//! its ResourceList, and lists those that answered. A request is made only from
//! such a handle, so an app cannot ask a peer that never declared the category
//! or never answered. The handle is a snapshot: the peer may be lost before the
//! request is sent, and that is refused ([`PropertyError::NoSuchPeer`]).
//!
//! As the Responder it serves a [`ResourceStore`]
//! ([`MidiSession::serve_properties`](crate::MidiSession::serve_properties)),
//! empty until the app gives one: it answers a peer's Capabilities Inquiry,
//! Get, Set and Subscription, and tells what a peer changed
//! ([`PropertyChange::Served`]). The app does not publish updates through the
//! session.
//!
//! A request started by a poll is [`PropertyChange::Started`] with the
//! [`CallId`] its [`ResourceEvent`]s name, in the order the requests were made.

use std::num::NonZeroU32;
use std::sync::Arc;

use tutti_midi_device::{
    CallError, CallId, DeviceId, Muid, ResourceEvent, ResourceGet, ResourceSet, ResponderEvent,
    SubscribeId,
};
use tutti_ump::MidiGroup;

/// A peer that declared Property Exchange and answered its Capabilities Inquiry:
/// the handle a [`PropertyAsk`] is made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyPeer {
    pub(crate) device: DeviceId,
    pub(crate) group: MidiGroup,
    pub(crate) muid: Muid,
}

impl PropertyPeer {
    /// The input device the peer was found on.
    #[must_use]
    pub const fn device(&self) -> &DeviceId {
        &self.device
    }

    /// The Group of the connection the peer answered on.
    #[must_use]
    pub const fn group(&self) -> MidiGroup {
        self.group
    }

    /// The peer's MUID.
    #[must_use]
    pub const fn muid(&self) -> Muid {
        self.muid
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
    /// A Get ([`ResourceInitiator::get`](tutti_midi_device::ResourceInitiator::get)).
    Get(ResourceGet),
    /// A Get of a whole List Resource, `page_size` entries at a time
    /// ([`ResourceInitiator::get_list`](tutti_midi_device::ResourceInitiator::get_list)).
    GetList {
        /// The Resource.
        get: ResourceGet,
        /// How many entries a page asks for.
        page_size: NonZeroU32,
    },
    /// A Set ([`ResourceInitiator::set`](tutti_midi_device::ResourceInitiator::set)).
    Set(ResourceSet),
    /// A Subscription to a Resource
    /// ([`ResourceInitiator::subscribe`](tutti_midi_device::ResourceInitiator::subscribe)).
    Subscribe(ResourceGet),
    /// The end of a Subscription
    /// ([`ResourceInitiator::unsubscribe`](tutti_midi_device::ResourceInitiator::unsubscribe)).
    Unsubscribe(SubscribeId),
}

/// A request to one peer's Property Exchange. Made only from a [`PropertyPeer`],
/// and it names the input device that handle was read from, so it cannot be sent
/// to another.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyAsk {
    pub(crate) peer: PropertyPeer,
    pub(crate) call: PropertyCall,
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
    /// its ResourceList, or the end of a call.
    Event(Box<ResourceEvent>),
    /// A [`PropertyAsk`] was started, and its events name this call.
    Started(CallId),
    /// A [`PropertyAsk`] was not started.
    NotSent {
        /// What was asked.
        call: Box<PropertyCall>,
        /// Why not.
        error: CallError,
    },
    /// The peer did something to what this device serves: a Set changed the
    /// store, or a Subscription began or ended.
    Served(ResponderEvent),
    /// The peer is gone (lost, or MIDI-CI ended): its Initiator and its
    /// Subscriptions went with it.
    Dropped,
}

/// A [`PropertyAsk`] that cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum PropertyError {
    /// The device the request was made from is no longer an open input.
    #[error("the device is not an open input")]
    NotOpen,
    /// The peer is not one found on that input that answered its Property
    /// Exchange Capabilities (it was lost, or never did).
    #[error("no such MIDI-CI peer with Property Exchange on that input")]
    NoSuchPeer,
}
