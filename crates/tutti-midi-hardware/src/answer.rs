//! How a request the session made of a MIDI-CI peer ended.
//!
//! A request ([`ProfileSwitch`](crate::ProfileSwitch), [`ReportAsk`](crate::ReportAsk))
//! is sent at a [`poll`](crate::MidiSession::poll), and its end is one
//! [`Answer`], reported once from a later poll as
//! [`Polled::Answer`](crate::Polled::Answer) beside the peer it is from. An
//! answer is not a change of what the peer lists: the peer's tables change
//! through [`ProfilesChange`](crate::ProfilesChange) and
//! [`ProcessChange`](crate::ProcessChange).

use tutti_midi_device::{Nak, Outcome, Request};

/// How a request to a peer ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Answer {
    /// A Set Profile On or Off ended: the peer reported the Profile's state,
    /// NAKed it, or did not answer in time.
    Switch {
        /// What was asked.
        request: Request,
        /// How it ended.
        outcome: Outcome,
    },
    /// A switch was not sent: the peer was lost between the request and the
    /// poll that would have sent it.
    SwitchLost {
        /// What was asked.
        request: Request,
    },
    /// A MIDI Message Report ended.
    Report(ReportEnd),
    /// A report was not sent: the peer was lost between the request and the
    /// poll that would have sent it.
    ReportLost,
    /// A report was not sent: the peer has since said it does not support MIDI
    /// Message Report.
    ReportUnsupported,
}

/// How a MIDI Message Report ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReportEnd {
    /// The peer's End of MIDI Message Report arrived: the report is complete,
    /// and its messages came on the input before it.
    Ended,
    /// The peer NAKed the report (§9.8.1), which ends it, complete or not. The
    /// NAK's status says whether to retry.
    Refused(Nak),
    /// The peer did not finish the report in time.
    TimedOut,
}
