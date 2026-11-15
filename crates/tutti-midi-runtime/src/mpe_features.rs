//! Asking a peer which optional features its MPE Profile supports, and reading
//! the answer as [`MpeFeatures`] (M2-120 section 3.7; M2-102 section 7.6).
//!
//! The question is a Profile Details Inquiry for the MPE Profile
//! ([`ProfileId::MPE`]) with the Optional Features Inquiry Target
//! ([`MpeFeatures::INQUIRY_TARGET`]); the Reply carries four Inquiry Target
//! Data bytes. [`ask_mpe_features`] sends the inquiry through a
//! [`ProfileInitiator`], and [`mpe_features`] reads the [`Outcome`] it
//! settles with. The two live here because `MpeFeatures` is the protocol
//! crate's and the Initiator the device crate's, and neither names the other.

use std::time::Instant;

use tutti_midi_device::{
    CiDeviceId, DetailsData, InquiryTarget, Nak, Outcome, ProfileAction, ProfileId,
    ProfileInitiator, RequestError,
};
use tutti_midi_protocol::MpeFeatures;
use tutti_ump::U7;

/// How an answer to the Optional Features inquiry is not a feature set.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MpeFeaturesError {
    /// The peer sent a NAK: the Profile is not supported at that address, or
    /// the peer refused the inquiry.
    #[error("the peer refused the Optional Features inquiry")]
    Refused(Nak),
    /// No Reply came in time.
    #[error("no Reply to the Optional Features inquiry")]
    TimedOut,
    /// The Reply's Inquiry Target Data is not four bytes of defined values.
    #[error("the Reply's data is not a defined set of MPE features")]
    Unreadable(DetailsData),
    /// The outcome answers a different request (a Set Profile Report).
    #[error("the outcome is not the answer to a Details Inquiry")]
    NotDetails,
}

/// Asks the peer at `address` for the optional features of its MPE Profile.
///
/// # Errors
///
/// [`RequestError::InFlight`] when the same inquiry is waiting.
pub fn ask_mpe_features(
    initiator: &mut ProfileInitiator,
    address: CiDeviceId,
    now: Instant,
) -> Result<Vec<ProfileAction>, RequestError> {
    initiator.details(address, ProfileId::MPE, features_target(), now)
}

/// The features an [`Outcome`] of [`ask_mpe_features`] declares.
///
/// # Errors
///
/// [`MpeFeaturesError`] for a refusal, a timeout, data that is not a defined
/// feature set (a reserved bit or enumerator is refused, not read as the
/// nearest feature), or an outcome of another request.
pub fn mpe_features(outcome: &Outcome) -> Result<MpeFeatures, MpeFeaturesError> {
    match outcome {
        Outcome::Details(data) => MpeFeatures::from_data(data.as_bytes())
            .ok_or_else(|| MpeFeaturesError::Unreadable(data.clone())),
        Outcome::Refused(nak) => Err(MpeFeaturesError::Refused(nak.clone())),
        Outcome::TimedOut => Err(MpeFeaturesError::TimedOut),
        _ => Err(MpeFeaturesError::NotDetails),
    }
}

fn features_target() -> InquiryTarget {
    InquiryTarget::new(U7::masked(MpeFeatures::INQUIRY_TARGET))
}

#[cfg(test)]
mod tests;
