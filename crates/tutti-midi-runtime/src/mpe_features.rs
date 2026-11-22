//! Reading a peer's answer about which optional features its MPE Profile supports, as [`MpeFeatures`] (M2-120 section 3.7, Table 6; M2-102 sections 2.5 and 3.3).
//!
//! The question is a Profile Details Inquiry for the MPE Profile
//! ([`ProfileId::MPE`](tutti_midi_ci::ProfileId::MPE)) with the Optional
//! Features Inquiry Target ([`MpeFeatures::INQUIRY_TARGET`]), sent through a
//! [`ProfileInitiator::details`](tutti_midi_ci::ProfileInitiator::details); the
//! Reply carries four Inquiry Target Data bytes. [`mpe_features`] reads the
//! [`ProfileOutcome`] the inquiry settles with. It lives here because
//! `MpeFeatures` is the protocol crate's and the outcome the CI crate's, and
//! neither names the other.

use tutti_midi_ci::{DetailsData, Nak, ProfileOutcome};
use tutti_midi_protocol::MpeFeatures;

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

/// The features a [`ProfileOutcome`] of the Optional Features inquiry declares.
///
/// # Errors
///
/// [`MpeFeaturesError`] for a refusal, a timeout, data that is not a defined
/// feature set (a reserved bit or enumerator is refused, not read as the
/// nearest feature), or an outcome of another request.
pub fn mpe_features(outcome: &ProfileOutcome) -> Result<MpeFeatures, MpeFeaturesError> {
    match outcome {
        ProfileOutcome::Details(data) => MpeFeatures::from_data(data.as_bytes())
            .ok_or_else(|| MpeFeaturesError::Unreadable(data.clone())),
        ProfileOutcome::Refused(nak) => Err(MpeFeaturesError::Refused(nak.clone())),
        ProfileOutcome::TimedOut => Err(MpeFeaturesError::TimedOut),
        _ => Err(MpeFeaturesError::NotDetails),
    }
}

#[cfg(test)]
mod tests;
