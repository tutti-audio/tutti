//! A node's settings: the cells a session sets and reads through the node's key.

use std::any::Any;
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

/// One setting cell of a node as `NodeParts` carries it: its kind and the cell,
/// the `Arc<AtomicCell<Word>>` the node reads once per block, behind `Any`.
pub struct ErasedSetting {
    kind: SettingKind,
    cell: Arc<dyn Any + Send + Sync>,
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
            kind: SettingKind::from_u16(K)
                .expect("`HasSetting<K>` is declared only for a `SettingKind`"),
            cell,
        }
    }

    /// The setting's kind.
    #[must_use]
    pub const fn kind(&self) -> SettingKind {
        self.kind
    }

    /// The cell, when it holds the word `W`.
    #[must_use]
    pub fn cell<W: AtomicWord + 'static>(&self) -> Option<Arc<AtomicCell<W>>> {
        Arc::clone(&self.cell).downcast().ok()
    }
}

impl std::fmt::Debug for ErasedSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErasedSetting")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}
