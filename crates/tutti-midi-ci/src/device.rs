//! [`CiDevice`]: one MIDI-CI Device on one Group of one connection, and the
//! Management messages it runs (M2-101 §3.3, §4, §5.5 to §5.9, Appendix E).
//!
//! It is a pure state machine:
//! every step returns the [`CiAction`]s to carry out, time comes in as an
//! [`Instant`] argument, and nothing here reads a clock, sends a byte, draws from
//! an OS random source or runs a thread. The caller routes to it the CI messages
//! of one Group of the connection (a [`Decoded`] from
//! [`CiMessage::decode`]), and sends every [`CiAction::Send`].
//!
//! The device is not associated with a Function Block (§5.6.2): it speaks with
//! Device ID `0x7F` and answers Discovery with Function Block `0x7F`. A device with
//! Function Blocks owes a Function Block Info Change Alert (UMP) whenever its MUID
//! changes (§3.3.1); this one has none to alert, and [`CiAction::MuidChanged`] is how
//! the caller learns of the change.
//!
//! # The rules
//!
//! - Discovery as Initiator (§4.1, §5.5): [`CiDevice::new`] and every new MUID start a
//!   round: a Discovery from the device's MUID to the Broadcast MUID, Output Path ID 0
//!   (§5.5.4: one MIDI Out). The round's window is [`DISCOVERY_WINDOW`] (§5.5.5);
//!   [`CiAction::WindowClosed`] is signalled once when it ends.
//! - Replies (§5.6): a Reply to Discovery addressed to the device records its peer
//!   ([`CiAction::Found`]). Replies for another MUID are not the device's.
//! - Collision among replies (§5.9.2): a second reply with the same source MUID in a
//!   round means two Responders share a MUID; the device sends an Invalidate MUID for it
//!   and forgets it ([`CiAction::Lost`] if it had been found). Replies are counted for
//!   the whole round, not only inside the window.
//! - Discovery as Responder (§4.1, §5.6): a Discovery from another MUID is answered
//!   to it, echoing its Output Path ID (§5.6.1), with Function Block `0x7F`.
//! - Collision on the device's MUID (§5.9.1): a Discovery whose Source MUID is the
//!   device's own and whose body is not the one it sent, or a Reply from the device's
//!   MUID addressed to it (an answer to its Discovery from a device that drew the same
//!   MUID). Option A needs a MUID not yet used in any Transaction, and the
//!   device's is (its own Discovery), so it always takes Option B: change the MUID, send
//!   an Invalidate MUID targeting the duplicated one, and start a new round. The Invalidate
//!   goes *from the new MUID*, because a source equal to the target would not tell the
//!   other device from this one.
//! - Echo (Appendix A.1.1): a MIDI Thru or merge loops the device's own messages back.
//!   Its Discovery returns with its MUID and its own body, and is ignored; its Reply to
//!   another device returns addressed to that device, and is ignored. A device of the
//!   same model that drew the same MUID sends the same Discovery body and is not seen
//!   there, but it answers this device's Discovery from that MUID to it, which is.
//!   Treating an echo as a collision would change the MUID on every round, for ever.
//! - Invalidate MUID (§5.9): its target the device's MUID ends every Transaction and
//!   starts over under a new MUID; its target a peer's discards that peer
//!   ([`CiAction::Lost`]). It is never answered.
//! - NAK (§5.3, §5.11, Appendix E): a message addressed to the device's MUID that it
//!   cannot handle (one the codec could not read, or a Property Exchange inquiry when
//!   the device does not declare [`CiCategories::PROPERTY_EXCHANGE`]) is answered with a
//!   NAK carrying Table 16's status. A device that declares it leaves those inquiries to
//!   its Property Exchange Responder ([`CiDevice::refuse_property_exchange`] is the NAK for
//!   the ones the Responder did not accept). A message for another MUID, one to the Broadcast MUID, an ACK and a NAK are never NAK'd.
//! - Profile Configuration as Responder (§7, M2-102 §2.4 to §2.7): the device's
//!   Profile Responder answers a Profile Inquiry, Set Profile On, Set Profile Off and Profile
//!   Details Inquiry from the Profiles the app offered ([`CiDevice::offer_profiles`]); with none
//!   offered it answers an Inquiry on the address it used with both lists empty (§7.13) and NAKs
//!   the rest with Status `0x04`, whose details are the Profile Id (M2-101 §5.11.3). The
//!   category is the caller's to declare. A Reply, Report, Details Reply or Profile Specific Data is the
//!   Initiator's to read ([`ProfileInitiator`](super::ProfileInitiator)) and is ignored here;
//!   Profile Specific Data to a device with no Profiles is not answered, as nothing requires it
//!   and a NAK of data would invite one back.
//! - Process Inquiry as Responder (§9): a device that declares
//!   [`CiCategories::PROCESS_INQUIRY`] answers an Inquiry: Process Inquiry Capabilities with
//!   MIDI Message Report supported (§9.3, Table 42), and an Inquiry: MIDI Message Report with
//!   the form of a device with no MIDI state to report: a Reply whose three bitmaps are empty,
//!   which §9.6 has a Responder do for each kind it cannot provide, and the End, on the Device ID
//!   of the Inquiry (§9.6, §9.8). Asked of a Channel, it answers with a NAK, Status `0x03`
//!   (§9.5: the Channel is not in use on the device, as none is). A device that does not declare
//!   the category NAKs both inquiries with Status `0x01`, message not supported. A Reply or an
//!   End is the Initiator's to read ([`ProcessInitiator`](super::ProcessInitiator)) and is ignored here.
//! - Endpoint Inquiry as Initiator (§5.7): once, for each found peer that declared
//!   a Function Block, status `0x00`. The reply sets the peer's Product Instance Id
//!   ([`CiAction::Updated`]); a NAK from it leaves it `None`. No retry and no timeout:
//!   §5.7 and §5.8 give none, the id is a convenience (Appendix A.1.1), and a peer
//!   that never answers is just one with no id.
//! - Endpoint Inquiry as Responder (§5.8): the device has no UMP Endpoint Product
//!   Instance Id to report, so it NAKs (§5.8: a device that cannot provide the
//!   requested information replies with a NAK), status `0x00`.
//!
//! Every new MUID is drawn from the `entropy` the caller gave, redrawn while it is
//! reserved, the device's own or a found peer's. The closure must not return one
//! value for ever, or the redraw does not end.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use std::vec::Vec;
use tutti_platform::Instant;

