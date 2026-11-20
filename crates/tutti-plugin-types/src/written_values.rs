//! The values a host last wrote to a format whose parameter word is `f32`.

use tutti_rt::{AtomicCell, AtomicWord};
use tutti_units::Normalized;

/// What a slot holds: the value the host last wrote, or nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Written(Option<Normalized>);

/// The word of a slot nothing was written to: a NaN, which no [`Normalized`] is.
const UNWRITTEN: u64 = f64::NAN.to_bits();

impl AtomicWord for Written {
    fn to_word(self) -> u64 {
        self.0.map_or(UNWRITTEN, |value| value.get().to_bits())
    }

    fn from_word(word: u64) -> Option<Self> {
        if word == UNWRITTEN {
            return Some(Self(None));
        }
        Normalized::new(f64::from_bits(word)).map(|value| Self(Some(value)))
    }
}

/// The `f64` value a host last wrote to each parameter slot of a format that
/// carries a parameter as `f32` (VST2's `setParameter`, AU's
/// `AudioUnitSetParameter`).
///
/// The format narrows a write to `f32`, so a plugin that reports the value it
/// holds reports the narrowing of the written one, not the written one.
/// [`absorbs`](Self::absorbs) drops a report equal to that narrowing while the
/// cell still holds the host's last write (the plugin repeating it, as often as
/// it likes), so the `f64` the host wrote stays the value of the parameter. A
/// report that is anything else is the plugin's own change: it clears the cell,
/// so a later report of the old narrowing is a change too.
///
/// A slot is an index the format owns (VST2's parameter index, AU's position in
/// its sorted ids). Every call is wait-free and allocation-free after
/// [`new`](Self::new), so the audio thread may call any of them.
#[derive(Debug)]
pub struct WrittenValues {
    cells: Box<[AtomicCell<Written>]>,
}

impl WrittenValues {
    /// Cells for `len` slots, none written. Allocates: the control side's call.
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self {
            cells: (0..len).map(|_| AtomicCell::new(Written(None))).collect(),
        }
    }

    /// Records `value` as the latest the host wrote to `slot`. A slot past the
    /// cells records nothing.
    pub fn record(&self, slot: usize, value: Normalized) {
        if let Some(cell) = self.cells.get(slot) {
            cell.store(Written(Some(value)));
        }
    }

    /// The latest value written to `slot` and not since cleared, `None` when
    /// there is none.
    #[must_use]
    pub fn recorded(&self, slot: usize) -> Option<Normalized> {
        self.cells.get(slot)?.load().0
    }

    /// Forgets what was written to every slot: the plugin set its own state
    /// (a preset, a chunk), so no report is the host's write repeated.
    pub fn clear(&self) {
        for cell in &self.cells {
            cell.store(Written(None));
        }
    }

    /// Whether a report for `slot` is the host's last write repeated, and so
    /// not an edit. `repeats` is given the value last written and answers
    /// whether the format's own narrowing of it (`value as f32` for a normalized
    /// word, the denormalized plain value for AU) equals what the plugin
    /// reported; it is not called for a slot nothing was written to.
    ///
    /// A report that is not repeated clears the slot and returns `false`: it is
    /// the plugin's own change, and the narrowing of an older write is no longer
    /// an echo of anything.
    pub fn absorbs(&self, slot: usize, repeats: impl FnOnce(Normalized) -> bool) -> bool {
        let Some(cell) = self.cells.get(slot) else {
            return false;
        };
        match cell.load().0 {
            Some(written) if repeats(written) => true,
            Some(_) => {
                cell.store(Written(None));
                false
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BELOW_F32: f64 = 0.1 + 1e-12;

    /// A value that differs from its `f32` only below `f32` precision reads as
    /// an echo of that `f32`, as often as it is reported, and the cell keeps the
    /// `f64`.
    #[test]
    fn narrowing_of_the_written_value_is_absorbed_every_time_and_the_cell_keeps_the_f64() {
        let cells = WrittenValues::new(2);
        cells.record(1, Normalized::of(BELOW_F32));
        assert_ne!(f64::from(BELOW_F32 as f32), BELOW_F32);
        for _ in 0..2 {
            assert!(cells.absorbs(1, |v| v.get() as f32 == BELOW_F32 as f32));
        }
        assert_eq!(cells.recorded(1).map(Normalized::get), Some(BELOW_F32));
    }

    /// A report off the narrowing is the plugin's own change and clears the cell,
    /// so the narrowing of the old write is a change when it comes later.
    #[test]
    fn a_report_off_the_narrowing_clears_the_cell() {
        let cells = WrittenValues::new(1);
        cells.record(0, Normalized::of(BELOW_F32));
        let narrowed = BELOW_F32 as f32;
        assert!(!cells.absorbs(0, |v| v.get() as f32 == narrowed.next_up()));
        assert_eq!(cells.recorded(0), None);
        assert!(!cells.absorbs(0, |v| v.get() as f32 == narrowed));
    }

    /// A slot never written, cleared, or past the cells absorbs nothing.
    #[test]
    fn unwritten_cleared_and_out_of_range_slots_absorb_nothing() {
        let cells = WrittenValues::new(2);
        assert!(!cells.absorbs(0, |_| true));
        cells.record(0, Normalized::of(0.5));
        cells.record(1, Normalized::of(0.5));
        cells.clear();
        assert!(!cells.absorbs(0, |_| true) && !cells.absorbs(1, |_| true));
        cells.record(5, Normalized::of(0.5));
        assert!(cells.recorded(5).is_none());
        assert!(!cells.absorbs(5, |_| true));
    }
}
