//! [`ProcessInitiator`]: the Initiator's side of Process Inquiry with one
//! discovered peer (M2-101 §9).
//!
//! A pure state machine in the shape of [`ProfileInitiator`](super::ProfileInitiator):
//! every step returns the [`ProcessAction`]s to carry out, time is an [`Instant`]
//! argument, and nothing here reads a clock or sends a byte. The caller makes one
//! per peer that declared Process Inquiry in Discovery (Table 7, bit D4), routes
//! to it every decoded message of that Group, sends each [`ProcessAction::Send`]
//! on that Group, and drops it when the peer is lost.
//!
//! # The rules
//!
//! - Capabilities (§9.2, §9.3): [`new`](ProcessInitiator::new) sends an Inquiry:
//!   Process Inquiry Capabilities, to the Function Block, the only address it may
//!   use. The reply's features ([`ProcessAction::Capabilities`]) say whether the
//!   peer supports MIDI Message Report; [`report`](ProcessInitiator::report)
//!   refuses a request once the peer has said it does not.
//! - A MIDI Message Report (§9.4 to §9.8): [`report`](ProcessInitiator::report)
//!   sends the Inquiry; the peer answers with a Reply (the Begin), the MIDI
//!   messages themselves, and an End. The Reply must be on the Device ID the
//!   Inquiry used (§9.6) and is handed out as [`ProcessAction::ReportBegan`],
//!   holding the kinds the peer will send cut to the kinds asked, since it may
//!   not set another bit. The End, on that Device ID too, ends the transaction
//!   ([`ProcessAction::ReportEnded`]). The messages between them are
//!   not CI and do not pass through here: they arrive on the connection as MIDI,
//!   and are the report. One report is in flight at a time, since the MIDI
//!   messages carry nothing to tell two apart.
//! - A NAK of either inquiry, from the peer to this device, ends it, even with no
//!   End received (§9.8.1: a Responder that aborts or errs sends a NAK with
//!   Status `0x20` or `0x40`, and the Initiator assumes the transaction ended).
//!   [`ProcessAction::ReportRefused`] and [`ProcessAction::CapabilitiesRefused`]
//!   hand it out.
//! - Timeout Wait (§5.10, Table 14): an ACK with Status Code `0x10` from the
//!   peer, to this device, names the transaction by its Original Transaction
//!   Sub-ID#2 and the Device ID it is on, and adds the wait to its deadline.
//!
//! A message from another source, or one not addressed to this device, is not
//! this peer's and changes nothing.
//!
//! # The timeout
//!
//! The capabilities inquiry waits [`PEER_REPLY_TIMEOUT`] for its reply. A report waits that long for the Reply, and the Reply
//! starts the wait again, for the End; so the time a report takes is bounded by
//! the peer's silence, not by how much it has to say.

use std::vec::Vec;
use tutti_platform::Instant;

use super::Initiator;
use super::handshake::Handshake;
use super::link::Link;
use super::{PEER_REPLY_TIMEOUT, TimeoutWait};
use crate::deadline::Deadline;

use super::{
    CiDeviceId, CiHeader, CiMessage, DataControl, Decoded, Muid, Nak, ProcessFeatures, ReportScope,
    SUB_ID2_MESSAGE_REPORT_INQUIRY, SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
};

/// What a MIDI Message Report asks (§9.5): how much, and of what.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReportRequest {
    /// How much the peer reports.
    pub control: DataControl,
    /// Where, and which kinds of message.
    pub scope: ReportScope,
}

impl ReportRequest {
    /// A request of `scope` at `control`.
    #[must_use]
    pub const fn new(control: DataControl, scope: ReportScope) -> Self {
        Self { control, scope }
    }
}

