//! Folds built from folds: composition and a routing of channels to
//! channels. Each is a matrix operation on the coefficients
//! ([`Fold::contributions`]), so what they build is the product, not the two
//! folds run in turn.

use super::{Fold, FoldError, Row, expr_of_coefficients, narrow, row_coefficients};
use crate::{BufferError, Float};
use alloc::vec::Vec;
use tutti_types::ChannelLayout;

impl<F: Float> Fold<F> {
    /// The fold that does `self` and then `next`: its coefficients are the
    /// matrix product, `(self then next)[o][i] = Σ_m next[o][m] · self[m][i]`.
    ///
    /// The product is exact matrix semantics, not the two folds applied in
    /// sequence: a coefficient is the sum of the products of the gains along
    /// each path, so the result rounds differently from running both (by a few
    /// ulps) and agrees with the product of the coefficients each fold states
    /// ([`contributions`](Self::contributions)). An identity on either side
    /// returns the other unchanged, bit for bit. Control side: it allocates.
    ///
    /// # Errors
    ///
    /// [`BufferError::WidthMismatch`] when `next` does not read as many
    /// channels as `self` writes.
    pub fn then(&self, next: &Self) -> Result<Self, BufferError> {
        if self.dst_layout() != next.src_layout() {
            return Err(BufferError::WidthMismatch {
                src: self.dst_layout(),
                dst: next.src_layout(),
            });
        }
        if next.is_identity() {
            return Ok(self.clone());
        }
        if self.is_identity() {
            return Ok(next.clone());
        }
        let rows = (0..next.rows.len())
            .map(|o| {
                let mut acc: Vec<(usize, F)> = Vec::new();
                for (mid, g) in row_coefficients(&next.rows[o]) {
                    for (i, h) in row_coefficients(&self.rows[mid]) {
                        let gain = g * h;
                        match acc.iter_mut().find(|(s, _)| *s == i) {
                            Some((_, a)) => *a += gain,
                            None => acc.push((i, gain)),
                        }
                    }
                }
                acc.sort_unstable_by_key(|&(i, _)| i);
                Row::One(expr_of_coefficients(acc))
            })
            .collect();
        Ok(Self {
            src: self.src,
            rows,
        })
    }

    /// Sends source channel `c` to destination channel `to[c]`, every other
    /// destination channel silent: a routing of channels to channels, with no
    /// speaker in it. Two sources sent to one destination are summed.
    ///
    /// # Errors
    ///
    /// [`FoldError::Buffer`] ([`BufferError::ZeroWidth`]) for an empty width,
    /// [`BufferError::WidthMismatch`] (as [`FoldError::Buffer`]) when `to` does
    /// not name every source channel, and [`FoldError::NoSuchChannel`] for a
    /// destination at or past `dst`.
    pub fn route(src: ChannelLayout, dst: ChannelLayout, to: &[usize]) -> Result<Self, FoldError> {
        if src == ChannelLayout::EMPTY || dst == ChannelLayout::EMPTY {
            return Err(BufferError::ZeroWidth.into());
        }
        if to.len() != usize::from(src.count()) {
            return Err(BufferError::WidthMismatch {
                src,
                dst: ChannelLayout::from_count(narrow(to.len())),
            }
            .into());
        }
        if let Some(&channel) = to.iter().find(|&&d| d >= usize::from(dst.count())) {
            return Err(FoldError::NoSuchChannel {
                channel,
                width: dst,
            });
        }
        let rows = (0..usize::from(dst.count()))
            .map(|d| {
                let pairs = to
                    .iter()
                    .enumerate()
                    .filter(|&(_, &t)| t == d)
                    .map(|(c, _)| (c, F::ONE))
                    .collect();
                Row::One(expr_of_coefficients(pairs))
            })
            .collect();
        Ok(Self {
            src: src.count(),
            rows,
        })
    }
}
