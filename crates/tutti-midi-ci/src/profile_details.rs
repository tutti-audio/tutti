//! A Profile Details reply read as the Profile's own typed data.
//!
//! The Inquiry Target Data of a reply has no tag of its own: what it is follows
//! from the Profile Id and the Inquiry Target the inquiry named (M2-101 §7.6).
//! [`DetailsData::decode`] reads it by that pair and by nothing else, never by
//! its length.

use super::{
    ArticulationDetails, ArticulationLevel, ChannelsDetail, CustomSounds, DetailsData,
    DrumFeatures, DrumNoteMapLevel, InquiryTarget, OrganFeatures, OrganLevel, ProfileId,
};

/// The Inquiry Target Data of a Profile Details reply, typed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProfileDetails {
    /// `Number of MIDI Channels` (M2-102 §2.5.1), the Registered Target of any
    /// Profile.
    Channels(ChannelsDetail),
    /// The Drawbar Organ Profile's optional features (M2-121 §4.2).
    Organ(OrganFeatures),
    /// The Default Drum Note Map Profile's optional features and sounds
    /// (M2-125 §7.1.3).
    Drum(DrumFeatures),
    /// The Note On Orchestral Articulation Profile's features and Variations
    /// (M2-123 §9.1).
    Articulation(ArticulationDetails),
    /// The Articulation Profile's custom sounds (M2-123 §9.2).
    ArticulationCustom(CustomSounds),
}

impl DetailsData {
    /// The data of a reply to an inquiry for `target` of `profile`, typed, or
    /// `None` when the pair names no typed data here or the data is not
    /// well-formed for it.
    #[must_use]
    pub fn decode(&self, profile: ProfileId, target: InquiryTarget) -> Option<ProfileDetails> {
        if target == InquiryTarget::NUMBER_OF_CHANNELS {
            return self.as_channels().map(ProfileDetails::Channels);
        }
        let bytes = self.as_bytes();
        if OrganLevel::of_profile(profile).is_some() && target == OrganFeatures::INQUIRY_TARGET {
            OrganFeatures::from_data(bytes).map(ProfileDetails::Organ)
        } else if DrumNoteMapLevel::of_profile(profile).is_some()
            && target == DrumFeatures::INQUIRY_TARGET
        {
            DrumFeatures::from_data(bytes).map(ProfileDetails::Drum)
        } else if profile == ArticulationLevel::One.profile() {
            if target == ArticulationDetails::TARGET {
                ArticulationDetails::try_from(self)
                    .ok()
                    .map(ProfileDetails::Articulation)
            } else if target == CustomSounds::TARGET {
                CustomSounds::try_from(self)
                    .ok()
                    .map(ProfileDetails::ArticulationCustom)
            } else {
                None
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests;
