//! ACK and NAK (M2-101 §5.10 and §5.11, Tables 13 to 16): the two share one
//! body, an original sub-ID#2, a status code and data, five bytes of detail and
//! a text.

use std::vec::Vec;

use tutti_ump::{U7, U14};

use super::{MessageText, PeTerminator, TimeoutWait};

/// Fixes the two bodies as one shape: the spec lays out Table 13 and Table 15
/// field for field alike, and what differs is the codes and the sub-ID#2.
macro_rules! status_message {
    (
        $(#[$doc:meta])*
        $name:ident, $status:ident, $sub:ident, $table:literal, $classification:literal
    ) => {
        $(#[$doc])*
        #[derive(Clone, Debug, Default, PartialEq, Eq)]
        pub struct $name {
            #[doc = $classification]
            pub $sub: U7,
            /// The status code and its data: why.
            pub status: $status,
            /// The details for each Sub ID Classification: 5 bytes whose meaning
            /// depends on the original sub-ID#2 (the Profile ID, or a Property
            /// Exchange stream id and chunk number; reserved for Process
            /// Inquiry). Zero when a transaction has no detail to add.
            pub details: [U7; 5],
            /// The human-readable reason (Message Length and Message Text).
            pub message: MessageText,
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(
                    f,
                    concat!(stringify!($name), " with status {:#04x}"),
                    self.status.code().get()
                )
            }
        }

        impl $name {
            /// A message for the original sub-ID#2 `sub_id2` with a status,
            /// no details and no text.
            #[must_use]
            pub fn new(sub_id2: U7, status: $status) -> Self {
                Self {
                    $sub: sub_id2,
                    status,
                    ..Self::default()
                }
            }

            /// With the 5-byte detail block.
            #[must_use]
            pub fn with_details(mut self, details: [U7; 5]) -> Self {
                self.details = details;
                self
            }

            /// With a text.
            #[must_use]
            pub fn with_message(mut self, message: MessageText) -> Self {
                self.message = message;
                self
            }

            #[doc = concat!("The body of ", $table, ".")]
            pub(crate) fn encode_body(&self, out: &mut impl Extend<u8>) {
                out.extend([
                    self.$sub.get(),
                    self.status.code().get(),
                    self.status.data().get(),
                ]);
                out.extend(self.details.iter().map(|d| d.get()));
                let mut text = Vec::new();
                self.message.encode(&mut text);
                // The length is a 14-bit field; the text of a message we hold
                // is at most 103 bytes, and one read from a peer fits it.
                let len = U14::saturating_from_len(text.len());
                out.extend(len.to_le7());
                out.extend(text.into_iter().take(usize::from(len.get())));
            }

            /// Reads the body, or `None` when it is malformed: a byte with its
            /// top bit set, or a Message Length that runs past the message.
            ///
            /// Everything past the first byte was added in Message Format
            /// Version 2, and an older device sent a NAK with no body at all, so
            /// a body that stops before a field defaults it, as §5.3
            /// has a receiver read only the fields of the received version.
            pub(crate) fn decode_body(b: &[u8]) -> Option<Self> {
                let status = super::read::status_body(b)?;
                Some(Self {
                    $sub: status.sub_id2,
                    status: $status::from_wire(status.status_code, status.status_data),
                    details: status.details,
                    message: status.message,
                })
            }
        }
    };
}

status_message!(
    /// An ACK (M2-101 §5.10, Table 13): the positive answer or the notice of
    /// an activity under way, such as a Timeout Wait. The message is
    /// defined for mechanisms published elsewhere, whose own specifications
    /// explain its use.
    Ack,
    AckStatus,
    ack_sub_id2,
    "Table 13",
    "`Original Transaction Sub-ID#2 Classification`: the sub-ID#2 of the message this answers (§5.10.1)."
);

status_message!(
    /// A NAK (M2-101 §5.11, Table 15): the answer to a message a device does not
    /// understand. Everything past the first byte was added in Message Format
    /// Version 2; an older device sent an empty NAK, and each absent field reads
    /// as zero or empty.
    Nak,
    NakStatus,
    nak_sub_id2,
    "Table 15",
    "`Original Transaction Sub-ID#2 Classification`: the sub-ID#2 of the message being NAK'd (0 if unknown); with the other fields it links the NAK to the original Transaction (§5.11.1)."
);

/// An ACK's status code and status data (M2-101 §5.10.2, Table 14).
///
/// A pair the table names is its variant; any other pair is
/// [`Other`](Self::Other), which holds it as it came, so every status read
/// writes back as it was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AckStatus {
    /// `0x00`: success, do not retry.
    #[default]
    Ack,
    /// `0x10`: Timeout Wait. The status data is the time to wait.
    TimeoutWait(TimeoutWait),
    /// `0x11`: Flow Control, send the next Chunks (Property Exchange).
    FlowControlNextChunks,
    /// A status code and data the table does not name.
    Other {
        /// The status code.
        code: U7,
        /// The status data.
        data: U7,
    },
}

