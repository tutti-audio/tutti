//! The host's parameter writes to a running plugin, as one slot per declared
//! parameter: [`edit_slots`].
//!
//! The control side writes a parameter through [`EditWriter::write`]; the
//! Processor reads each slot once per block through [`EditReader::drain`] and
//! applies what changed at the head of the block. A slot holds one value, so a
//! newer write replaces an older one that no block took: nothing queues, so
//! nothing fills and nothing is retried.
//!
//! The slots and the `ParamId` table are built from the parameters the plugin
//! declares at activation. The list changes only across a reactivation (CLAP's
//! `rescan(ALL)` needs the plugin deactivated, and a VST3 id change goes
//! through the restart rules), so the table cannot go stale while the
//! activation lasts; a write to an id outside it is refused at the call
//! ([`UnknownParam`]).
//!
//! [`EditWriter::reset`] makes every write made so far stale, for a state or a
//! preset load that replaced the values they were written against. Each write
//! carries the stamp current when it was made, and the reader drops a value
//! whose stamp is older than the reset stamp it loads. Stamps compare as
//! 32-bit serial numbers, so they wrap.
//!
//! What a plugin reports back to the host is the other direction and a ring of
//! events: [`reports`](crate::reports()).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bytemuck::{Pod, Zeroable};
use tutti_rt::{SeqReader, SeqWriter};
use tutti_units::{Normalized, ParamId};

/// What a slot holds.
///
/// The value is a raw `f64`, not a [`Normalized`]: the sequence cell copies its
/// payload as plain words and needs `AnyBitPattern`, which a range-checked type
/// cannot be. Every value came from a `Normalized`, and the reader makes one
/// again.
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct Cell {
    value: f64,
    /// The writer's stamp when it wrote the value.
    stamp: u32,
    _pad: u32,
}

/// Whether stamp `a` is older than stamp `b`, as 32-bit serial numbers.
const fn stamp_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// A write to a parameter the activation did not declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("parameter {0} is not declared by the activation's plugin")]
pub struct UnknownParam(pub ParamId);

/// The slots for `ids`, as the control side's writer and the audio side's
/// reader. A repeated id is one slot.
#[must_use]
pub fn edit_slots(ids: impl IntoIterator<Item = ParamId>) -> (EditWriter, EditReader) {
    let mut ids: Vec<ParamId> = ids.into_iter().collect();
    ids.sort_unstable();
    ids.dedup();
    let ids: Arc<[ParamId]> = ids.into();
    let cells: Vec<SeqWriter<Cell>> = ids.iter().map(|_| SeqWriter::new()).collect();
    let readers: Vec<SeqReader<Cell>> = cells.iter().map(SeqWriter::reader).collect();
    let reset = Arc::new(AtomicU32::new(0));
    (
        EditWriter {
            ids: Arc::clone(&ids),
            cells,
            stamp: 0,
            reset: Arc::clone(&reset),
        },
        EditReader {
            seen: vec![0; ids.len()],
            ids,
            cells: readers,
            reset,
        },
    )
}

/// The control side of the slots.
#[derive(Debug)]
pub struct EditWriter {
    ids: Arc<[ParamId]>,
    cells: Vec<SeqWriter<Cell>>,
    /// The stamp of the writes made now.
    stamp: u32,
    /// The stamp of the last [`reset`](Self::reset), which the reader loads.
    reset: Arc<AtomicU32>,
}

impl EditWriter {
    /// Stores `value` in the slot of `id`, replacing the value a block has not
    /// taken. Wait-free; allocates nothing.
    ///
    /// # Errors
    ///
    /// [`UnknownParam`] when the activation did not declare `id`.
    pub fn write(&mut self, id: ParamId, value: Normalized) -> Result<(), UnknownParam> {
        let slot = self.ids.binary_search(&id).map_err(|_| UnknownParam(id))?;
        self.cells[slot].write(Cell {
            value: value.get(),
            stamp: self.stamp,
            _pad: 0,
        });
        Ok(())
    }

    /// Makes every write made so far stale: a state or a preset load replaced
    /// the values they were written against, and no block may put them back.
    pub fn reset(&mut self) {
        self.stamp = self.stamp.wrapping_add(1);
        self.reset.store(self.stamp, Ordering::Release);
    }
}

/// The audio side of the slots.
#[derive(Debug)]
pub struct EditReader {
    ids: Arc<[ParamId]>,
    cells: Vec<SeqReader<Cell>>,
    /// The sequence of the last write each slot gave up, per slot.
    seen: Vec<u64>,
    reset: Arc<AtomicU32>,
}