/// What the driver does next.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcessAction {
    /// Send this message to the peer, on the Group of the connection.
    Send(CiMessage),
    /// The peer's Reply to Process Inquiry Capabilities arrived.
    Capabilities(ProcessFeatures),
    /// The peer NAKed the capabilities inquiry: it has no Process Inquiry to
    /// ask about.
    CapabilitiesRefused(Nak),
    /// [`PEER_REPLY_TIMEOUT`] passed with no reply to the capabilities inquiry.
    CapabilitiesTimedOut,
    /// The peer's Reply to MIDI Message Report arrived: it is about to send the
    /// kinds of message in `declared`, which are the kinds it was asked for that
    /// it supports.
    ReportBegan {
        /// What the peer will report.
        declared: ReportScope,
    },
    /// The End of MIDI Message Report arrived: the report is complete.
    ReportEnded,
    /// The peer NAKed the report, which ends it with whatever it had sent.
    ReportRefused(Nak),
    /// [`PEER_REPLY_TIMEOUT`] passed with no Reply, or after the Reply with no End:
    /// the report is over, complete or not.
    ReportTimedOut,
}

/// A report that cannot be asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum ProcessError {
    /// A report is still waiting for its Reply or its End.
    #[error("a MIDI Message Report is already in flight")]
    AlreadyAsking,
    /// The peer's capabilities say it does not support MIDI Message Report.
    #[error("the peer does not support MIDI Message Report")]
    NotSupported,
}

#[derive(Clone, Copy, Debug)]
enum Stage {
    /// The Inquiry is out; the Reply is not in.
    AwaitingReply,
    /// The Reply came; the End is not in.
    Reporting,
}

#[derive(Clone, Copy, Debug)]
struct Report {
    request: ReportRequest,
    stage: Stage,
    wait: Deadline,
}

/// The Initiator of Process Inquiry with one peer.
#[derive(Clone, Debug)]
pub struct ProcessInitiator {
    link: Link,
    capabilities: Handshake<ProcessFeatures>,
    report: Option<Report>,
}

impl ProcessInitiator {
    /// An Initiator for `peer`, from this device's MUID `own`, and the actions that
    /// start it at `now`: an Inquiry: Process Inquiry Capabilities.
    #[must_use]
    pub fn new(own: Muid, peer: Muid, now: Instant) -> (Self, Vec<ProcessAction>) {
        let mut initiator = Self {
            link: Link { own, peer },
            capabilities: Handshake::Unanswered,
            report: None,
        };
        let actions = initiator.inquire(now);
        (initiator, actions)
    }

    /// The peer's MUID.
    #[must_use]
    pub fn peer(&self) -> Muid {
        self.link.peer
    }

    /// The features the peer's reply declared, `None` before one arrived; a new
    /// inquiry keeps them until its reply, and a timeout or refusal drops them.
    #[must_use]
    pub fn features(&self) -> Option<ProcessFeatures> {
        self.capabilities.answer().copied()
    }

    /// Asks for the capabilities again: a wait already running starts over.
    pub fn inquire(&mut self, now: Instant) -> Vec<ProcessAction> {
        self.capabilities.ask(now, PEER_REPLY_TIMEOUT, ());
        vec![self.send(CiDeviceId::FunctionBlock, |header, to| {
            CiMessage::ProcessCapabilitiesInquiry {
                source: header.source,
                ci_version: header.ci_version,
                to,
            }
        })]
    }

    /// Asks the peer for a MIDI Message Report; its Reply, End or refusal is a
    /// [`ProcessAction`] of a later step.
    ///
    /// # Errors
    ///
    /// [`ProcessError::AlreadyAsking`] while a report is waiting, and
    /// [`ProcessError::NotSupported`] once the peer's capabilities lack MIDI
    /// Message Report. A request made before the capabilities are known is sent:
    /// the spec does not require the inquiry first.
    pub fn report(
        &mut self,
        request: ReportRequest,
        now: Instant,
    ) -> Result<Vec<ProcessAction>, ProcessError> {
        if self.report.is_some() {
            return Err(ProcessError::AlreadyAsking);
        }
        if self
            .features()
            .is_some_and(|features| !features.contains(ProcessFeatures::MIDI_MESSAGE_REPORT))
        {
            return Err(ProcessError::NotSupported);
        }
        self.report = Some(Report {
            request,
            stage: Stage::AwaitingReply,
            wait: Deadline::after(now, PEER_REPLY_TIMEOUT),
        });
        let ReportRequest { control, scope } = request;
        Ok(vec![self.send(scope.address(), |header, to| {
            CiMessage::MessageReportInquiry {
                source: header.source,
                ci_version: header.ci_version,
                to,
                control,
                scope,
            }
        })])
    }

