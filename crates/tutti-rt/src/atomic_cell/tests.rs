use super::*;

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
enum Mode {
    Off,
    Preroll,
    Always = 7,
}
atomic_word!(Mode {
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
    let cell = AtomicCell::new(Mode::Preroll);
    assert_eq!(cell.load(), Mode::Preroll);
    for mode in [Mode::Always, Mode::Off, Mode::Always] {
        cell.store(mode);
        assert_eq!(cell.load(), mode);
    }
}

/// A `AtomicWord` whose decode refuses its own encoding is caught at the
/// write that would have stored the word.
#[test]
#[should_panic(expected = "refuses the word")]
fn a_broken_encoding_is_refused_at_the_write() {
    #[derive(Clone, Copy)]
    struct Broken;
    impl AtomicWord for Broken {
        fn to_word(self) -> u64 {
            1
        }
        fn from_word(_: u64) -> Option<Self> {
            None
        }
    }
    let _ = AtomicCell::new(Broken);
}

/// `swap` returns the value it replaced and leaves the new one.
#[test]
fn swap_returns_the_replaced_value() {
    let cell = AtomicCell::new([0u32, 0]);
    cell.store([7, 8]);
    assert_eq!(cell.swap([0, 0]), [7, 8]);
    assert_eq!(cell.load(), [0, 0]);
    assert_eq!(cell.swap([1, 2]), [0, 0]);
    assert_eq!(cell.load(), [1, 2]);
}

/// A pair keeps both halves apart, at the extremes of each.
#[test]
fn a_pair_of_words_round_trips_both_halves() {
    for pair in [[0, 0], [u32::MAX, 0], [0, u32::MAX], [1, u32::MAX - 1]] {
        let cell = AtomicCell::new(pair);
        assert_eq!(cell.load(), pair);
    }
    assert_eq!([1u32, 2].to_word(), 1 | 2 << 32);
}

/// An integer is its own word, and a word past its range decodes to `None`.
#[test]
fn an_integer_is_its_own_word_and_a_wider_word_is_refused() {
    assert_eq!(AtomicCell::new(0xA5u8).load(), 0xA5);
    assert_eq!(AtomicCell::new(0xBEEFu16).load(), 0xBEEF);
    assert_eq!(AtomicCell::new(u32::MAX).load(), u32::MAX);
    assert_eq!(AtomicCell::new(u64::MAX - 1).load(), u64::MAX - 1);
    assert_eq!(u8::from_word(256), None);
    assert_eq!(u16::from_word(1 << 16), None);
    assert_eq!(u32::from_word(1 << 32), None);
}

/// A `bool` is `0` or `1`, and any other word decodes to `None`.
#[test]
fn a_bool_is_zero_or_one() {
    let flag = AtomicCell::new(false);
    flag.store(true);
    assert!(flag.load());
    assert_eq!(bool::from_word(2), None);
}

/// A pending rate is taken once and the latest parked wins.
#[test]
fn a_pending_rate_is_taken_once_and_the_latest_wins() {
    let cell = AtomicCell::new(None);
    assert_eq!(cell.swap(None), None, "a new cell is empty");
    cell.store(Some(SampleRate::SR_44K1));
    cell.store(Some(SampleRate::SR_48K));
    assert_eq!(cell.swap(None), Some(SampleRate::SR_48K));
    assert_eq!(cell.swap(None), None, "a take empties the cell");
}

/// A rate loads what was stored and `swap` answers the old one; a word that is
/// no rate is refused.
#[test]
fn a_rate_loads_what_was_stored_and_swap_answers_the_old_one() {
    let cell = AtomicCell::new(SampleRate::SR_44K1);
    assert_eq!(cell.load(), SampleRate::SR_44K1);
    cell.store(SampleRate::SR_96K);
    assert_eq!(cell.swap(SampleRate::SR_48K), SampleRate::SR_96K);
    assert_eq!(cell.load(), SampleRate::SR_48K);
    assert_eq!(SampleRate::from_word(0), None);
    assert_eq!(SampleRate::from_word((-1.0f64).to_bits()), None);
}

/// An optional beat survives the word for every finite beat, zero and negatives
/// included, and `None` is a word no finite beat has.
#[test]
fn an_optional_beat_round_trips_and_none_is_no_beat() {
    let cell = AtomicCell::new(None::<Beat>);
    assert_eq!(cell.load(), None);
    for raw in [0.0, -0.0, -3.5, 1.0e-300, 1.0e12] {
        let beat = Beat::new(raw);
        cell.store(Some(beat));
        assert_eq!(cell.load(), Some(beat));
    }
    assert_eq!(<Option<Beat>>::from_word(f64::NAN.to_bits()), None);
    assert_eq!(<Option<Beat>>::from_word(f64::INFINITY.to_bits()), None);
}
