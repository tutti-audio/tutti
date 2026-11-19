//! The Note On Orchestral Articulation Profile (M2-123) in MIDI-CI.
//!
//! The Profile is a Single Channel Profile (M2-123 §3.1) with one Level
//! (§3.2.2): [`ArticulationLevel`] is its Profile Id and says whether a peer has
//! it enabled on a channel. While it is enabled a Note On's attribute is an
//! articulation, which `tutti_ump::NoteArticulation` types;
//! [`ArticulationLevel::read`] reads it only then, because the Profile defines
//! nothing for a channel it is not enabled on and the receiver chooses the
//! articulation itself (§5.1).
//!
//! A receiver declares what it has through the Profile Details Inquiry (§9):
//! [`ArticulationDetails`] is the reply to Inquiry Target `0x01` (the optional
//! features and the variations of each Subclass of the eight defined
//! Classifications) and [`CustomSounds`] the reply to Inquiry Target `0x40`
//! (the six custom Classifications). The Profile defines no Profile Specific
//! Data message. [`SoundSet`] is the two together and applies the fallback of
//! §6.1 to a requested sound.

use tutti_ump::{
    Articulation, ArticulationError, Classification, CustomClassification, MidiChannel,
    NoteArticulation, U4, U7, U14, Voice2,
};

use super::{
    CiDeviceId, DetailsData, InquiryTarget, ProfileDataError, ProfileId, ProfileState,
    ProfileTable, StandardProfile,
};

/// The Level of the Profile (M2-123 §3.2.2): `0x01`, with `0x02` to `0x7F`
/// reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArticulationLevel {
    /// Level `0x01`.
    One,
}

impl ArticulationLevel {
    /// The Profile Id (M2-123 §3.2, Table 4): Bank `0x21`, Number `0x01`,
    /// Version `0x01`, Level `0x01`.
    #[must_use]
    pub const fn profile(self) -> ProfileId {
        ProfileId::Standard(StandardProfile::ARTICULATION)
    }

    /// The Level of the Profile enabled on `channel` in `table`, or `None` when
    /// it is not. The Profile works per channel (§3.1), so only a channel
    /// address counts.
    #[must_use]
    pub fn enabled_on(table: &ProfileTable, channel: MidiChannel) -> Option<Self> {
        let level = Self::One;
        table
            .get(CiDeviceId::Channel(channel), level.profile())
            .is_some_and(|entry| entry.state() == ProfileState::Enabled)
            .then_some(level)
    }

    /// The articulation of a Note On received on `channel`.
    ///
    /// The outer `Option` is the Profile: `None` when it is not enabled on the
    /// channel, because the attribute is then the receiver's to interpret
    /// (§5.1). The inner `Result` is the attribute: the articulation, or why it
    /// is not one.
    ///
    /// # Errors
    ///
    /// When the Profile is enabled, the inner [`ArticulationError`] of
    /// [`NoteArticulation::from_note_on`]: the message is not a Note On, the
    /// attribute type is not an articulation's (a Sender with no articulation
    /// sends `0x00`, §5.1), or the attribute names a reserved Classification,
    /// Subclass or stroke or sets a reserved bit.
    #[must_use]
    pub fn read(
        table: &ProfileTable,
        channel: MidiChannel,
        note_on: &Voice2,
    ) -> Option<Result<NoteArticulation, ArticulationError>> {
        Self::enabled_on(table, channel).map(|_| NoteArticulation::from_note_on(note_on))
    }
}

tutti_units::ranged! {
    /// The number of Variations a Subclass has in a receiver, `0..=16`
    /// (M2-123 Tables 23 and 25); zero declares no sound for the Subclass.
    #[derive(Default, Eq, PartialOrd, Ord, Hash)]
    VariationCount(u8), NotVariationCount, "a Variation count within 0..=16",
    |raw| raw <= 16
}

/// The Variation counts of the 16 Subclasses of one Classification (M2-123
/// Tables 23 and 25).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SubclassCounts([VariationCount; 16]);