use tutti_ump::{DeviceIdentity, MpeZoneSet};

use super::{
    CI_VERSION, CiCategories, CiDeviceId, CiHeader, CiMessage, Decoded, Destination, DeviceBody,
    DiscoveryData, EndpointInfo, EndpointStatus, MaxSysexSize, Muid, NakStatus, PeCiMessage,
    PeMessageKind, ProcessFeatures, ProfileOffer, ReplyData, ReportScope, SUB_ID2_ENDPOINT_INQUIRY,
    nak_message,
};
use crate::action::CiAction;
use crate::profile_responder::ProfileResponder;
use tutti_ump::FunctionBlockId;

use tutti_ump::ProductInstanceId;

/// How long a Discovery waits for its replies: at least 3 seconds (§5.5.5).
pub const DISCOVERY_WINDOW: Duration = Duration::from_secs(3);

/// What Discovery told about a peer, and what was learned since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiPeer {
    data: ReplyData,
    instance_id: Option<ProductInstanceId>,
    asked: bool,
}

impl CiPeer {
    /// What its Reply to Discovery declared: identity, categories, receivable
    /// size, and the Output Path ID and Function Block of the reply.
    #[must_use]
    pub fn discovery(&self) -> &ReplyData {
        &self.data
    }

    /// The Function Block the peer represents, `None` for none (`0x7F`).
    #[must_use]
    pub fn function_block(&self) -> Option<FunctionBlockId> {
        self.data.function_block()
    }

