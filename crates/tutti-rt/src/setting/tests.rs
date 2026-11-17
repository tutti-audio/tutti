use super::*;

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
enum Mode {
    Off,
    Preroll,
    Always = 7,
}
setting_word!(Mode {
    Off,
    Preroll,
    Always
});

/// A word is the variant's discriminant, and every variant decodes back.
#[test]
fn every_variant_round_trips_through_its_discriminant() {
    for (mode, word) in [(Mode::Off, 0), (Mode::Preroll, 1), (Mode::Always, 7)] {
        assert_eq!(mode.to_word(), word);
        assert_eq!(Mode::from_word(word), Some(mode));
    }
}

/// A word that is no variant's discriminant is refused: the gap between
/// explicit discriminants and the words past the last.
#[test]
fn a_word_no_variant_encodes_is_refused() {
    for word in [2, 6, 8, 255, 256, u64::MAX] {
        assert_eq!(Mode::from_word(word), None, "word {word}");
    }
}

/// A cell returns the last value set, for each variant in turn.
#[test]
fn a_get_returns_the_last_set() {
    let cell = Setting::new(Mode::Preroll);
    assert_eq!(cell.get(), Mode::Preroll);
    for mode in [Mode::Always, Mode::Off, Mode::Always] {
        cell.set(mode);
        assert_eq!(cell.get(), mode);
    }
}

/// A `SettingWord` whose decode refuses its own encoding is caught at the
/// write that would have stored the word.
#[test]
#[should_panic(expected = "refuses the word")]
fn a_broken_encoding_is_refused_at_the_write() {
    #[derive(Clone, Copy)]
    struct Broken;
    impl SettingWord for Broken {
        fn to_word(self) -> u64 {
            1
        }
        fn from_word(_: u64) -> Option<Self> {
            None
        }
    }
    let _ = Setting::new(Broken);
}
