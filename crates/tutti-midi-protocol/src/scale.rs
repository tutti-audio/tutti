//! Where M2-115 section 4.1 zero-extends rather than scales.

/// Whether the Registered Controller `index` is zero-extended (M2-115 section
/// 4.1: "Translators shall use Zero-Extension Scaling on all Registered
/// Controller (RPN) Messages where the index (RPN LSB) is 0-31") rather than
/// scaled Min-Center-Max (section 3.1, index 32 to 127), whatever the bank.
/// Both translators go by it, so the two cannot disagree.
#[inline]
pub(crate) const fn zero_extended(index: tutti_ump::U7) -> bool {
    index.get() < 32
}
