//! The values a host last wrote to a format whose parameter word is `f32`.

use std::sync::atomic::{AtomicU64, Ordering};
use tutti_units::Normalized;

/// The bits of a cell nothing was written to: a NaN, which no [`Normalized`] is.
const UNWRITTEN: u64 = f64::NAN.to_bits();

/// The `f64` value a host last wrote to each parameter slot of a format that
/// carries a parameter as `f32` (VST2's `setParameter`, AU's
/// `AudioUnitSetParameter`).
///
/// The format narrows a write to `f32`, so a plugin that reports the value it
/// holds reports the narrowing of the written one, not the written one. A
/// report equal to that narrowing ([`echoes`](Self::echoes)) is the plugin
/// repeating the host's own write: it is not an edit, and the `f64` the host
/// wrote stays the value of the parameter.
///
/// A slot is an index the format owns (VST2's parameter index, AU's position in
/// its sorted ids). Recording and reading are lock-free and wait-free, so the
/// audio thread may record.
#[derive(Debug)]
pub struct WrittenValues {
    cells: Box<[AtomicU64]>,
}

impl WrittenValues {
    /// Cells for `len` slots, none written. Allocates: the control side's call.
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self {
            cells: (0..len).map(|_| AtomicU64::new(UNWRITTEN)).collect(),
        }
    }

    /// Records `value` as the latest the host wrote to `slot`. A slot past the
    /// cells records nothing.
    pub fn record(&self, slot: usize, value: Normalized) {
        if let Some(cell) = self.cells.get(slot) {
            cell.store(value.get().to_bits(), Ordering::Relaxed);
        }
    }

    /// The latest value written to `slot`, `None` when nothing was.
    #[must_use]
    pub fn recorded(&self, slot: usize) -> Option<Normalized> {
        let bits = self.cells.get(slot)?.load(Ordering::Relaxed);
        (bits != UNWRITTEN).then(|| Normalized::clamped(f64::from_bits(bits)))
    }

    /// Whether `report`, a word the plugin gave back for `slot`, is the
    /// `narrow`ing of the value last written there: the format's own rounding
    /// of the cell (`value as f32` for a normalized word, the denormalized
    /// plain value for AU). `false` when nothing was written.
    #[must_use]
    pub fn echoes(&self, slot: usize, report: f32, narrow: impl FnOnce(Normalized) -> f32) -> bool {
        self.recorded(slot)
            .is_some_and(|written| narrow(written) == report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BELOW_F32: f64 = 0.1 + 1e-12;

    /// A value that differs from its `f32` only below `f32` precision reads as
    /// an echo of that `f32`, and the cell keeps the `f64`.
    #[test]
    fn narrowing_of_the_written_value_is_an_echo_and_the_cell_keeps_the_f64() {
        let cells = WrittenValues::new(2);
        cells.record(1, Normalized::of(BELOW_F32));
        assert_ne!(f64::from(BELOW_F32 as f32), BELOW_F32);
        assert!(cells.echoes(1, BELOW_F32 as f32, |v| v.get() as f32));
        assert_eq!(cells.recorded(1).map(Normalized::get), Some(BELOW_F32));
    }

    /// A report one `f32` step from the narrowing is the plugin's own change.
    #[test]
    fn report_off_the_narrowing_is_not_an_echo() {
        let cells = WrittenValues::new(1);
        cells.record(0, Normalized::of(BELOW_F32));
        let next = (BELOW_F32 as f32).next_up();
        assert!(!cells.echoes(0, next, |v| v.get() as f32));
    }

    /// A slot never written, or past the cells, echoes nothing.
    #[test]
    fn unwritten_and_out_of_range_slots_echo_nothing() {
        let cells = WrittenValues::new(1);
        assert!(!cells.echoes(0, 0.0, |v| v.get() as f32));
        cells.record(5, Normalized::of(0.5));
        assert!(cells.recorded(5).is_none());
        assert!(!cells.echoes(5, 0.5, |v| v.get() as f32));
    }
}
