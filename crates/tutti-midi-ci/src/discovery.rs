//! MIDI-CI Discovery and its reply: the body, the sub-IDs of every message the
//! codec models, and the two range-checked fields (M2-101 §5.5 and §5.6).
//!
//! Discovery is the opening handshake: an initiator broadcasts its identity and
//! the CI categories it supports, and each responder replies in kind. The reply
//! shares the Discovery *body* format, so [`DeviceBody`] serves both.

use bitflags::bitflags;
use tutti_ump::{DeviceIdentity, FunctionBlockId, U7, U28};

/// Sub-ID#2: Discovery message (M2-101 §5.5).
pub const SUB_ID2_DISCOVERY: u8 = 0x70;
/// Sub-ID#2: Discovery Reply message.
pub const SUB_ID2_DISCOVERY_REPLY: u8 = 0x71;
/// Sub-ID#2: Invalidate MUID message (M2-101 §5.9).
pub const SUB_ID2_INVALIDATE_MUID: u8 = 0x7E;
/// Sub-ID#2: Inquiry: Endpoint message (M2-101 §5.7).
pub const SUB_ID2_ENDPOINT_INQUIRY: u8 = 0x72;
/// Sub-ID#2: Reply to Endpoint message (M2-101 §5.8).
pub const SUB_ID2_ENDPOINT_REPLY: u8 = 0x73;
/// Sub-ID#2: ACK message (M2-101 §5.10).
pub const SUB_ID2_ACK: u8 = 0x7D;
/// Sub-ID#2: NAK message (M2-101 §5.11).
pub const SUB_ID2_NAK: u8 = 0x7F;

bitflags! {
    /// The CI capability categories a device advertises in Discovery
    /// (M2-101 §5.5.2, the CI Category Supported bitmap).
    ///
    /// Bit D1, Protocol Negotiation, is deprecated (Table 7) and has no flag:
    /// a device built from this type cannot advertise it, and a peer's D1 is
    /// dropped when its bitmap is read.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct CiCategories: u8 {
        /// Supports Profile Configuration.
        const PROFILE_CONFIGURATION = 1 << 2;
        /// Supports Property Exchange.
        const PROPERTY_EXCHANGE = 1 << 3;
        /// Supports Process Inquiry.
        const PROCESS_INQUIRY = 1 << 4;
    }
}

/// The wire value of no Function Block, `0x7F` (M2-101 §5.6.2: used when the
/// device is not associated with a Function Block, for example on a MIDI 1.0
/// byte stream). [`ReplyV2::function_block`] is `None` for it.
const NO_FUNCTION_BLOCK_WIRE: u8 = 0x7F;

tutti_units::ranged! {
    /// The Receivable Maximum SysEx Message Size (M2-101 §5.5.3): 28 bits, and at
    /// least 128.
    ///
    /// Every device must support System Exclusive messages of at least 128 bytes,
    /// so 128 is the smallest value. A device that can initiate Profile Configuration
    /// or Property Exchange must support at least 512 bytes:
    /// whether a device is one is the caller's to say, so that floor is
    /// [`INITIATOR_MIN`](Self::INITIATOR_MIN) and not checked here.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    MaxSysexSize(u32), NotASysexSize, "a receivable SysEx size (128 to 2^28 - 1)",
    |raw| raw >= 128 && raw <= 0x0FFF_FFFF
}

impl MaxSysexSize {
    /// The smallest size, 128 bytes (§5.5.3).
    pub const MIN: u32 = 128;
    /// The smallest size of a device that initiates Profile Configuration or
    /// Property Exchange transactions, 512 bytes (§5.5.3).
    pub const INITIATOR_MIN: u32 = 512;
    /// The largest, 28 bits: four 7-bit bytes.
    pub const MAX: u32 = 0x0FFF_FFFF;

    /// The four 7-bit wire bytes, least significant first (§5.5.3). The size
    /// is at most 28 bits, so each byte is the 7 bits its place holds.
    #[must_use]
    pub(crate) const fn to_le7(self) -> [u8; 4] {
        // The size is at most 28 bits, so it is a U28.
        U28::masked(self.get()).to_le7()
    }
}

/// The fields a Discovery (M2-101 Table 6) and a Reply to Discovery (Table 8)
/// share, in wire order: the sending device's SysEx identity, the CI categories
/// it supports and its receive buffer size.
///
/// What follows them is Message Format Version 2's and differs by direction:
/// [`DiscoveryData`] appends the Output Path ID, [`ReplyData`] the Output Path
/// ID it echoes and the Function Block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceBody {
    /// The device's SysEx identity: the 3-byte manufacturer id, the family and
    /// model (each a little-endian pair of 7-bit bytes) and the 4-byte software
    /// revision, the same ones a Device Inquiry reply carries (M2-101 §5.5.1).
    pub identity: DeviceIdentity,
    /// CI categories this device supports.
    pub categories: CiCategories,
    /// Maximum SysEx message size the device can receive (§5.5.3).
    pub max_sysex_size: MaxSysexSize,
}

impl DeviceBody {
    /// A body of the device `identity`, supporting `categories`, able to receive
    /// `max_sysex_size`.
    #[must_use]
    pub fn new(
        identity: DeviceIdentity,
        categories: CiCategories,
        max_sysex_size: MaxSysexSize,
    ) -> Self {
        Self {
            identity,
            categories,
            max_sysex_size,
        }
    }