    /// Whether a report is waiting for its Reply or its End.
    #[must_use]
    pub fn is_reporting(&self) -> bool {
        self.report.is_some()
    }

    /// A Timeout Wait: the time asked is added to the deadline of the transaction
    /// it names.
    fn on_wait(&mut self, address: CiDeviceId, original: u8, wait: TimeoutWait) {
        match original {
            SUB_ID2_PROCESS_CAPABILITIES_INQUIRY if address == CiDeviceId::FunctionBlock => {
                if let Some(pending) = self.capabilities.wait_mut() {
                    wait.extend(pending);
                }
            }
            SUB_ID2_MESSAGE_REPORT_INQUIRY => {
                if let Some(report) = self
                    .report
                    .as_mut()
                    .filter(|report| report.request.scope.address() == address)
                {
                    wait.extend(&mut report.wait);
                }
            }
            _ => {}
        }
    }

    fn on_nak(&mut self, address: CiDeviceId, nak: &Nak, actions: &mut Vec<ProcessAction>) {
        match nak.nak_sub_id2.get() {
            SUB_ID2_PROCESS_CAPABILITIES_INQUIRY
                if address == CiDeviceId::FunctionBlock && self.capabilities.refuse() =>
            {
                actions.push(ProcessAction::CapabilitiesRefused(nak.clone()));
            }
            SUB_ID2_MESSAGE_REPORT_INQUIRY
                if self
                    .report
                    .is_some_and(|report| report.request.scope.address() == address) =>
            {
                self.report = None;
                actions.push(ProcessAction::ReportRefused(nak.clone()));
            }
            _ => {}
        }
    }

    fn send(
        &self,
        address: CiDeviceId,
        message: impl FnOnce(CiHeader, Muid) -> CiMessage,
    ) -> ProcessAction {
        ProcessAction::Send(self.link.message(address, message))
    }
}

impl Initiator for ProcessInitiator {
    type Action = ProcessAction;

    fn deadline(&self) -> Option<Instant> {
        Deadline::earliest(
            self.capabilities
                .wait()
                .into_iter()
                .chain(self.report.map(|report| report.wait)),
        )
    }

    fn on_message(&mut self, decoded: &Decoded, now: Instant) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        let Decoded::Message(message) = decoded else {
            return actions;
        };
        if !self.link.is_from_peer(message) || !self.link.is_to_us(message) {
            return actions;
        }
        let address = message.header().device_id;
        match message {
            CiMessage::ProcessCapabilitiesReply { features, .. }
                if self.capabilities.is_asking() =>
            {
                self.capabilities = Handshake::Answered(*features);
                actions.push(ProcessAction::Capabilities(*features));
            }
            CiMessage::MessageReportReply { scope, .. } => {
                if let Some(report) = self.report.as_mut()
                    && matches!(report.stage, Stage::AwaitingReply)
                    && scope.address() == report.request.scope.address()
                {
                    report.stage = Stage::Reporting;
                    report.wait.restart(now, PEER_REPLY_TIMEOUT);
                    actions.push(ProcessAction::ReportBegan {
                        declared: scope.within(&report.request.scope),
                    });
                }
            }
            CiMessage::MessageReportEnd { .. } => {
                if self.report.is_some_and(|report| {
                    matches!(report.stage, Stage::Reporting)
                        && report.request.scope.address() == address
                }) {
                    self.report = None;
                    actions.push(ProcessAction::ReportEnded);
                }
            }
            CiMessage::Ack { ack, .. } => {
                if let Some(wait) = TimeoutWait::from_ack(ack) {
                    self.on_wait(address, ack.ack_sub_id2.get(), wait);
                }
            }
            CiMessage::Nak { nak, .. } => self.on_nak(address, nak, &mut actions),
            _ => {}
        }
        actions
    }

    fn on_timeout(&mut self, now: Instant) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        if self.capabilities.expire(now) {
            actions.push(ProcessAction::CapabilitiesTimedOut);
        }
        if self.report.is_some_and(|report| report.wait.expired(now)) {
            self.report = None;
            actions.push(ProcessAction::ReportTimedOut);
        }
        actions
    }
}

#[cfg(test)]
mod tests;
