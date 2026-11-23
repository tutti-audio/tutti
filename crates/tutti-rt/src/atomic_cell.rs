//! [`AtomicCell`]: a value that is one word by its encoding (a mode, a switch
//! set, a pair of counts), stored whole in one atomic, and
//! [`atomic_word!`](crate::atomic_word), the encoding of a fieldless enum.
//!
//! A cell is stored by its encoding: [`AtomicWord::to_word`] writes the word and
//! [`AtomicWord::from_word`] reads it back, so a load returns a value one store
//! wrote, never fields of two. The `loom` model in `tests/atomic_cell_loom.rs`
//! runs this code.

use crate::sync::{AtomicU64, Ordering};
use tutti_units::SampleRate;

/// A value stored in one atomic word by its encoding.
///
/// `to_word` and `from_word` are pure and deterministic, so `from_word(to_word(v))`
/// is `Some(v)` every time, for every `v`. A word no value encodes
/// reads `None`; an [`AtomicCell`] never holds one, because only `to_word` writes
/// it. [`atomic_word!`](crate::atomic_word) writes this for a fieldless enum.
pub trait AtomicWord: Copy {
    /// The word that encodes `self`.
    fn to_word(self) -> u64;

    /// The value `word` encodes, or `None` for a word no value encodes.
    fn from_word(word: u64) -> Option<Self>;
}

/// One [`AtomicWord`] value in one atomic word: a control thread stores it, the
/// audio thread reads it once per block.
///
/// Wait-free on both sides, no allocation: a `store` is one `Release` store and a
/// `load` one `Acquire` load, and a `swap` one `AcqRel` exchange.
/// A store lands at once: the next `load` on any thread that observes the
/// store reads the new value, whole.
///
/// `load` is total. Every write goes through [`AtomicWord::to_word`], and `new`
/// and `store` assert (on the control side, naming the type) that the word they
/// are about to store decodes, so the cell never holds a word `from_word`
/// refuses and `load` has no failing path to handle on the audio thread.
///
/// ```
/// use tutti_rt::{AtomicCell, atomic_word};
///
/// #[derive(Clone, Copy, Debug, PartialEq)]
/// #[repr(u8)]
/// enum Mode { Off, On }
/// atomic_word!(Mode { Off, On });
///
/// let mode = AtomicCell::new(Mode::Off);
/// mode.store(Mode::On);
/// assert_eq!(mode.load(), Mode::On);
/// ```
pub struct AtomicCell<T> {
    word: AtomicU64,
    _value: std::marker::PhantomData<fn() -> T>,
}

impl<T: AtomicWord> AtomicCell<T> {
    /// The word `value` encodes.
    ///
    /// # Panics
    ///
    /// When `T`'s `from_word` refuses its own `to_word`: a broken
    /// [`AtomicWord`] impl, named at the write, never at a read.
    #[track_caller]
    fn encode(value: T) -> u64 {
        let word = value.to_word();
        assert!(
            T::from_word(word).is_some(),
            "{}'s `from_word` refuses the word its `to_word` wrote",
            std::any::type_name::<T>()
        );
        word
    }

    /// A cell holding `value`.
    ///
    /// # Panics
    ///
    /// When `T`'s [`AtomicWord`] impl does not decode its own encoding.
    #[must_use]
    #[track_caller]
    pub fn new(value: T) -> Self {
        Self {
            word: AtomicU64::new(Self::encode(value)),
            _value: std::marker::PhantomData,
        }
    }

    /// Replaces the value. Wait-free.
    ///
    /// # Panics
    ///
    /// When `T`'s [`AtomicWord`] impl does not decode its own encoding.
    #[inline]
    #[track_caller]
    pub fn store(&self, value: T) {
        self.word.store(Self::encode(value), Ordering::Release);
    }

    /// Replaces the value and returns the one it replaced, in one atomic step:
    /// a store from another thread lands before or after it, never between the
    /// read and the write, so a taker that swaps in an empty value loses no
    /// request. Wait-free.
    ///
    /// # Panics
    ///
    /// When `T`'s [`AtomicWord`] impl does not decode its own encoding.
    #[inline]
    #[track_caller]
    pub fn swap(&self, value: T) -> T {
        Self::decode(self.word.swap(Self::encode(value), Ordering::AcqRel))
    }

