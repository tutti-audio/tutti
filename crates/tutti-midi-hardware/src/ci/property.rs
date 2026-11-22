//! Property Exchange on a member device: an Initiator for each peer that declares
//! it and the one Responder that serves the device's store (M2-103). The Initiators
//! and the Responder are stepped by the member's messages and time, as Profile
//! Configuration and Process Inquiry are, and what they send goes in the same
//! batch.

use tutti_platform::Instant;

use tutti_midi_ci::Muid;
use tutti_midi_ci::{
    BodyLimit, BufferLimit, ChunkPosition, ChunkTotal, CiCategories, CiMessage, Decoded,
    PeCapabilities, PeCiMessage, PeHeader, PeMessageKind, PeReassemblyLimits, PeSettings,
    PeSubscription, ResourceAction, ResourceEvent, ResourceInitiator, ResourceResponder,
    ResourceStore, ResponderAction, ResponderSettings, SimultaneousRequests, StreamLimit,
};
use tutti_ump::MidiGroup;

use super::{CiChanges, Member, Running, expire_all, hear_all};
use crate::answer::Answer;
use crate::properties::{PropertyAsk, PropertyCall, PropertyChange};

/// The requests of a peer this device takes at once.
const REQUESTS: SimultaneousRequests = SimultaneousRequests::of(4);

/// What the inquiries and replies being received may buffer.
const REASSEMBLY: PeReassemblyLimits = PeReassemblyLimits {
    streams: StreamLimit::of(8),
    bytes: BufferLimit::of(1 << 17),
};

/// The most a decoded body may be.
const BODY: BodyLimit = BodyLimit::of(1 << 16);

/// What the Responder holds.
const RESPONDER: ResponderSettings = ResponderSettings {
    capabilities: PeCapabilities::new(REQUESTS),
    reassembly: REASSEMBLY,
    body_limit: BODY,
};

/// The Responder of a device with MUID `own`.
pub(super) fn responder(own: Muid) -> ResourceResponder {
    ResourceResponder::new(own, RESPONDER)
}

/// Which side of a device a Property Exchange message is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// The Responder and every Initiator: each keeps or ignores it.
    Both,
    /// The Initiators alone.
    Initiators,
    /// The Responder alone.
    Responder,
}

impl Member {
    /// Whether `muid` declared Property Exchange in its Discovery Reply.
    fn declares_property_exchange(&self, muid: Muid) -> bool {
        self.device.peer(muid).is_some_and(|peer| {
            peer.discovery()
                .categories
                .contains(CiCategories::PROPERTY_EXCHANGE)
        })
    }

    /// A found peer that declared Property Exchange is served, and asked for its
    /// Capabilities (M2-101 §8.5): by a new Initiator, or, for one that answered
    /// Discovery again, by the old one asking again.
    pub(super) fn initiate_property(
        &mut self,
        muid: Muid,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        if !self.declares_property_exchange(muid) {
            return;
        }
        let Some(max_sysex) = self
            .device
            .peer(muid)
            .map(|peer| peer.discovery().max_sysex_size)
        else {
            return;
        };
        self.server.accept(muid, max_sysex);
        let actions = match self.properties.get_mut(&muid) {
            Some(initiator) => initiator.ask_capabilities(now),
            None => {
                let settings = PeSettings {
                    capabilities: PeCapabilities::new(REQUESTS),
                    peer_max_sysex: max_sysex,
                    reassembly: REASSEMBLY,
                    body_limit: BODY,
                };
                let (initiator, first) =
                    ResourceInitiator::new(self.device.muid(), muid, settings, now);
                self.properties.insert(muid, initiator);
                first
            }
        };
        self.carry_out_property(muid, actions, sends, changes);
    }

    /// A peer is lost, or the device's MUID changed: what is held for the peer
    /// goes.
    pub(super) fn forget_property(&mut self, muid: Muid, changes: &mut CiChanges) {
        self.server.forget(muid);
        self.updates.retain(|(from, _)| *from != muid);
        if self.properties.remove(&muid).is_some() {
            changes
                .properties
                .push((self.peer_id(muid), PropertyChange::Dropped));
        }
    }

    /// The device's MUID changed: the Responder is for the new one, is handed
    /// the session's store on each call, and holds no Subscription.
    pub(super) fn renew_property(&mut self, new: Muid) {
        self.server = responder(new);
    }

    /// Hands the Responder and every Initiator the message, which is theirs to
    /// keep or ignore.
    pub(super) fn route_property(
        &mut self,
        store: &mut ResourceStore,
        decoded: &Decoded,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        // The Management core leaves Property Exchange inquiries to the Responder, so
        // one from a peer it does not serve is refused here (M2-101 §5.11).
        if let Decoded::Message(message) = decoded
            && !self.server.serves(message.header().source)
            && let Some(nak) = self.device.refuse_property_exchange(decoded)
        {
            sends.push((self.group, nak));
        }
        let route = self.route_of(decoded);
        if route != Route::Initiators {
            let served = self.server.on_message(store, decoded, now);
            self.carry_out_served(served, sends, changes);
        }
        let heard: Vec<_> = if route == Route::Responder {
            Vec::new()
        } else {
            hear_all(&mut self.properties, decoded, now)
        };
        for (muid, actions) in heard {
            self.carry_out_property(muid, actions, sends, changes);
        }
    }