impl SubclassCounts {
    /// The counts, index 0 being Subclass `0x0`.
    #[must_use]
    pub const fn new(counts: [VariationCount; 16]) -> Self {
        Self(counts)
    }

    /// The count of one Subclass.
    #[must_use]
    pub const fn get(&self, subclass: U4) -> VariationCount {
        self.0[subclass.get() as usize]
    }

    /// Sets the count of one Subclass.
    pub const fn set(&mut self, subclass: U4, count: VariationCount) {
        self.0[subclass.get() as usize] = count;
    }
}

/// Why Inquiry Target Data is not what the Profile defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ArticulationDetailsError {
    /// The data is not the length the Inquiry Target defines.
    #[error("{got} bytes of Target Data, not {expected}")]
    Length {
        /// The bytes the Target defines.
        expected: usize,
        /// The bytes received.
        got: usize,
    },
    /// A bit M2-123 Table 21 reserves (D5 to D13) is set.
    #[error("reserved feature bits {0:#06x} are set")]
    ReservedFeatures(u16),
    /// A Variation count is above 16.
    #[error("the Variation count {value} at byte {index} is above 16")]
    Count {
        /// The byte, from 0 in the data.
        index: usize,
        /// Its value.
        value: u8,
    },
}

/// The optional features of the Profile a receiver supports (M2-123 Table 21).
/// The bitmap is 14 bits in two 7-bit bytes, D0 the least significant bit of
/// the first (Figure 7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ArticulationFeatures {
    /// D0: Registered Controller `0x20/0x22`, Orchestral Mute Type.
    pub mute_type: bool,
    /// D1: Registered Controller `0x20/0x23`, Orchestral Mute Amount.
    pub mute_amount: bool,
    /// D2: Registered Per-Note Controller `0x0C`, Playing Position.
    pub playing_position: bool,
    /// D3: the Note Off velocity sets the release time.
    pub note_off_velocity: bool,
    /// D4: the custom Classifications are discoverable by Inquiry Target `0x40`.
    pub custom_sound_discovery: bool,
}

impl ArticulationFeatures {
    const RESERVED: u16 = 0x3FFF & !0x1F;

    const fn bits(self) -> u16 {
        self.mute_type as u16
            | (self.mute_amount as u16) << 1
            | (self.playing_position as u16) << 2
            | (self.note_off_velocity as u16) << 3
            | (self.custom_sound_discovery as u16) << 4
    }
}

/// The reply to Profile Details Inquiry for Inquiry Target `0x01` (M2-123
/// §9.1): the optional features and, for each of the eight defined
/// Classifications, the Variations of each Subclass. The Target Data is 130
/// bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ArticulationDetails {
    /// The optional features supported.
    pub features: ArticulationFeatures,
    sounds: [SubclassCounts; 8],
}

/// Reads `N` Classification declarations of 16 count bytes each from `bytes`,
/// whose first byte is byte `base` of the Target Data.
fn read_counts<const N: usize>(
    bytes: &[u8],
    base: usize,
) -> Result<[SubclassCounts; N], ArticulationDetailsError> {
    let mut out = [SubclassCounts::default(); N];
    let (declarations, _) = bytes.as_chunks::<16>();
    for (class, (counts, chunk)) in out.iter_mut().zip(declarations).enumerate() {
        for (subclass, &value) in chunk.iter().enumerate() {
            counts.0[subclass] =
                VariationCount::new(value).ok_or(ArticulationDetailsError::Count {
                    index: base + class * 16 + subclass,
                    value,
                })?;
        }
    }
    Ok(out)
}

/// The count bytes of `classes`, 16 each.
fn write_counts(classes: &[SubclassCounts], out: &mut Vec<u8>) {
    for counts in classes {
        out.extend(counts.0.iter().map(|count| count.get()));
    }
}

