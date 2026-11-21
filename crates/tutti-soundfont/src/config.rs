// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: the settings follow rustysynth's synthesizer_settings.rs.

//! What a unit is made from.

use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::SoundFontUnit;
use crate::player::Player;
use tutti_buffers::Float;
use tutti_dsp::SimdLanes;
use tutti_midi_protocol::SysexDeviceId;
use tutti_sf2::SoundFont;
use tutti_ump::MidiGroup;

/// The most voices a unit sounds at once: 1 to [`MAX`](Self::MAX).
///
/// A pool allocates every voice when it is built, so a count past the ceiling
/// is refused rather than tried; 4096 is far beyond what a SoundFont plays.
/// `new` refuses and never clamps; `of` is for a literal or a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Polyphony(NonZeroUsize);

impl Polyphony {
    /// The most voices a unit can be built with.
    pub const MAX: usize = 4096;
    /// The polyphony of [`SoundFontConfig::new`]: 64 voices.
    pub const DEFAULT: Self = Self(NonZeroUsize::new(64).unwrap());

    /// `voices` voices, or `None` for none or more than [`MAX`](Self::MAX).
    #[must_use]
    pub const fn new(voices: usize) -> Option<Self> {
        match NonZeroUsize::new(voices) {
            Some(voices) if voices.get() <= Self::MAX => Some(Self(voices)),
            _ => None,
        }
    }

    /// `voices` voices, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `voices` is 0 or more than [`MAX`](Self::MAX).
    #[must_use]
    pub const fn of(voices: usize) -> Self {
        match Self::new(voices) {
            Some(polyphony) => polyphony,
            None => panic!("a polyphony is 1 to 4096 voices"),
        }
    }

    /// The number of voices.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }

    /// The number of voices, as the count the note logic
    /// ([`tutti_voicing::VoiceConfig::poly`]) takes.
    #[must_use]
    pub const fn voices(self) -> NonZeroUsize {
        self.0
    }
}

impl Default for Polyphony {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The refused voice count of [`Polyphony`]'s `TryFrom<usize>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the polyphony must be between 1 and {} voices, but was {}", Polyphony::MAX, .0)]
pub struct PolyphonyError(pub usize);

impl TryFrom<usize> for Polyphony {
    type Error = PolyphonyError;

    fn try_from(voices: usize) -> Result<Self, PolyphonyError> {
        Self::new(voices).ok_or(PolyphonyError(voices))
    }
}

impl From<Polyphony> for NonZeroUsize {
    fn from(polyphony: Polyphony) -> NonZeroUsize {
        polyphony.0
    }
}

impl From<Polyphony> for usize {
    fn from(polyphony: Polyphony) -> usize {
        polyphony.get()
    }
}

/// What a [`SoundFontUnit`] is made from: the decoded SoundFont, and what differs from the
/// defaults, [`polyphony`](Self::polyphony), [`group`](Self::group) and
/// [`device`](Self::device). Convert it with `SoundFontUnit::try_from`, which makes the unit.
///
/// The rate is not here: a unit renders at the rate its graph prepares it at.
///
/// A unit plays one Group's sixteen channels ([`group`](Self::group), the first by default): a
/// Group is a channel space of its own (M2-104 section 2.1.2, Appendix B.1), so another
/// Group's messages are not this unit's.
///
/// ```no_run
/// use tutti_sf2::SoundFont;
/// use tutti_soundfont::{Polyphony, SoundFontConfig, SoundFontUnit};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let font = SoundFont::open("piano.sf2")?;
/// let config = SoundFontConfig::new(font).polyphony(Polyphony::of(16));
/// let unit = SoundFontUnit::<f32>::try_from(config)?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct SoundFontConfig {
    pub(crate) font: Arc<SoundFont>,
    pub(crate) polyphony: Polyphony,
    pub(crate) group: MidiGroup,
    pub(crate) device: SysexDeviceId,
}

impl SoundFontConfig {
    /// A config over `font` with 64 voices, playing the first Group. An `Arc` is shared
    /// rather than copied.
    #[must_use]
    pub fn new(font: impl Into<Arc<SoundFont>>) -> Self {
        Self {
            font: font.into(),
            polyphony: Polyphony::DEFAULT,
            group: MidiGroup::FIRST,
            device: SysexDeviceId::DEFAULT,
        }
    }

    /// This config with `polyphony` voices at most sounding at once.
    #[must_use]
    pub fn polyphony(mut self, polyphony: Polyphony) -> Self {
        self.polyphony = polyphony;
        self
    }

    /// This config playing the channels of `group`, and no other Group's.
    #[must_use]
    pub fn group(mut self, group: MidiGroup) -> Self {
        self.group = group;
        self
    }

    /// This config answering to the Universal System Exclusive device id
    /// `device` besides the all-call (M1 p.40): a message addressed to it, or to
    /// all, is read (Master Volume, Balance and Tuning, General MIDI System On
    /// and Off). Master Volume and Balance keep what arrives on this id apart
    /// from what arrives on the all-call (M1 p.57).
    #[must_use]
    pub fn device(mut self, device: SysexDeviceId) -> Self {
        self.device = device;
        self
    }
}

/// A unit of float `F` over the config's SoundFont, which it shares rather than copies.
///
/// The unit renders at 44.1 kHz until its graph prepares it at the graph's rate ("The
/// rate" on [`SoundFontUnit`]). Allocates the voices and reads every zone of the font
/// once (for the tail), so convert off the audio thread.
///
/// # Errors
///
/// [`Error::NoPlayablePreset`](crate::Error::NoPlayablePreset) for a font none of whose
/// presets has a MIDI bank and program.
impl<F: Float + SimdLanes> TryFrom<SoundFontConfig> for SoundFontUnit<F> {
    type Error = crate::Error;

    fn try_from(config: SoundFontConfig) -> crate::Result<Self> {
        Ok(Self::from_player(Player::new(config)?, 0))
    }
}