    pub(crate) fn encode(&self, out: &mut impl Extend<u8>) {
        let id = &self.identity;
        out.extend(
            id.manufacturer
                .sysex_bytes()
                .iter()
                .chain(&id.family)
                .chain(&id.model)
                .chain(&id.revision.get())
                .map(|b| b.get()),
        );
        out.extend(self.categories.bits().to_le_bytes());
        out.extend(self.max_sysex_size.to_le7());
    }
}

/// The body of a Discovery message (M2-101 Table 6): the [`DeviceBody`] and, in
/// Message Format Version 2, the initiator's Output Path ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryData {
    /// The device's identity, categories and receive buffer size.
    pub body: DeviceBody,
    /// **Output Path ID** (Message Version 2): §5.5.4 has an initiator with
    /// several MIDI Out connections use a unique Output Path ID for each.
    ///
    /// `None` is a Message Format Version 1 body, which ends before the field:
    /// the peer sent no path id, which is not path 0.
    pub output_path_id: Option<U7>,
}

impl DiscoveryData {
    /// A Message Format Version 2 body for an initiator with a single output:
    /// path id 0 (§5.5.4).
    #[must_use]
    pub fn new(body: DeviceBody) -> Self {
        Self {
            body,
            output_path_id: Some(U7::default()),
        }
    }

    pub(crate) fn encode_body(&self, out: &mut impl Extend<u8>) {
        self.body.encode(out);
        if let Some(path) = self.output_path_id {
            out.extend([path.get()]);
        }
    }

    /// Decode a Discovery body, or `None` when it is malformed: shorter than the
    /// common fields, a byte with its top bit set, a Receivable Maximum SysEx
    /// Message Size under 128 (§5.5.3), or a one-byte manufacturer id followed
    /// by a non-zero byte (M2-104 §7.1.3).
    ///
    /// A body that ends after the Receivable Maximum SysEx Message Size is a
    /// Message Format Version 1 body (§5.3, §5.4): no Output Path ID (`None`,
    /// not path 0).
    pub(crate) fn decode_body(b: &[u8]) -> Option<Self> {
        super::read::discovery_body(b)
    }
}

/// The body of a Reply to Discovery message (M2-101 Table 8): the
/// [`DeviceBody`] and, in Message Format Version 2, [`ReplyV2`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyData {
    /// The device's identity, categories and receive buffer size.
    pub body: DeviceBody,
    /// The Version 2 fields, or `None` for a Message Format Version 1 body,
    /// which ends before them.
    pub v2: Option<ReplyV2>,
}

/// The Message Format Version 2 fields of a Reply to Discovery (Table 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyV2 {
    /// The **Initiator's Output Path ID** of the Discovery being answered
    /// (§5.6.1 makes echoing it mandatory).
    pub output_path_id: U7,
    /// The **Function Block** this responder represents, or `None` (wire `0x7F`)
    /// if it represents none (§5.6.2). A number the spec does not give a
    /// Function Block (32 to 126) is malformed.
    pub function_block: Option<FunctionBlockId>,
}

impl ReplyData {
    /// A Version 2 Reply to the Discovery of an initiator with a single output
    /// (Output Path ID 0, §5.5.4), representing no Function Block.
    #[must_use]
    pub fn new(body: DeviceBody) -> Self {
        Self {
            body,
            v2: Some(ReplyV2 {
                output_path_id: U7::default(),
                function_block: None,
            }),
        }
    }

    /// The Reply of the device `body` to `heard`: §5.6.1 returns its Output Path
    /// ID, which a Version 1 Discovery lacks and an initiator with a single
    /// output sets to 0 (§5.5.4). It represents no Function Block.
    #[must_use]
    pub fn answering(heard: &DiscoveryData, body: DeviceBody) -> Self {
        Self {
            body,
            v2: Some(ReplyV2 {
                output_path_id: heard.output_path_id.unwrap_or_default(),
                function_block: None,
            }),
        }
    }

    /// The Function Block the responder represents, `None` for none (`0x7F`) or
    /// from a Version 1 body.
    #[must_use]
    pub fn function_block(&self) -> Option<FunctionBlockId> {
        self.v2.and_then(|v2| v2.function_block)
    }

    pub(crate) fn encode_body(&self, out: &mut impl Extend<u8>) {
        self.body.encode(out);
        if let Some(v2) = self.v2 {
            out.extend([
                v2.output_path_id.get(),
                v2.function_block
                    .map_or(NO_FUNCTION_BLOCK_WIRE, FunctionBlockId::get),
            ]);
        }
    }

    /// Decode a Reply body, or `None` when it is malformed: as
    /// [`DiscoveryData::decode_body`], or a Function Block that is neither 0 to
    /// 31 nor `0x7F` (§5.6.2). A body that ends after the Receivable Maximum
    /// SysEx Message Size is Version 1: no Output Path ID and no Function Block
    /// (not block 0, which would be a lie).
    pub(crate) fn decode_body(b: &[u8]) -> Option<Self> {
        super::read::reply_body(b)
    }
}

#[cfg(test)]
mod tests;