impl EditReader {
    /// Hands `apply` each slot written since the last call, by id, with the
    /// value of its latest write, skipping what a [`reset`](EditWriter::reset)
    /// made stale. Reads each slot once and never retries: a slot a write
    /// overlaps is read on the next call. Never allocates, locks or waits.
    pub fn drain(&mut self, mut apply: impl FnMut(ParamId, Normalized)) {
        for (slot, cell) in self.cells.iter().enumerate() {
            let Some((sequence, cell)) = cell.try_read() else {
                continue;
            };
            if sequence == self.seen[slot] {
                continue;
            }
            self.seen[slot] = sequence;
            if stamp_before(cell.stamp, self.reset.load(Ordering::Acquire)) {
                continue;
            }
            // `clamped` is the range contract's hot-path form: every value came
            // from a `Normalized`, so it changes nothing, and the audio thread
            // must not panic.
            apply(self.ids[slot], Normalized::clamped(cell.value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> ParamId {
        ParamId::new(n)
    }

    fn drained(reader: &mut EditReader) -> Vec<(ParamId, f64)> {
        let mut out = Vec::new();
        reader.drain(|id, value| out.push((id, value.get())));
        out
    }

    /// **Many writes to one parameter before a drain are one value, the last.**
    #[test]
    fn writes_before_a_drain_coalesce_to_the_last() {
        let (mut writer, mut reader) = edit_slots([id(1), id(2)]);
        for step in 0..100 {
            writer
                .write(id(1), Normalized::of(f64::from(step) / 100.0))
                .unwrap();
        }
        assert_eq!(drained(&mut reader), [(id(1), 0.99)]);
        assert!(drained(&mut reader).is_empty(), "a slot is given up once");
    }

    /// **A reset makes the writes before it stale; a write after it is not.**
    #[test]
    fn a_reset_voids_the_writes_before_it() {
        let (mut writer, mut reader) = edit_slots([id(1), id(2)]);
        writer.write(id(1), Normalized::of(0.25)).unwrap();
        writer.reset();
        assert!(drained(&mut reader).is_empty());
        writer.write(id(2), Normalized::of(0.5)).unwrap();
        assert_eq!(drained(&mut reader), [(id(2), 0.5)]);
        // The stale cell stays stale: it is not given up later.
        assert!(drained(&mut reader).is_empty());
    }

    /// **A slot written again after a stale write gives up the new value.**
    #[test]
    fn a_write_after_a_reset_replaces_the_stale_one() {
        let (mut writer, mut reader) = edit_slots([id(1)]);
        writer.write(id(1), Normalized::of(0.25)).unwrap();
        writer.reset();
        writer.write(id(1), Normalized::of(0.75)).unwrap();
        assert_eq!(drained(&mut reader), [(id(1), 0.75)]);
    }

    /// **Stamps compare as serial numbers across the 32-bit wrap**: a stamp just
    /// before the wrap is older than one just after it, and equal is not older.
    #[test]
    fn stamps_compare_across_the_wrap() {
        assert!(stamp_before(u32::MAX, 0));
        assert!(stamp_before(u32::MAX - 5, 3));
        assert!(!stamp_before(0, u32::MAX));
        assert!(!stamp_before(3, u32::MAX - 5));
        assert!(!stamp_before(7, 7));
        assert!(stamp_before(6, 7));
        assert!(!stamp_before(7, 6));
    }

    /// **A write to an undeclared id is refused at the call**, naming it.
    #[test]
    fn an_undeclared_id_is_refused() {
        let (mut writer, mut reader) = edit_slots([id(1)]);
        assert_eq!(
            writer.write(id(9), Normalized::of(0.5)),
            Err(UnknownParam(id(9)))
        );
        assert!(drained(&mut reader).is_empty());
    }

    /// **A drain allocates nothing**, with every slot written (the crate's test
    /// binary installs `AllocDisabler`).
    #[test]
    fn a_drain_allocates_nothing() {
        let (mut writer, mut reader) = edit_slots((0..64).map(id));
        for n in 0..64 {
            writer.write(id(n), Normalized::of(0.5)).unwrap();
        }
        let mut seen = 0;
        assert_no_alloc::assert_no_alloc(|| reader.drain(|_, _| seen += 1));
        assert_eq!(seen, 64);
    }
}
