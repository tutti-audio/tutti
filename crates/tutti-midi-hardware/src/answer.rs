//! How a request the session made of a MIDI-CI peer ended.
//!
//! A request ([`ProfileSwitch`](crate::ProfileSwitch), [`ReportAsk`](crate::ReportAsk),
//! [`PropertyAsk`](crate::PropertyAsk))
//! is sent at a [`poll`](crate::MidiSession::poll), and its end is one
//! [`Answer`], reported once from a later poll as
//! [`Polled::Answer`](crate::Polled::Answer) beside the peer it is from. An
//! answer is not a change of what the peer lists: the peer's tables change
//! through [`ProfilesChange`](crate::ProfilesChange) and
//! [`ProcessChange`](crate::ProcessChange).

use tutti_midi_ci::{
    CallError, Nak, PeData, PeResource, PeStatus, ProfileOutcome, ProfileRequest, ResourceFailure,
    ResourceTarget, SubscribeId,
};

use crate::ProfileError;
use crate::properties::PropertyCall;

/// How a request to a peer ended.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Answer {
    /// A Set Profile On or Off ended: the peer reported the Profile's state,
    /// NAKed it, or did not answer in time.
    Switch {
        /// What was asked.
        request: ProfileRequest,
        /// How it ended.
        outcome: ProfileOutcome,
    },
    /// A switch was not sent: the peer was lost between the request and the
    /// poll that would have sent it, or the peer declared a maximum that the
    /// switch asks more Channels than (M2-102 §2.5.1) in that time.
    SwitchLost {
        /// What was asked.
        request: ProfileRequest,
    },
    /// A switch was not sent: the peer declared a maximum, between the request
    /// and the poll that would have sent it, that the switch asks more Channels
    /// than (M2-102 §2.5.1). It is the refusal
    /// [`MidiSession::set_profile`](crate::MidiSession::set_profile) makes at the call
    /// when the maximum is known by then.
    SwitchRefused {
        /// What was asked.
        request: ProfileRequest,
        /// Why it was refused.
        error: ProfileError,
    },
    /// A MIDI Message Report ended.
    Report(ReportEnd),
    /// A report was not sent: the peer was lost between the request and the
    /// poll that would have sent it.
    ReportLost,
    /// A report was not sent: the peer has since said it does not support MIDI
    /// Message Report.
    ReportUnsupported,
    /// A Get of a Resource ended: the Property Data, or why not.
    Got {
        /// The Resource got.
        target: ResourceTarget,
        /// The data, or why not.
        result: Result<PeData, ResourceFailure>,
    },
    /// A Set of a Resource ended: the status of the reply, 200 or 201, or why
    /// not.
    Set {
        /// The Resource set.
        resource: PeResource,
        /// The status, or why not.
        result: Result<PeStatus, ResourceFailure>,
    },
    /// A Subscription to a Resource started: the peer accepted it and the Resource
    /// is being got. Its updates are [`PropertyChange::Event`](crate::PropertyChange::Event)s.
    Subscribed {
        /// The Resource subscribed to.
        target: ResourceTarget,
        /// The Subscription.
        subscribe_id: SubscribeId,
    },
    /// A Subscription did not start: the peer refused it or did not answer.
    SubscribeFailed {
        /// The Resource asked for.
        target: ResourceTarget,
        /// Why not.
        failure: ResourceFailure,
    },
    /// The Initiator ended a Subscription: the peer's reply, or why it did not
    /// come. The Subscription is over either way.
    Unsubscribed {
        /// The Subscription.
        subscribe_id: SubscribeId,
        /// The status of the reply, or why not.
        result: Result<PeStatus, ResourceFailure>,
    },
    /// A Property Exchange call was not started: the Initiator refused it.
    PropertyNotSent {
        /// What was asked.
        call: Box<PropertyCall>,
        /// Why not.
        error: CallError,
    },
    /// A Property Exchange call was not started: the peer was lost between the
    /// request and the poll that would have started it.
    PropertyLost {
        /// What was asked.
        call: Box<PropertyCall>,
    },
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