/// The data, whose bytes are 7-bit by construction.
fn data(bytes: Vec<u8>) -> DetailsData {
    match DetailsData::new(bytes) {
        Ok(data) => data,
        Err(ProfileDataError::NotSevenBit | ProfileDataError::TooLong) => {
            unreachable!("the bitmap bytes are 7-bit and the counts at most 16, 130 bytes long")
        }
    }
}

impl TryFrom<&DetailsData> for ArticulationDetails {
    type Error = ArticulationDetailsError;

    /// The reply to Inquiry Target `0x01` in `data`.
    ///
    /// # Errors
    ///
    /// [`ArticulationDetailsError`] for data that is not [`LEN`](Self::LEN)
    /// bytes, sets a reserved feature bit, or has a count above 16.
    fn try_from(data: &DetailsData) -> Result<Self, ArticulationDetailsError> {
        let bytes = data.as_bytes();
        if bytes.len() != Self::LEN {
            return Err(ArticulationDetailsError::Length {
                expected: Self::LEN,
                got: bytes.len(),
            });
        }
        // The bytes are 7-bit: `DetailsData::new` refused any other.
        let bits = U14::from_halves(U7::masked(bytes[1]), U7::masked(bytes[0])).get();
        if bits & ArticulationFeatures::RESERVED != 0 {
            return Err(ArticulationDetailsError::ReservedFeatures(
                bits & ArticulationFeatures::RESERVED,
            ));
        }
        Ok(Self {
            features: ArticulationFeatures {
                mute_type: bits & 0x01 != 0,
                mute_amount: bits & 0x02 != 0,
                playing_position: bits & 0x04 != 0,
                note_off_velocity: bits & 0x08 != 0,
                custom_sound_discovery: bits & 0x10 != 0,
            },
            sounds: read_counts(&bytes[2..], 2)?,
        })
    }
}

impl ArticulationDetails {
    /// Inquiry Target `0x01`, Profile Optional Features Supported (M2-123
    /// §9.1.1).
    pub const TARGET: InquiryTarget = InquiryTarget::new(U7::of(0x01));

    /// The Target Data length: the two bitmap bytes and 16 counts for each of
    /// eight Classifications.
    pub const LEN: usize = 2 + 8 * 16;

    /// The declaration of the features and the Variations of each Subclass of
    /// the Classifications `0x10` to `0x17`, in that order.
    #[must_use]
    pub const fn new(features: ArticulationFeatures, sounds: [SubclassCounts; 8]) -> Self {
        Self { features, sounds }
    }

    /// The counts of a defined Classification (`0x10` to `0x17`), or `None` for
    /// a custom one, which [`CustomSounds`] declares.
    #[must_use]
    pub const fn counts(&self, classification: Classification) -> Option<&SubclassCounts> {
        match classification {
            Classification::Custom(_) => None,
            other => Some(&self.sounds[(other.attribute_type() - 0x10) as usize]),
        }
    }

    /// The Target Data of the reply (§9.1.2, Tables 21 and 22).
    ///
    /// ```
    /// use tutti_midi_ci::ArticulationDetails;
    ///
    /// // The bitmap and 16 counts for each of eight Classifications.
    /// let data = ArticulationDetails::default().to_data();
    /// assert_eq!(data.as_bytes().len(), ArticulationDetails::LEN);
    /// assert_eq!(ArticulationDetails::LEN, 130);
    /// ```
    #[must_use]
    pub fn to_data(&self) -> DetailsData {
        let bits = self.features.bits();
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend(U14::masked(bits).to_le7());
        write_counts(&self.sounds, &mut out);
        data(out)
    }
}

/// The reply to Profile Details Inquiry for Inquiry Target `0x40` (M2-123
/// §9.2): the Variations of each Subclass of the six custom Classifications
/// `0x1A` to `0x1F`. The Target Data is 96 bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CustomSounds([SubclassCounts; 6]);

impl TryFrom<&DetailsData> for CustomSounds {
    type Error = ArticulationDetailsError;