    /// Its Endpoint's Product Instance Id, once an Endpoint Inquiry was answered.
    #[must_use]
    pub fn product_instance_id(&self) -> Option<&ProductInstanceId> {
        self.instance_id.as_ref()
    }
}

#[derive(Clone, Debug)]
struct Round {
    window_end: Instant,
    closed: bool,
    /// Every source of a Reply to Discovery addressed to us this round, found
    /// or in collision.
    replied: BTreeSet<Muid>,
}

/// One MIDI-CI Device on one Group of one connection.
pub struct CiDevice<R> {
    muid: Muid,
    own: DeviceBody,
    entropy: R,
    peers: BTreeMap<Muid, CiPeer>,
    round: Round,
    profiles: ProfileResponder,
}

impl<R> std::fmt::Debug for CiDevice<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CiDevice")
            .field("muid", &self.muid)
            .field("peers", &self.peers)
            .field("round", &self.round)
            .finish_non_exhaustive()
    }
}

impl<R: FnMut() -> u32> CiDevice<R> {
    /// A device with a MUID drawn from `entropy`, and the actions that start its
    /// first round at `now`: the Discovery to send.
    ///
    /// `max_sysex_size` is what it can receive (§5.5.3: 128 or more, which the type
    /// holds); `categories` are what it supports, [`CiCategories::empty`] for
    /// Management alone. The Output Path ID and Function Block of its Discovery data
    /// are the device's, not a caller's: 0 and none.
    pub fn new(
        identity: DeviceIdentity,
        categories: CiCategories,
        max_sysex_size: MaxSysexSize,
        mut entropy: R,
        now: Instant,
    ) -> (Self, Vec<CiAction>) {
        let muid = draw(&mut entropy, |_| false);
        let mut device = Self {
            muid,
            own: DeviceBody::new(identity, categories, max_sysex_size),
            entropy,
            peers: BTreeMap::new(),
            round: Round {
                window_end: now,
                closed: true,
                replied: BTreeSet::new(),
            },
            profiles: ProfileResponder::default(),
        };
        let mut actions = Vec::new();
        device.start_round(now, &mut actions);
        (device, actions)
    }

    /// The device's MUID.
    #[must_use]
    pub fn muid(&self) -> Muid {
        self.muid
    }

    /// Makes the device offer `offer` as its Profiles (M2-101 §7, M2-102 §2.4): each
    /// entry that differs from the one offered before is Reported (Added, then
    /// Enabled; Disabled, then Removed), and an entry left equal keeps what a
    /// peer switched. An offer equal to the last one does nothing.
    pub fn offer_profiles(&mut self, offer: &ProfileOffer) -> Vec<CiAction> {
        self.profiles.offer(self.muid, offer)
    }

    /// The zones of the MPE Profile (M2-120) enabled on the device, whether the app or a
    /// peer's Set Profile On enabled them, lowest Manager first; `None` with none.
    #[must_use]
    pub fn mpe_zones(&self) -> Option<MpeZoneSet> {
        self.profiles.mpe_zones()
    }

    /// The peers found, in MUID order.
    pub fn peers(&self) -> impl Iterator<Item = (Muid, &CiPeer)> {
        self.peers.iter().map(|(muid, peer)| (*muid, peer))
    }

    /// One found peer.
    #[must_use]
    pub fn peer(&self, muid: Muid) -> Option<&CiPeer> {
        self.peers.get(&muid)
    }

