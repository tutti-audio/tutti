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
//! A report is a Reply, the MIDI messages themselves, and an End. The End is an
//! [`Answer::Report`](crate::Answer::Report); the messages between the Reply and
//! it arrive on the input as the MIDI they are, in order (§9.8.2), and reach the
//! graph like any other message of the device.

use std::sync::Arc;

use tutti_midi_ci::Muid;
use tutti_midi_ci::{ProcessFeatures, ReportRequest};
use tutti_midi_device::MidiDeviceId;
use tutti_ump::MidiGroup;

use crate::peer::{PeerRef, PeerRequestError};

/// A peer that declared Process Inquiry and answered that it supports MIDI
/// Message Report: the handle a [`ReportAsk`] is made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reporter {
    pub(crate) at: PeerRef,
}

impl Reporter {
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
    /// The input device the request was made from.
    #[must_use]
    pub const fn device(&self) -> &MidiDeviceId {
        self.peer.at.device()
    }

    /// The peer the request is for.
    #[must_use]
    pub const fn peer(&self) -> crate::ci::PeerId {
        self.peer.at.peer()
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
