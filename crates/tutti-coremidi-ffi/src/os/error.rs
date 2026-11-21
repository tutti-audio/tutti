//! The errors of a CoreMIDI call.

use super::objc_exception::ObjcException;
use crate::checks::blocks::BlockRefusal;
use std::fmt;

pub use crate::checks::os_status::OsStatus;

/// The CoreMIDI call that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    /// `MIDIClientCreate`.
    CreateClient,
    /// `MIDISourceCreateWithProtocol`.
    CreateSource,
    /// `MIDIDestinationCreateWithProtocol`.
    CreateDestination,
    /// `MIDIInputPortCreateWithProtocol`.
    CreateInputPort,
    /// `MIDIPortConnectSource`.
    ConnectSource,
    /// `MIDIOutputPortCreate`.
    CreateOutputPort,
    /// `MIDIReceivedEventList`: a virtual source publishing an event list.
    ReceivedEventList,
    /// `MIDISendEventList`: an output port sending to a destination.
    SendEventList,
    /// `-[MIDIUMPMutableEndpoint initWithName:...]`.
    CreateUmpEndpoint,
    /// `-[MIDIUMPMutableFunctionBlock initWithName:...]`.
    CreateFunctionBlock,
    /// `-[MIDIUMPMutableEndpoint registerFunctionBlocks:markAsStatic:error:]`.
    RegisterFunctionBlocks,
    /// `-[MIDIUMPMutableEndpoint setEnabled:error:]`.
    EnableUmpEndpoint,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CreateClient => "MIDIClientCreate",
            Self::CreateSource => "MIDISourceCreateWithProtocol",
            Self::CreateDestination => "MIDIDestinationCreateWithProtocol",
            Self::CreateInputPort => "MIDIInputPortCreateWithProtocol",
            Self::ConnectSource => "MIDIPortConnectSource",
            Self::CreateOutputPort => "MIDIOutputPortCreate",
            Self::ReceivedEventList => "MIDIReceivedEventList",
            Self::SendEventList => "MIDISendEventList",
            Self::CreateUmpEndpoint => "MIDIUMPMutableEndpoint init",
            Self::CreateFunctionBlock => "MIDIUMPMutableFunctionBlock init",
            Self::RegisterFunctionBlocks => "MIDIUMPMutableEndpoint registerFunctionBlocks",
            Self::EnableUmpEndpoint => "MIDIUMPMutableEndpoint setEnabled",
        })
    }
}