    /// When the device next needs [`on_timeout`](Self::on_timeout): the end of the
    /// round's window, or `None` once it has closed.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        (!self.round.closed).then_some(self.round.window_end)
    }

    /// Time has passed: closes the window once [`DISCOVERY_WINDOW`] is over.
    /// Calling it early is harmless.
    pub fn on_timeout(&mut self, now: Instant) -> Vec<CiAction> {
        if !self.round.closed && now >= self.round.window_end {
            self.round.closed = true;
            return vec![CiAction::WindowClosed];
        }
        Vec::new()
    }

    /// A CI message of this device's Group arrived.
    pub fn on_message(&mut self, decoded: &Decoded, now: Instant) -> Vec<CiAction> {
        let mut actions = Vec::new();
        match decoded {
            Decoded::NotCi => {}
            Decoded::Unread(unread) => {
                if unread.destination == Some(Destination::Muid(self.muid))
                    && let Some(nak) = unread.nak(self.muid)
                {
                    actions.push(CiAction::Send(nak));
                }
            }
            Decoded::Message(message) => self.on_decoded(message, now, &mut actions),
        }
        actions
    }

    fn on_decoded(&mut self, message: &CiMessage, now: Instant, actions: &mut Vec<CiAction>) {
        match message {
            CiMessage::Discovery { source, data, .. } => {
                if *source == self.muid {
                    // Appendix A.1.1: our own Discovery, looped back by a Thru or a
                    // merge, is the message we sent; anything else under our MUID is
                    // another device that drew it.
                    if *data != self.sent_discovery() {
                        self.collide(now, actions);
                    }
                } else {
                    actions.push(CiAction::Send(CiMessage::DiscoveryReply {
                        device_id: CiDeviceId::FunctionBlock,
                        source: self.muid,
                        to: *source,
                        data: ReplyData::answering(data, self.own),
                    }));
                }
            }
            CiMessage::DiscoveryReply {
                source, to, data, ..
            } => {
                if *source == self.muid {
                    // A Reply from our MUID to our MUID answers our Discovery from a
                    // device that drew our MUID (we never answer ourselves); one to
                    // another MUID is our own Reply looped back.
                    if *to == self.muid {
                        self.collide(now, actions);
                    }
                } else if *to == self.muid {
                    self.on_reply(*source, *data, actions);
                }
            }
            CiMessage::InvalidateMuid { target, .. } => {
                if *target == self.muid {
                    self.renew(None, now, actions);
                } else if self.peers.remove(target).is_some() {
                    actions.push(CiAction::Lost(*target));
                }
            }
            CiMessage::EndpointInquiry { header, to, .. } => {
                if *to == self.muid {
                    actions.push(self.nak(
                        CiDeviceId::FunctionBlock,
                        header.source,
                        SUB_ID2_ENDPOINT_INQUIRY,
                        NakStatus::Nak,
                    ));
                }
            }
            CiMessage::EndpointReply { header, to, info } => {
                if *to == self.muid
                    && let Some(peer) = self.peers.get_mut(&header.source)
                    && peer.asked
                {
                    let EndpointInfo::ProductInstanceId(id) = info;
                    peer.instance_id = Some(*id);
                    actions.push(CiAction::Updated(header.source));
                }
            }
            // A NAK of the Endpoint Inquiry ends it: the peer has no id to give.
            CiMessage::Nak { header, to, nak } => {
                if *to == self.muid
                    && nak.nak_sub_id2.get() == SUB_ID2_ENDPOINT_INQUIRY
                    && let Some(peer) = self.peers.get_mut(&header.source)
                {
                    peer.asked = false;
                }
            }
            CiMessage::ProfileInquiry { .. }
            | CiMessage::SetProfileOn { .. }
            | CiMessage::SetProfileOff { .. }
            | CiMessage::ProfileDetailsInquiry { .. } => {
                actions.extend(self.profiles.on_message(self.muid, message));
            }
            CiMessage::ProcessCapabilitiesInquiry { source, to, .. } => {
                if *to == self.muid {
                    actions.push(match self.process_refusal(message) {
                        Some(refusal) => refusal,
                        None => CiAction::Send(CiMessage::ProcessCapabilitiesReply {
                            source: self.muid,
                            ci_version: CI_VERSION,
                            to: *source,
                            features: ProcessFeatures::MIDI_MESSAGE_REPORT,
                        }),
                    });
                }
            }
            CiMessage::MessageReportInquiry {
                source, to, scope, ..
            } => {
                if *to == self.muid {
                    match (self.process_refusal(message), scope.address()) {
                        (Some(refusal), _) => actions.push(refusal),
                        // Tutti has no Channel in use (§9.5).
                        (None, address @ CiDeviceId::Channel(_)) => {
                            actions.push(self.nak(
                                address,
                                *source,
                                message.sub_id2(),
                                NakStatus::NotInUse,
                            ));
                        }
                        (None, address) => {
                            actions.push(CiAction::Send(CiMessage::MessageReportReply {
                                source: self.muid,
                                ci_version: CI_VERSION,
                                to: *source,
                                scope: ReportScope::none(address),
                            }));
                            actions.push(CiAction::Send(CiMessage::MessageReportEnd {
                                header: self.header_on(address),
                                to: *source,
                            }));
                        }
                    }
                }
            }
            // A Property Exchange Responder is not the Management core's: a device
            // that declares none refuses the inquiries one would answer, and one
            // that declares it leaves them to its Responder. The rest is the
            // Initiator's.
            CiMessage::PropertyExchange {
                header,
                to,
                message,
            } => {
                if *to == self.muid
                    && !self
                        .own
                        .categories
                        .contains(CiCategories::PROPERTY_EXCHANGE)
                    && is_property_inquiry(message)
                {
                    actions.push(self.nak(
                        CiDeviceId::FunctionBlock,
                        header.source,
                        message.sub_id2(),
                        NakStatus::MessageNotSupported,
                    ));
                }
            }
            // The Initiator's to read, not the Management core's.
            CiMessage::ProcessCapabilitiesReply { .. }
            | CiMessage::MessageReportReply { .. }
            | CiMessage::MessageReportEnd { .. }
            | CiMessage::ProfileReply { .. }
            | CiMessage::ProfileEnabled { .. }
            | CiMessage::ProfileDisabled { .. }
            | CiMessage::ProfileAdded { .. }
            | CiMessage::ProfileRemoved { .. }
            | CiMessage::ProfileDetailsReply { .. }
            | CiMessage::ProfileSpecificData(_) => {}
            // Never answered: a pair of devices would trade NAKs for ever.
            CiMessage::Ack { .. } => {}
        }
    }

    /// The NAK a Process Inquiry Capabilities or MIDI Message Report inquiry
    /// addressed to this device is refused with, on the address it used, when
    /// the device does not declare Process Inquiry (§9), or `None`.
    fn process_refusal(&self, inquiry: &CiMessage) -> Option<CiAction> {
        if self.own.categories.contains(CiCategories::PROCESS_INQUIRY) {
            return None;
        }
        let asked = inquiry.header();
        Some(self.nak(
            asked.device_id,
            asked.source,
            inquiry.sub_id2(),
            NakStatus::MessageNotSupported,
        ))
    }

    fn on_reply(&mut self, source: Muid, data: ReplyData, actions: &mut Vec<CiAction>) {
        if !self.round.replied.insert(source) {
            // §5.9.2: a second reply with the same MUID.
            actions.push(CiAction::Send(CiMessage::InvalidateMuid {
                header: self.header(),
                target: source,
            }));
            if self.peers.remove(&source).is_some() {
                actions.push(CiAction::Lost(source));
            }
            return;
        }
        let asks = data.function_block().is_some();
        self.peers.insert(
            source,
            CiPeer {
                data,
                instance_id: None,
                asked: asks,
            },
        );
        actions.push(CiAction::Found(source));
        if asks {
            actions.push(CiAction::Send(CiMessage::EndpointInquiry {
                header: self.header(),
                to: source,
                status: EndpointStatus::ProductInstanceId,
            }));
        }
    }

    /// §5.9.1 Option B: a Discovery or Reply carried our own MUID.
    fn collide(&mut self, now: Instant, actions: &mut Vec<CiAction>) {
        let duplicated = self.muid;
        self.renew(Some(duplicated), now, actions);
    }

    /// Changes the MUID, drops every peer, announces the Invalidate (when one is
    /// owed) and starts a new round.
    fn renew(&mut self, invalidate: Option<Muid>, now: Instant, actions: &mut Vec<CiAction>) {
        let old = self.muid;
        let peers = &self.peers;
        self.muid = draw(&mut self.entropy, |m| m == old || peers.contains_key(&m));
        actions.push(CiAction::MuidChanged {
            old,
            new: self.muid,
        });
        for (peer, _) in std::mem::take(&mut self.peers) {
            actions.push(CiAction::Lost(peer));
        }
        if let Some(target) = invalidate {
            actions.push(CiAction::Send(CiMessage::InvalidateMuid {
                header: self.header(),
                target,
            }));
        }
        self.start_round(now, actions);
    }

    fn start_round(&mut self, now: Instant, actions: &mut Vec<CiAction>) {
        self.round = Round {
            window_end: now + DISCOVERY_WINDOW,
            closed: false,
            replied: BTreeSet::new(),
        };
        actions.push(CiAction::Send(CiMessage::Discovery {
            device_id: CiDeviceId::FunctionBlock,
            source: self.muid,
            data: self.sent_discovery(),
        }));
    }

    /// The body of the Discovery this device sends: Output Path 0.
    fn sent_discovery(&self) -> DiscoveryData {
        DiscoveryData::new(self.own)
    }

    fn header(&self) -> CiHeader {
        CiHeader {
            device_id: CiDeviceId::FunctionBlock,
            ci_version: CI_VERSION,
            source: self.muid,
        }
    }

    /// The preamble of a message from this device on `address`.
    fn header_on(&self, address: CiDeviceId) -> CiHeader {
        CiHeader {
            device_id: address,
            ..self.header()
        }
    }

    /// The NAK a Property Exchange inquiry to this device is answered with when
    /// the caller's Responder does not serve its sender (M2-101 §5.11, Table 16),
    /// or `None` for any other message. A device that declares Property
    /// Exchange leaves its inquiries to a Responder: the caller sends this for
    /// the ones the Responder did not accept, a Subscription Start among them.
    #[must_use]
    pub fn refuse_property_exchange(&self, decoded: &Decoded) -> Option<CiMessage> {
        let Decoded::Message(CiMessage::PropertyExchange {
            header,
            to,
            message,
        }) = decoded
        else {
            return None;
        };
        let subscription = matches!(
            message,
            PeCiMessage::Chunk {
                kind: PeMessageKind::SUBSCRIPTION_INQUIRY,
                ..
            }
        );
        (*to == self.muid && (is_property_inquiry(message) || subscription)).then(|| {
            nak_message(
                self.header(),
                header.source,
                message.sub_id2(),
                NakStatus::MessageNotSupported,
                None,
            )
        })
    }

    /// A NAK of `original` to `to`, on `address`.
    fn nak(&self, address: CiDeviceId, to: Muid, original: u8, status: NakStatus) -> CiAction {
        CiAction::Send(nak_message(
            self.header_on(address),
            to,
            original,
            status,
            None,
        ))
    }
}

/// Whether `message` is an inquiry a Property Exchange Responder answers.
fn is_property_inquiry(message: &PeCiMessage) -> bool {
    matches!(
        message,
        PeCiMessage::CapabilitiesInquiry(_)
            | PeCiMessage::Chunk {
                kind: PeMessageKind::GET_INQUIRY | PeMessageKind::SET_INQUIRY,
                ..
            }
    )
}

/// A MUID from `entropy` that `taken` does not refuse.
fn draw(entropy: &mut impl FnMut() -> u32, taken: impl Fn(Muid) -> bool) -> Muid {
    loop {
        if let Some(muid) = Muid::from_entropy(entropy())
            && !taken(muid)
        {
            return muid;
        }
    }
}

#[cfg(test)]
mod tests;