    /// The value, as one `store` left it. Wait-free.
    ///
    /// # Panics
    ///
    /// Never, for an impl that holds the [`AtomicWord`] contract: the cell
    /// holds only a word `new` or `store` checked decodes. An impure impl that
    /// stops decoding its own word panics here, at the read.
    #[inline]
    pub fn load(&self) -> T {
        Self::decode(self.word.load(Ordering::Acquire))
    }

    /// The value `word` encodes: the cell holds only words `encode` checked.
    #[inline]
    fn decode(word: u64) -> T {
        T::from_word(word)
            .expect("an AtomicCell holds only words its `to_word` wrote, which `from_word` decodes")
    }
}

/// Writes [`AtomicWord`] for the unsigned integers: the word is the value, and a
/// word past the type's range decodes to `None`.
macro_rules! word_of_uint {
    ($($ty:ty),+) => {$(
        impl AtomicWord for $ty {
            fn to_word(self) -> u64 {
                u64::from(self)
            }

            fn from_word(word: u64) -> Option<Self> {
                Self::try_from(word).ok()
            }
        }
    )+};
}
word_of_uint!(u8, u16, u32, u64);

/// `0` is `false` and `1` is `true`; any other word decodes to `None`.
impl AtomicWord for bool {
    fn to_word(self) -> u64 {
        u64::from(self)
    }

    fn from_word(word: u64) -> Option<Self> {
        match word {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
}

/// The first element in the low half of the word, the second in the high half:
/// a pair stored whole, so a load never pairs one store's first with another's
/// second.
impl AtomicWord for [u32; 2] {
    fn to_word(self) -> u64 {
        u64::from(self[0]) | u64::from(self[1]) << 32
    }

    fn from_word(word: u64) -> Option<Self> {
        // Each `as u32` keeps its half's 32 bits: the truncation is the decode.
        Some([word as u32, (word >> 32) as u32])
    }
}

/// The rate's `f64` bits. A word whose bits are no rate (zero, negative, not
/// finite) decodes to `None`.
impl AtomicWord for SampleRate {
    fn to_word(self) -> u64 {
        self.get().to_bits()
    }

    fn from_word(word: u64) -> Option<Self> {
        Self::new(f64::from_bits(word))
    }
}

/// `None` is word `0`, the bits of `0.0`, which no [`SampleRate`] has; a
/// `Some` is the rate's bits. So an `AtomicCell<Option<SampleRate>>` is a rate
/// one thread parks for another to `swap(None)` out; parking over a rate nobody
/// took replaces it.
impl AtomicWord for Option<SampleRate> {
    fn to_word(self) -> u64 {
        self.map_or(0, AtomicWord::to_word)
    }

    fn from_word(word: u64) -> Option<Self> {
        match word {
            0 => Some(None),
            bits => SampleRate::from_word(bits).map(Some),
        }
    }
}

impl<T: AtomicWord + std::fmt::Debug> std::fmt::Debug for AtomicCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AtomicCell").field(&self.load()).finish()
    }
}

/// Writes [`AtomicWord`] for a fieldless enum: the word is the variant's
/// discriminant, and a word that is no listed variant's discriminant decodes
/// to `None`.
///
/// Every variant is listed once; the match that encodes is exhaustive, so a
/// variant left out does not compile.
///
/// ```
/// use tutti_rt::{AtomicWord, atomic_word};
///
/// #[derive(Clone, Copy, Debug, PartialEq)]
/// #[repr(u8)]
/// enum Rate { Slow = 1, Fast = 4 }
/// atomic_word!(Rate { Slow, Fast });
///
/// assert_eq!(Rate::Fast.to_word(), 4);
/// assert_eq!(Rate::from_word(1), Some(Rate::Slow));
/// assert_eq!(Rate::from_word(2), None);
/// ```
#[macro_export]
macro_rules! atomic_word {
    ($ty:ty { $($variant:ident),+ $(,)? }) => {
        impl $crate::AtomicWord for $ty {
            fn to_word(self) -> u64 {
                match self {
                    $(<$ty>::$variant => <$ty>::$variant as u64,)+
                }
            }

            fn from_word(word: u64) -> Option<Self> {
                $(
                    if word == <$ty>::$variant as u64 {
                        return Some(<$ty>::$variant);
                    }
                )+
                None
            }
        }
    };
}

#[cfg(all(test, not(loom)))]
mod tests;