/// A CoreMIDI call that failed, or a message CoreMIDI could not be given.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A call returned a non-zero status.
    #[error("{operation} failed: {status}")]
    Call {
        /// The call.
        operation: Operation,
        /// What it returned.
        status: OsStatus,
    },
    /// The protocol is a reserved value (only a reader of the wire builds
    /// one), and no endpoint is created in it.
    #[error("no endpoint is created in a reserved protocol")]
    ReservedProtocol,
    /// A message does not fit the event list it is sent in.
    #[error("the message does not fit the event list")]
    EventListFull,
    /// An Objective-C call returned an `NSError`: its code and domain are the
    /// error's own, not assumed to be an `OSStatus`.
    #[error("{operation} failed: {description} (domain {domain}, code {code})")]
    Objc {
        /// The call.
        operation: Operation,
        /// `NSError.code`.
        code: isize,
        /// `NSError.domain`.
        domain: String,
        /// `NSError.localizedDescription`.
        description: String,
    },
    /// CoreMIDI numbered a Function Block outside the 5 bits the spec holds.
    #[error("CoreMIDI numbered a Function Block {0}, which the spec cannot hold")]
    BlockNumber(u8),
    /// CoreMIDI listed blocks other than the ones registered (another count, or a
    /// registered name that is not listed once).
    #[error("CoreMIDI listed {listed} Function Blocks for the {registered} registered")]
    BlockReadBack {
        /// How many blocks were registered.
        registered: usize,
        /// How many CoreMIDI listed.
        listed: usize,
    },
    /// CoreMIDI refused the call with `kMIDINotPermitted` (-10844).
    #[error("{0} was refused: kMIDINotPermitted (-10844)")]
    NotPermitted(Operation),
    /// The endpoint a send was for has been dropped.
    #[error("the endpoint was dropped")]
    EndpointGone,
    /// An Objective-C call returned no object and no error.
    #[error("{0} returned no object")]
    NoObject(Operation),
    /// The OS has no `MIDIUMPMutableEndpoint` (before macOS 15), so an endpoint
    /// with Function Blocks cannot be created.
    #[error("this macOS has no MIDIUMPMutableEndpoint (before macOS 15)")]
    UmpEndpointUnsupported,
    /// A Function Block CoreMIDI cannot carry.
    #[error(transparent)]
    Block(#[from] BlockRefusal),
    /// CoreMIDI raised an Objective-C exception.
    #[error(transparent)]
    Exception(#[from] ObjcException),
    /// The process's client has been started, or is being: a second is not made.
    #[error("the process's CoreMIDI client is already started")]
    ClientExists,
    /// A call kept failing with the status it is retried on until its bound passed.
    #[error("{operation} kept failing with {last} until its retry bound passed")]
    RetryTimedOut {
        /// The call.
        operation: Operation,
        /// The last status it failed with.
        last: OsStatus,
    },
}

impl Error {
    /// `operation` failed with `status`.
    #[must_use]
    pub fn call(operation: Operation, status: OsStatus) -> Self {
        Self::Call { operation, status }
    }

    /// `operation` failed with the `NSError` code `code`: `NotPermitted` for
    /// `kMIDINotPermitted`, the error's own fields otherwise.
    pub(crate) fn from_ns_error(
        operation: Operation,
        code: isize,
        domain: String,
        description: String,
    ) -> Self {
        if i32::try_from(code) == Ok(OsStatus::NOT_PERMITTED.get()) {
            return Self::NotPermitted(operation);
        }
        Self::Objc {
            operation,
            code,
            domain,
            description,
        }
    }

    /// The `OSStatus` of the failure when it carries one: a call's status, the
    /// refusal `kMIDINotPermitted`, or an `NSError` code that fits one.
    pub(crate) fn os_status(&self) -> Option<OsStatus> {
        match self {
            Self::Call { status, .. } => Some(*status),
            Self::NotPermitted(_) => Some(OsStatus::NOT_PERMITTED),
            Self::Objc { code, .. } => i32::try_from(*code).ok().map(OsStatus::new),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `kMIDINotPermitted` (-10844) from an `NSError` is `NotPermitted`; any other
    /// code keeps the error's own fields.
    #[test]
    fn an_ns_error_with_kmidinotpermitted_is_not_permitted() {
        let op = Operation::EnableUmpEndpoint;
        assert_eq!(
            Error::from_ns_error(op, -10844, "NSOSStatusErrorDomain".into(), "x".into()),
            Error::NotPermitted(op)
        );
        assert!(matches!(
            Error::from_ns_error(op, -10843, "d".into(), "x".into()),
            Error::Objc { code: -10843, .. }
        ));
    }

    #[test]
    fn os_status_is_the_status_a_failure_carries() {
        let op = Operation::EnableUmpEndpoint;
        let status = OsStatus::new(-10843);
        assert_eq!(Error::call(op, status).os_status(), Some(status));
        assert_eq!(
            Error::NotPermitted(op).os_status(),
            Some(OsStatus::NOT_PERMITTED)
        );
        assert_eq!(
            Error::from_ns_error(op, -10843, "d".into(), "x".into()).os_status(),
            Some(status)
        );
        assert_eq!(
            Error::from_ns_error(op, isize::MAX, "d".into(), "x".into()).os_status(),
            None
        );
        assert_eq!(Error::EndpointGone.os_status(), None);
    }
}