impl AckStatus {
    /// The status a status code and data on the wire say.
    #[must_use]
    pub fn from_wire(code: U7, data: U7) -> Self {
        match (code.get(), data.get()) {
            (0x00, 0x00) => Self::Ack,
            (0x10, wait) => {
                TimeoutWait::new(wait).map_or(Self::Other { code, data }, Self::TimeoutWait)
            }
            (0x11, 0x00) => Self::FlowControlNextChunks,
            _ => Self::Other { code, data },
        }
    }

    /// The status code.
    #[must_use]
    pub const fn code(self) -> U7 {
        match self {
            Self::Ack => U7::of(0x00),
            Self::TimeoutWait(_) => U7::of(0x10),
            Self::FlowControlNextChunks => U7::of(0x11),
            Self::Other { code, .. } => code,
        }
    }

    /// The status data.
    #[must_use]
    pub const fn data(self) -> U7 {
        match self {
            Self::TimeoutWait(wait) => U7::of(wait.tenths()),
            Self::Ack | Self::FlowControlNextChunks => U7::of(0x00),
            Self::Other { data, .. } => data,
        }
    }
}

/// A NAK's status code and status data (M2-101 §5.11.2, Table 16).
///
/// A pair the table names is its variant; any other pair (including the
/// reserved codes `0x60` to `0x7F`) is [`Other`](Self::Other), which holds it
/// as it came, so every status read writes back as it was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NakStatus {
    /// `0x00`: generic failure, do not retry.
    #[default]
    Nak,
    /// `0x01`: the MIDI-CI message is not supported.
    MessageNotSupported,
    /// `0x02`: the MIDI-CI version is not supported.
    VersionNotSupported,
    /// `0x03`: the Channel, Group or Function Block is not in use.
    NotInUse,
    /// `0x04`: the Profile is not supported on the requested Channel, Group
    /// or Function Block.
    ProfileNotSupported,
    /// `0x12`: Flow Control, resend the most recent Chunk.
    ResendLastChunk,
    /// `0x20`: the responder (status data 0) or the initiator (status data 1)
    /// terminates the transaction.
    Terminate(PeTerminator),
    /// `0x21`: Property Exchange Chunks are out of sequence and cannot be
    /// recovered.
    ChunksOutOfSequence,
    /// `0x40`: an error occurred and a retry is recommended.
    Retry,
    /// `0x41`: the message was malformed.
    Malformed,
    /// `0x42`: a timeout has occurred.
    Timeout,
    /// `0x43`: busy, try again after the status data's time.
    Busy(TimeoutWait),
    /// A status code and data the table does not name.
    Other {
        /// The status code.
        code: U7,
        /// The status data.
        data: U7,
    },
}

impl NakStatus {
    /// The status a status code and data on the wire say.
    #[must_use]
    pub fn from_wire(code: U7, data: U7) -> Self {
        let other = Self::Other { code, data };
        match (code.get(), data.get()) {
            (0x00, 0x00) => Self::Nak,
            (0x01, 0x00) => Self::MessageNotSupported,
            (0x02, 0x00) => Self::VersionNotSupported,
            (0x03, 0x00) => Self::NotInUse,
            (0x04, 0x00) => Self::ProfileNotSupported,
            (0x12, 0x00) => Self::ResendLastChunk,
            (0x20, 0x00) => Self::Terminate(PeTerminator::Responder),
            (0x20, 0x01) => Self::Terminate(PeTerminator::Initiator),
            (0x21, 0x00) => Self::ChunksOutOfSequence,
            (0x40, 0x00) => Self::Retry,
            (0x41, 0x00) => Self::Malformed,
            (0x42, 0x00) => Self::Timeout,
            (0x43, wait) => TimeoutWait::new(wait).map_or(other, Self::Busy),
            _ => other,
        }
    }

    /// The status code.
    #[must_use]
    pub const fn code(self) -> U7 {
        match self {
            Self::Nak => U7::of(0x00),
            Self::MessageNotSupported => U7::of(0x01),
            Self::VersionNotSupported => U7::of(0x02),
            Self::NotInUse => U7::of(0x03),
            Self::ProfileNotSupported => U7::of(0x04),
            Self::ResendLastChunk => U7::of(0x12),
            Self::Terminate(_) => U7::of(0x20),
            Self::ChunksOutOfSequence => U7::of(0x21),
            Self::Retry => U7::of(0x40),
            Self::Malformed => U7::of(0x41),
            Self::Timeout => U7::of(0x42),
            Self::Busy(_) => U7::of(0x43),
            Self::Other { code, .. } => code,
        }
    }

    /// The status data.
    #[must_use]
    pub const fn data(self) -> U7 {
        match self {
            Self::Terminate(PeTerminator::Initiator) => U7::of(0x01),
            Self::Busy(wait) => U7::of(wait.tenths()),
            Self::Other { data, .. } => data,
            _ => U7::of(0x00),
        }
    }
}

#[cfg(test)]
mod tests;