    /// Who a message is for. A Subscription inquiry is the Initiator's when it is
    /// an update, which only a Responder sends (M2-103 §11.1), or the end of a
    /// Subscription the Initiator of its peer holds, and every chunk of such a
    /// message follows its first; any other is the Responder's, which serves what
    /// peers ask of the store. Each side would refuse what is the other's.
    fn route_of(&mut self, decoded: &Decoded) -> Route {
        let Decoded::Message(CiMessage::PropertyExchange {
            header,
            message: PeCiMessage::Chunk { kind, chunk },
            ..
        }) = decoded
        else {
            return Route::Both;
        };
        if *kind != PeMessageKind::SUBSCRIPTION_INQUIRY {
            return Route::Both;
        }
        let source = header.source;
        let key = (source, chunk.request_id());
        let owned = if chunk.header().is_empty() {
            self.updates.contains(&key)
        } else {
            match PeHeader::try_from(chunk.header()) {
                Ok(PeHeader::Subscription(PeSubscription::Update { .. })) => true,
                Ok(PeHeader::Subscription(PeSubscription::End { subscribe_id, .. })) => self
                    .properties
                    .get(&source)
                    .is_some_and(|i| i.subscriptions().any(|(id, ..)| *id == subscribe_id)),
                _ => false,
            }
        };
        let last = matches!(
            (chunk.position(), chunk.total()),
            (ChunkPosition::Number(n), ChunkTotal::Known(total)) if n == total
        ) || chunk.position() == ChunkPosition::Abandoned;
        self.updates.retain(|k| *k != key);
        if owned && !last {
            self.updates.push(key);
        }
        if owned {
            Route::Initiators
        } else {
            Route::Responder
        }
    }

    /// Ends every wait of the Responder and the Initiators that has run out.
    pub(super) fn expire_property(
        &mut self,
        store: &mut ResourceStore,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        if self.server.deadline().is_some_and(|at| now >= at) {
            let served = self.server.on_timeout(store, now);
            self.carry_out_served(served, sends, changes);
        }
        for (muid, actions) in expire_all(&mut self.properties, now) {
            self.carry_out_property(muid, actions, sends, changes);
        }
    }

    /// Starts the call of `ask`: its Initiator refuses what it cannot make.
    pub(super) fn start_property(
        &mut self,
        ask: &PropertyAsk,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let muid = ask.peer.at().peer().muid;
        let Some(initiator) = self.properties.get_mut(&muid) else {
            changes.answers.push((
                self.peer_id(muid),
                Answer::PropertyLost {
                    call: Box::new(ask.call.clone()),
                },
            ));
            return;
        };
        let started = match &ask.call {
            PropertyCall::Get(get) => initiator.get(get, now),
            PropertyCall::GetList { get, page_size } => initiator.get_list(get, *page_size, now),
            PropertyCall::Set(set) => initiator.set(set, now),
            PropertyCall::Subscribe(get) => initiator.subscribe(get, now),
            PropertyCall::Unsubscribe(id) => initiator.unsubscribe(id, now),
        };
        let id = self.peer_id(muid);
        match started {
            Ok((_, actions)) => self.carry_out_property(muid, actions, sends, changes),
            Err(error) => changes.answers.push((
                id,
                Answer::PropertyNotSent {
                    call: Box::new(ask.call.clone()),
                    error,
                },
            )),
        }
    }

    /// What an Initiator asks for: messages to send, changes to tell.
    fn carry_out_property(
        &self,
        muid: Muid,
        actions: Vec<ResourceAction>,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let id = self.peer_id(muid);
        for action in actions {
            match action {
                ResourceAction::Send(message) => sends.push((self.group, message)),
                ResourceAction::Event(event) => match *event {
                    ResourceEvent::Got { target, result, .. } => {
                        changes.answers.push((id, Answer::Got { target, result }));
                    }
                    ResourceEvent::Set {
                        resource, result, ..
                    } => changes.answers.push((id, Answer::Set { resource, result })),
                    ResourceEvent::Subscribed {
                        subscribe_id,
                        target,
                        ..
                    } => changes.answers.push((
                        id,
                        Answer::Subscribed {
                            target,
                            subscribe_id,
                        },
                    )),
                    ResourceEvent::SubscribeFailed {
                        target, failure, ..
                    } => changes
                        .answers
                        .push((id, Answer::SubscribeFailed { target, failure })),
                    ResourceEvent::Unsubscribed {
                        subscribe_id,
                        result,
                        ..
                    } => changes.answers.push((
                        id,
                        Answer::Unsubscribed {
                            subscribe_id,
                            result,
                        },
                    )),
                    other => changes
                        .properties
                        .push((id, PropertyChange::Event(Box::new(other)))),
                },
                _ => {}
            }
        }
    }

    /// What the Responder asks for: replies to send, what the peers did to tell.
    fn carry_out_served(
        &self,
        actions: Vec<ResponderAction>,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        for action in actions {
            match action {
                ResponderAction::Send(message) => sends.push((self.group, message)),
                ResponderAction::Event(event) => changes
                    .properties
                    .push((self.peer_id(event.peer()), PropertyChange::Served(event))),
                _ => {}
            }
        }
    }
}

impl Running {
    /// Starts the calls the app asked for (see [`Ci::ask_property`](super::Ci)): each
    /// goes to the Initiator of its peer.
    pub(super) fn start_properties(
        &mut self,
        asks: &mut Vec<PropertyAsk>,
        now: Instant,
        out: &mut crate::devices::OpenOutput,
        changes: &mut CiChanges,
    ) {
        let mut sends = Vec::new();
        for ask in asks.drain(..) {
            let Some(member) = self
                .members
                .iter_mut()
                .find(|m| m.group == ask.peer.at().peer().group)
            else {
                changes.answers.push((
                    ask.peer.at().peer(),
                    Answer::PropertyLost {
                        call: Box::new(ask.call.clone()),
                    },
                ));
                continue;
            };
            member.start_property(&ask, now, &mut sends, changes);
        }
        changes.refused += super::send(out, &sends);
    }
}
