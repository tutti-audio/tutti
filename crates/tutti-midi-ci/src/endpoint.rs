//! Inquiry: Endpoint and Reply to Endpoint (M2-101 §5.7 and §5.8, Tables 9 to 11):
//! a device asks the UMP Endpoint behind a Function Block for one piece of
//! information, selected by a Status.
//!
//! In v1.2 the only Status is `0x00`, the Endpoint's Product Instance Id; `0x01` to
//! `0x7F` are reserved (Table 10). The reply's Information Data depends on the
//! Status it echoes (§5.8.1), so the two are one value, [`EndpointInfo`]: a reply
//! cannot carry data that disagrees with its status.

use tutti_ump::{U7, U14};

use tutti_ump::ProductInstanceId;

/// What an Inquiry: Endpoint asks for (§5.7.1, Table 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndpointStatus {
    /// `0x00`: the Product Instance ID of the UMP Endpoint the Responder
    /// belongs to.
    ProductInstanceId,
}

impl EndpointStatus {
    /// The wire value of [`ProductInstanceId`](Self::ProductInstanceId).
    pub(crate) const PRODUCT_INSTANCE_ID: u8 = 0x00;

    /// The status, or `None` for a reserved value (`0x01..=0x7F`) or one with the
    /// top bit set.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        match raw {
            Self::PRODUCT_INSTANCE_ID => Some(Self::ProductInstanceId),
            _ => None,
        }
    }

    /// The Status byte.
    #[must_use]
    pub const fn get(self) -> U7 {
        match self {
            Self::ProductInstanceId => U7::of(Self::PRODUCT_INSTANCE_ID),
        }
    }
}

/// The Status and Information Data of a Reply to Endpoint (§5.8): the data is
/// the kind the status names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndpointInfo {
    /// Status `0x00` with the Endpoint Product Instance Id (§5.8.3.1): ASCII
    /// text of at most 42 bytes, the length of the UMP Stream's.
    ProductInstanceId(ProductInstanceId),
}

impl EndpointInfo {
    /// The status this information answers.
    #[must_use]
    pub fn status(&self) -> EndpointStatus {
        match self {
            Self::ProductInstanceId(_) => EndpointStatus::ProductInstanceId,
        }
    }

    /// Appends Status, Length of Following Information Data (14 bits, LSB
    /// first) and the data.
    pub(crate) fn encode_body(&self, out: &mut impl Extend<u8>) {
        out.extend([self.status().get().get()]);
        match self {
            Self::ProductInstanceId(id) => {
                let text = id.as_str().as_bytes();
                // An Instance Id is 42 bytes at most.
                out.extend(U14::saturating_from_len(text.len()).to_le7());
                out.extend(text.iter().copied());
            }
        }
    }
}

#[cfg(test)]
mod tests;
