// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: the settings follow rustysynth's synthesizer_settings.rs.

//! What a unit is made from.

use std::num::NonZeroUsize;
use std::sync::Arc;

use std::collections::HashMap;

use crate::error::Error;
use tutti_graph::Shared;
use tutti_midi_protocol::SysexDeviceId;
use tutti_sf2::SoundFont;
use tutti_ump::{MidiGroup, U7};

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
/// [`device`](Self::device). Convert it with `SoundFontUnit::from`, which makes the unit.
///
/// A config exists only over a font with a playable preset ([`new`](Self::new) refuses
/// another), so the unit's build cannot fail. It is a [`NodeConfig`] with no param: every
/// field is structure, and a font is the same one when it is the same allocation.
///
/// The rate is not here: a unit renders at the rate its graph prepares it at.
///
/// A unit plays one Group's sixteen channels ([`group`](Self::group), the first by default): a
/// Group is a channel space of its own (M2-104 section 2.1.2, Appendix B.1), so another
/// Group's messages are not this unit's.
///
/// ```no_run
/// use std::sync::Arc;
/// use tutti_sf2::SoundFont;
/// use tutti_soundfont::{Polyphony, SoundFontConfig, SoundFontUnit};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let font = SoundFont::open("piano.sf2")?;
/// let config = SoundFontConfig::new(Arc::new(font))?.polyphony(Polyphony::of(16));
/// let unit = SoundFontUnit::<f32>::from(config);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct SoundFontConfig {
    pub(crate) font: Shared<SoundFont>,
    /// The font's playable presets, read once in [`new`](Self::new).
    pub(crate) index: Arc<PresetIndex>,
    pub(crate) polyphony: Polyphony,
    pub(crate) group: MidiGroup,
    pub(crate) device: SysexDeviceId,
}

impl SoundFontConfig {
    /// A config over `font` with 64 voices, playing the first Group. An `Arc` is shared
    /// rather than copied.
    ///
    /// # Errors
    ///
    /// [`Error::NoPlayablePreset`](crate::Error::NoPlayablePreset) for a font none of whose
    /// presets has a MIDI bank and program.
    pub fn new(font: impl Into<Shared<SoundFont>>) -> crate::Result<Self> {
        let font = font.into();
        let index = Arc::new(PresetIndex::of(&font)?);
        Ok(Self {
            font,
            index,
            polyphony: Polyphony::DEFAULT,
            group: MidiGroup::FIRST,
            device: SysexDeviceId::DEFAULT,
        })
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

/// The key of a preset in the player's lookup: its bank in the upper 16 bits,
/// its program in the lower.
pub(crate) fn preset_id(bank: tutti_sf2::Bank, program: U7) -> i32 {
    (i32::from(bank.get()) << 16) | i32::from(program.get())
}

/// Which preset of a font each bank and program selects, and the one a channel plays when
/// its selection names none: the font's playable presets, read once by
/// [`PresetIndex::of`] when the config is made.
#[derive(Debug)]
pub(crate) struct PresetIndex {
    pub(crate) presets: HashMap<i32, usize>,
    pub(crate) default_preset: usize,
}

impl PresetIndex {
    /// The index of `font`'s presets.
    ///
    /// A preset whose bank or program is not MIDI's cannot be played (SF2 2.04 section
    /// 7.2): it is kept in the font and not looked up. Of two presets with one bank and
    /// program, the first in the file is the active one (section 7.2). The default is the
    /// preset of the smallest id, which is the piano of a General MIDI font.
    ///
    /// # Errors
    ///
    /// [`Error::NoPlayablePreset`] when no preset of the font has a MIDI bank and program: a
    /// unit over it would sound a preset the file says cannot be played, or nothing at all.
    pub(crate) fn of(font: &SoundFont) -> crate::Result<Self> {
        let mut presets: HashMap<i32, usize> = HashMap::new();
        let mut default_preset: Option<(i32, usize)> = None;
        for (i, preset) in font.presets().iter().enumerate() {
            let (Some(bank), Some(patch)) = (preset.bank(), preset.patch()) else {
                continue;
            };
            let id = preset_id(bank, patch);
            presets.entry(id).or_insert(i);
            if default_preset.is_none_or(|(least, _)| id < least) {
                default_preset = Some((id, i));
            }
        }
        let (_, default_preset) = default_preset.ok_or(Error::NoPlayablePreset)?;
        Ok(Self {
            presets,
            default_preset,
        })
    }
}
