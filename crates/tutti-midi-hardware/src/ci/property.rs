//! Property Exchange on a member device: an Initiator for each peer that declares
//! it and the one Responder that serves the device's store (M2-103). The Initiators
//! and the Responder are stepped by the member's messages and time, as Profile
//! Configuration and Process Inquiry are, and what they send goes in the same
//! batch.

use std::time::Instant;

use tutti_midi_device::Muid;
use tutti_midi_device::{
    BodyLimit, BufferLimit, CiCategories, CiMessage, Decoded, PeCapabilities, PeReassemblyLimits,
    PeSettings, ResourceAction, ResourceInitiator, ResourceResponder, ResourceStore,
    ResponderAction, ResponderSettings, SimultaneousRequests, StreamLimit,
};
use tutti_ump::MidiGroup;

use super::{CiChanges, Member, Running};
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

/// The Responder of a device with MUID `own`, serving `store`.
pub(super) fn responder(own: Muid, store: ResourceStore) -> ResourceResponder {
    ResourceResponder::new(own, RESPONDER, store)
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
        if self.properties.remove(&muid).is_some() {
            changes
                .properties
                .push((self.peer_id(muid), PropertyChange::Dropped));
        }
    }

    /// The device's MUID changed: the Responder is for the new one, over the
    /// store as the peers left it, and holds no Subscription.
    pub(super) fn renew_property(&mut self, new: Muid) {
        self.server = responder(new, self.server.store().clone());
    }

    /// The Responder serves `store`, as the peers it already serves are.
    pub(super) fn serve(&mut self, store: ResourceStore) {
        self.server = responder(self.device.muid(), store);
        let serves: Vec<_> = self
            .properties
            .keys()
            .filter_map(|&muid| {
                self.device
                    .peer(muid)
                    .map(|peer| (muid, peer.discovery().max_sysex_size))
            })
            .collect();
        for (muid, max_sysex) in serves {
            self.server.accept(muid, max_sysex);
        }
    }

    /// Hands the Responder and every Initiator the message, which is theirs to
    /// keep or ignore.
    pub(super) fn route_property(
        &mut self,
        decoded: &Decoded,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let served = self.server.on_message(decoded, now);
        self.carry_out_served(served, sends, changes);
        let heard: Vec<_> = self
            .properties
            .iter_mut()
            .map(|(&muid, initiator)| (muid, initiator.on_message(decoded, now)))
            .collect();
        for (muid, actions) in heard {
            self.carry_out_property(muid, actions, sends, changes);
        }
    }

    /// Ends every wait of the Responder and the Initiators that has run out.
    pub(super) fn expire_property(
        &mut self,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        if self.server.deadline().is_some_and(|at| now >= at) {
            let served = self.server.on_timeout(now);
            self.carry_out_served(served, sends, changes);
        }
        let ended: Vec<_> = self
            .properties
            .iter_mut()
            .filter(|(_, initiator)| initiator.deadline().is_some_and(|at| now >= at))
            .map(|(&muid, initiator)| (muid, initiator.on_timeout(now)))
            .collect();
        for (muid, actions) in ended {
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
        let muid = ask.peer.muid;
        let Some(initiator) = self.properties.get_mut(&muid) else {
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
            Ok((call, actions)) => {
                changes.properties.push((id, PropertyChange::Started(call)));
                self.carry_out_property(muid, actions, sends, changes);
            }
            Err(error) => changes.properties.push((
                id,
                PropertyChange::NotSent {
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
                ResourceAction::Event(event) => {
                    changes.properties.push((id, PropertyChange::Event(event)))
                }
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
    /// Gives each member's Responder `store`.
    pub(super) fn serve(&mut self, store: &ResourceStore) {
        for member in &mut self.members {
            member.serve(store.clone());
        }
    }

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
            let Some(member) = self.members.iter_mut().find(|m| m.group == ask.peer.group) else {
                continue;
            };
            member.start_property(&ask, now, &mut sends, changes);
        }
        changes.refused += super::send(out, &sends);
    }
}
