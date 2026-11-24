//! A node's settings: the cells a session sets and reads through the node's key.

use std::any::TypeId;
use std::sync::Arc;

use tutti_rt::{AtomicCell, AtomicWord};
use tutti_units::SettingKind;

/// That the node `Self` has a setting of the kind `K` (a [`SettingKind`] as
/// `u16`), whose value is a `Word`. What a typed key's method for that kind
/// needs, and what [`declare_params!`](crate::declare_params) writes from a
/// cells form's `settings` entry.
pub trait HasSetting<const K: u16> {
    /// The enum the setting holds, one atomic word by its encoding.
    type Word: AtomicWord + 'static;
}

/// A [`Params`](crate::Params) node has the settings its node does.
impl<N: HasSetting<K>, const K: u16> HasSetting<K> for crate::Params<N> {
    type Word = N::Word;
}

/// A setting's cell as one atomic word, whatever the enum it holds.
pub trait SettingCell: Send + Sync {
    /// The word the cell holds now.
    fn load_word(&self) -> u64;

    /// Stores `word`; whether it decodes as a value of the cell's enum (a word
    /// that does not leaves the cell as it was).
    fn store_word(&self, word: u64) -> bool;
}

impl<W: AtomicWord> SettingCell for AtomicCell<W> {
    fn load_word(&self) -> u64 {
        self.load().to_word()
    }

    fn store_word(&self, word: u64) -> bool {
        W::from_word(word)
            .inspect(|&value| self.store(value))
            .is_some()
    }
}

/// One setting cell of a node as `NodeParts` carries it: its kind, the
/// [`TypeId`] of the enum it holds, and the cell as a [`SettingCell`].
#[derive(Clone)]
pub struct ErasedSetting {
    kind: SettingKind,
    word: TypeId,
    cell: Arc<dyn SettingCell>,
}

impl ErasedSetting {
    /// The setting `N` declares of the kind `K`, held in `cell`.
    ///
    /// # Panics
    ///
    /// When `K` is not a [`SettingKind`]: `HasSetting<K>` is declared only for
    /// the kinds.
    #[must_use]
    pub fn new<N: HasSetting<K> + ?Sized, const K: u16>(cell: Arc<AtomicCell<N::Word>>) -> Self {
        Self {
            kind: SettingKind::try_from(K)
                .expect("`HasSetting<K>` is declared only for a `SettingKind`"),
            word: TypeId::of::<N::Word>(),
            cell,
        }
    }

    /// The setting's kind.
    #[must_use]
    pub const fn kind(&self) -> SettingKind {
        self.kind
    }

    /// The [`TypeId`] of the enum the cell holds.
    #[must_use]
    pub const fn word(&self) -> TypeId {
        self.word
    }

    /// The value the cell holds, `None` when `W` is not the enum it holds.
    #[must_use]
    pub fn load<W: AtomicWord + 'static>(&self) -> Option<W> {
        (self.word == TypeId::of::<W>())
            .then(|| W::from_word(self.cell.load_word()))
            .flatten()
    }

    /// Stores `value`; whether `W` is the enum the cell holds (a value of
    /// another enum leaves the cell as it was).
    pub fn store<W: AtomicWord + 'static>(&self, value: W) -> bool {
        self.word == TypeId::of::<W>() && self.cell.store_word(value.to_word())
    }

    /// The cell.
    #[must_use]
    pub fn cell(&self) -> &Arc<dyn SettingCell> {
        &self.cell
    }
}

impl std::fmt::Debug for ErasedSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErasedSetting")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}
