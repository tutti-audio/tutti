//! The Process Inquiry of a MIDI-CI peer, as the session drives it: the peers
//! that can be asked for a MIDI Message Report ([`Reporter`]), the request
//! ([`ReportAsk`]) and what the peer's Process Inquiry did ([`ProcessChange`]).
//!
//! The session is the Initiator (M2-101 §9): it asks every peer that declared
//! Process Inquiry for its capabilities when it is found, and lists those whose
//! reply says they support MIDI Message Report. A request is made only from such
//! a handle, so an app cannot ask a peer that never declared the category or
//! answered that it lacks the feature. The handle is a snapshot: the peer may be
//! lost before the request is sent, and that is refused ([`PeerRequestError::NoSuchPeer`]).
//!
//! The session is also the Responder: a peer's request for a report of what the
//! session sent ([`ReportInquiry`]) is handed to the app through
//! [`Polled::ReportInquiry`](crate::Polled::ReportInquiry), which answers it from what
//! its output said, or hands it back to
//! [`MidiSession::decline_report`](crate::MidiSession::decline_report).
//!
//! A report is a Reply, the MIDI messages themselves, and an End. The End is an
//! [`Answer::Report`](crate::Answer::Report); the messages between the Reply and
//! it arrive on the input as the MIDI they are, in order (§9.8.2), and reach the
//! graph like any other message of the device.

use std::sync::Arc;

use tutti_midi_ci::{
    CI_VERSION, CiHeader, CiMessage, DataControl, Muid, ProcessFeatures, ReportRequest, ReportScope,
};
use tutti_ump::{GroupSpan, MidiGroup};

use crate::peer::{PeerRef, PeerRequestError};

/// A peer that declared Process Inquiry and answered that it supports MIDI
/// Message Report: the handle a [`ReportAsk`] is made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reporter {
    pub(crate) at: PeerRef,
}

impl Reporter {
    /// The peer, as every handle of MIDI-CI names one.
    #[must_use]
    pub const fn at(&self) -> &PeerRef {
        &self.at
    }

    /// A request that the peer report what `request` asks
    /// ([`MidiSession::report`](crate::MidiSession::report)).
    #[must_use]
    pub fn ask(&self, request: ReportRequest) -> ReportAsk {
        ReportAsk {
            peer: self.clone(),
            request,
        }
    }
}

/// A request for a MIDI Message Report from one peer. Made only from a
/// [`Reporter`], and it names the input device that handle was read from, so it
/// cannot be sent to another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportAsk {
    pub(crate) peer: Reporter,
    pub(crate) request: ReportRequest,
}

impl ReportAsk {
    /// The peer the request is for.
    #[must_use]
    pub const fn peer(&self) -> &Reporter {
        &self.peer
    }

    /// Whether `peers`, the peers an input can ask for a report now, still hold
    /// the request's peer.
    ///
    /// # Errors
    ///
    /// [`PeerRequestError::NoSuchPeer`] when the peer is gone or no longer supports
    /// MIDI Message Report.
    pub fn check<'a>(
        &self,
        peers: impl IntoIterator<Item = &'a Reporter>,
    ) -> Result<(), PeerRequestError> {
        self.peer.at.find(peers, |peer| &peer.at).map(|_| ())
    }
}

/// A peer's request for a MIDI Message Report (M2-101 §9.5) of the session's
/// MIDI-CI Device, with where to send the report.
///
/// The report is the Reply, the messages and the End, in that order on one
/// connection (§9.8.2): the messages are the state of what the session sent, which
/// the hardware layer does not hold, so [`Polled::ReportInquiry`](crate::Polled::ReportInquiry)
/// hands the request on, and [`MidiSession::decline_report`](crate::MidiSession::decline_report)
/// answers it with the report of a device that declares nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportInquiry {
    pub(crate) group: MidiGroup,
    pub(crate) groups: GroupSpan,
    pub(crate) own: Muid,
    pub(crate) peer: Muid,
    pub(crate) control: DataControl,
    pub(crate) scope: ReportScope,
}

impl ReportInquiry {
    /// The Group the MIDI-CI Device speaks on: the first Group of its Function
    /// Block, which the Reply and the End go out on.
    #[must_use]
    pub const fn group(&self) -> MidiGroup {
        self.group
    }

    /// The Groups of the Function Block the request covers at Device ID `0x7F`,
    /// in ascending order (§9.7.1): those of the block that are Groups of the
    /// wire.
    pub fn groups(&self) -> impl Iterator<Item = MidiGroup> {
        let first = self.group.get();
        (first..first.saturating_add(self.groups.get())).filter_map(MidiGroup::new)
    }

    /// How much to report.
    #[must_use]
    pub const fn control(&self) -> DataControl {
        self.control
    }

    /// Where, and which kinds of message.
    #[must_use]
    pub const fn scope(&self) -> &ReportScope {
        &self.scope
    }

    /// The Reply to MIDI Message Report that begins the report, on the request's
    /// Device ID and declaring `declared` (§9.6).
    #[must_use]
    pub const fn reply(&self, declared: ReportScope) -> CiMessage {
        CiMessage::MessageReportReply {
            source: self.own,
            ci_version: CI_VERSION,
            to: self.peer,
            scope: declared,
        }
    }

    /// The End of MIDI Message Report that completes it (§9.8).
    #[must_use]
    pub fn end(&self) -> CiMessage {
        let mut header = CiHeader::new(self.own);
        header.device_id = self.scope.address();
        CiMessage::MessageReportEnd {
            header,
            to: self.peer,
        }
    }
}

/// The peers of one input that can be asked for a report.
pub(crate) type Snapshot = Arc<[Reporter]>;

/// A change in the Process Inquiry of one peer, once each from
/// [`MidiSession::poll`](crate::MidiSession::poll); the [`PeerId`](crate::PeerId)
/// beside it in [`Polled::Process`](crate::Polled::Process) names the peer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcessChange {
    /// The peer's Reply to Process Inquiry Capabilities arrived, with what it
    /// supports. If it supports MIDI Message Report it is now in
    /// [`MidiSession::report_peers`](crate::MidiSession::report_peers).
    Capabilities(ProcessFeatures),
    /// The peer did not reply to the capabilities inquiry in time.
    CapabilitiesTimedOut,
    /// The peer NAKed the capabilities inquiry: it has no Process Inquiry.
    CapabilitiesRefused,
    /// The peer is gone (lost, or MIDI-CI ended): its Initiator went with it.
    Dropped,
}

#[cfg(test)]
mod tests {
    use tutti_midi_ci::CiDeviceId;

    use super::*;

    fn asked(first: u8, span: u8) -> ReportInquiry {
        ReportInquiry {
            group: MidiGroup::of(first),
            groups: GroupSpan::of(span),
            own: Muid::of(1),
            peer: Muid::of(2),
            control: DataControl::Full,
            scope: ReportScope::none(CiDeviceId::FunctionBlock),
        }
    }

    /// M2-101 section 9.7.1: a report of the Function Block covers the Groups of the block
    /// in ascending order, from its first; a block cannot reach past Group 16.
    #[test]
    fn the_groups_of_a_report_are_the_blocks_from_its_first_ascending_up_to_the_last_group() {
        let groups =
            |first, span| -> Vec<u8> { asked(first, span).groups().map(MidiGroup::get).collect() };
        assert_eq!(groups(3, 3), [3, 4, 5]);
        assert_eq!(groups(0, 1), [0]);
        assert_eq!(groups(14, 4), [14, 15]);
    }
}