    /// The reply to Inquiry Target `0x40` in `data`.
    ///
    /// # Errors
    ///
    /// [`ArticulationDetailsError`] for data that is not [`LEN`](Self::LEN)
    /// bytes or has a count above 16.
    fn try_from(data: &DetailsData) -> Result<Self, ArticulationDetailsError> {
        let bytes = data.as_bytes();
        if bytes.len() != Self::LEN {
            return Err(ArticulationDetailsError::Length {
                expected: Self::LEN,
                got: bytes.len(),
            });
        }
        Ok(Self(read_counts(bytes, 0)?))
    }
}

impl CustomSounds {
    /// Inquiry Target `0x40`, Profile Specific, Discover Manufacturer Specific
    /// Sounds (M2-123 §9.2.1).
    pub const TARGET: InquiryTarget = InquiryTarget::new(U7::of(0x40));

    /// The Target Data length: 16 counts for each of six Classifications.
    pub const LEN: usize = 6 * 16;

    /// The declaration for the Classifications `0x1A` to `0x1F`, in that order.
    #[must_use]
    pub const fn new(sounds: [SubclassCounts; 6]) -> Self {
        Self(sounds)
    }

    /// The counts of a custom Classification.
    #[must_use]
    pub const fn counts(&self, classification: CustomClassification) -> &SubclassCounts {
        &self.0[(classification.attribute_type() - 0x1A) as usize]
    }

    /// The Target Data of the reply (§9.2.2, Tables 24 and 25).
    #[must_use]
    pub fn to_data(&self) -> DetailsData {
        let mut out = Vec::with_capacity(Self::LEN);
        write_counts(&self.0, &mut out);
        data(out)
    }
}

/// One sound of a receiver: an articulation and the Variation of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sound {
    /// The Classification and Subclass.
    pub articulation: Articulation,
    /// The Variation within the Subclass.
    pub variation: U4,
}

/// The sounds a receiver declared, with the fallback of M2-123 §6.1.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct SoundSet {
    details: ArticulationDetails,
    custom: CustomSounds,
}

impl SoundSet {
    /// The sounds of a receiver. A receiver that does not declare its custom
    /// Classifications is [`CustomSounds::default`]: it has none.
    #[must_use]
    pub const fn new(details: ArticulationDetails, custom: CustomSounds) -> Self {
        Self { details, custom }
    }

    /// How many Variations the receiver has of an articulation's Subclass.
    #[must_use]
    pub fn variations(&self, articulation: Articulation) -> VariationCount {
        let subclass = articulation.subclass();
        match articulation.classification() {
            Classification::Custom(custom) => self.custom.counts(custom).get(subclass),
            standard => self
                .details
                .counts(standard)
                .map_or_else(VariationCount::default, |counts| counts.get(subclass)),
        }
    }

    /// The sound a receiver plays for a requested one (M2-123 §6.1), or `None`
    /// when it has not even the Normal sustain the Profile requires (§5.2.1).
    ///
    /// The rules, in order. The requested Variation of the Subclass, when the
    /// receiver has it. Otherwise a Variation within the same Subclass: the
    /// first, because §6.1 leaves the choice. Otherwise the same rules for
    /// Subclass `0x0` of the same Classification, then for Subclass `0x0` of
    /// Classification `0x10`: the spec's "Classification 1", the first of the
    /// Classifications it numbers from 1 (§4.1).
    #[must_use]
    pub fn resolve(&self, wanted: Sound) -> Option<Sound> {
        let class = wanted.articulation.classification();
        let subclass_zero = Articulation::from_parts(class, U4::of(0)).ok()?;
        [wanted.articulation, subclass_zero, Articulation::NORMAL]
            .into_iter()
            .find_map(|articulation| {
                let count = self.variations(articulation).get();
                (count > 0).then(|| Sound {
                    articulation,
                    variation: if wanted.variation.get() < count {
                        wanted.variation
                    } else {
                        U4::of(0)
                    },
                })
            })
    }
}

#[cfg(test)]
mod tests;
