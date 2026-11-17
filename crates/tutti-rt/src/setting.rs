//! [`Setting`]: a value that is one word by its encoding (a mode, a switch
//! set), stored whole in one atomic, and [`setting_word!`](crate::setting_word),
//! the encoding of a fieldless enum.
//!
//! [`PackedAtomic`](crate::PackedAtomic) stores a value's bytes and needs every
//! byte pattern to be a value, which no enum satisfies. A setting is stored by
//! its encoding instead: [`SettingWord::to_word`] writes the word and
//! [`SettingWord::from_word`] reads it back. The `loom` model in
//! `tests/setting_loom.rs` runs this code.

use sync::{AtomicU64, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU64, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU64, Ordering};
}

/// A value stored in one atomic word by its encoding.
///
/// `to_word` and `from_word` are pure and deterministic, so `from_word(to_word(v))`
/// is `Some(v)` every time, for every `v`. A word no value encodes
/// reads `None`; a [`Setting`] never holds one, because only `to_word` writes
/// it. [`setting_word!`](crate::setting_word) writes this for a fieldless enum.
pub trait SettingWord: Copy {
    /// The word that encodes `self`.
    fn to_word(self) -> u64;

    /// The value `word` encodes, or `None` for a word no value encodes.
    fn from_word(word: u64) -> Option<Self>;
}

/// One [`SettingWord`] value in one atomic word: a control thread sets it, the
/// audio thread reads it once per block.
///
/// Wait-free on both sides, no allocation: a `set` is one `Release` store and a
/// `get` one `Acquire` load, as [`PackedAtomic`](crate::PackedAtomic)'s are.
/// A setting lands at once: the next `get` on any thread that observes the
/// store reads the new value, whole.
///
/// `get` is total. Every write goes through [`SettingWord::to_word`], and `new`
/// and `set` assert (on the control side, naming the type) that the word they
/// are about to store decodes, so the cell never holds a word `from_word`
/// refuses and `get` has no failing path to handle on the audio thread.
///
/// ```
/// use tutti_rt::{Setting, setting_word};
///
/// #[derive(Clone, Copy, Debug, PartialEq)]
/// #[repr(u8)]
/// enum Mode { Off, On }
/// setting_word!(Mode { Off, On });
///
/// let mode = Setting::new(Mode::Off);
/// mode.set(Mode::On);
/// assert_eq!(mode.get(), Mode::On);
/// ```
pub struct Setting<T> {
    word: AtomicU64,
    _value: std::marker::PhantomData<fn() -> T>,
}

impl<T: SettingWord> Setting<T> {
    /// The word `value` encodes.
    ///
    /// # Panics
    ///
    /// When `T`'s `from_word` refuses its own `to_word`: a broken
    /// [`SettingWord`] impl, named at the write, never at a read.
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
    /// When `T`'s [`SettingWord`] impl does not decode its own encoding.
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
    /// When `T`'s [`SettingWord`] impl does not decode its own encoding.
    #[inline]
    #[track_caller]
    pub fn set(&self, value: T) {
        self.word.store(Self::encode(value), Ordering::Release);
    }

    /// The value, as one `set` left it. Wait-free.
    ///
    /// # Panics
    ///
    /// Never, for an impl that holds the [`SettingWord`] contract: the cell
    /// holds only a word `new` or `set` checked decodes. An impure impl that
    /// stops decoding its own word panics here, at the read.
    #[inline]
    pub fn get(&self) -> T {
        T::from_word(self.word.load(Ordering::Acquire))
            .expect("a Setting holds only words its `to_word` wrote, which `from_word` decodes")
    }
}

impl<T: SettingWord + std::fmt::Debug> std::fmt::Debug for Setting<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Setting").field(&self.get()).finish()
    }
}

/// Writes [`SettingWord`] for a fieldless enum: the word is the variant's
/// discriminant, and a word that is no listed variant's discriminant decodes
/// to `None`.
///
/// Every variant is listed once; the match that encodes is exhaustive, so a
/// variant left out does not compile.
///
/// ```
/// use tutti_rt::{SettingWord, setting_word};
///
/// #[derive(Clone, Copy, Debug, PartialEq)]
/// #[repr(u8)]
/// enum Rate { Slow = 1, Fast = 4 }
/// setting_word!(Rate { Slow, Fast });
///
/// assert_eq!(Rate::Fast.to_word(), 4);
/// assert_eq!(Rate::from_word(1), Some(Rate::Slow));
/// assert_eq!(Rate::from_word(2), None);
/// ```
#[macro_export]
macro_rules! setting_word {
    ($ty:ty { $($variant:ident),+ $(,)? }) => {
        impl $crate::SettingWord for $ty {
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
