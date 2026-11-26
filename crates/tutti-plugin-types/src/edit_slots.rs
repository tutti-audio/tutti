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
//! activation lasts (CLAP's `rescan(Info)` republishes the ranges but cannot
//! add or remove an id); a write to an id outside it is refused at the call
//! ([`UnknownParam`]).
//!
//! [`EditWriter::reset`] makes every write made so far stale, for a state or a
//! preset load that replaced the values they were written against: it ends the
//! [`tutti_rt::SeqSlotsWriter`] epoch, and a drain that observes it never
//! yields a write from before it. The slots, the epoch and the drain's
//! ordering are that type's; this one adds the `ParamId` table.
//!
//! What a plugin reports back to the host is the other direction and a ring of
//! events: [`reports`](crate::reports()).

use std::sync::Arc;

use tutti_rt::{
    Generation, GenerationPublisher, GenerationWatch, SeqSlotsReader, SeqSlotsWriter,
    generation_watch, seq_slots,
};
use tutti_units::{Normalized, ParamId};

/// A write to a parameter the activation did not declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("parameter {0} is not declared by the activation's plugin")]
pub struct UnknownParam(pub ParamId);

/// The slots for `ids`, as the control side's writer and the audio side's
/// reader. A repeated id is one slot.
///
/// The applied pair ([`EditWriter::has_applied`], [`EditReader::publish_applied`]) is
/// made here, with the slots: a writer's generation counts from 0 again for
/// each new set, and a publisher keeps only the latest generation it stored, so
/// a publisher kept from an earlier set would never reach the new writer's
/// marks. A new set of slots is always a new pair.
#[must_use]
pub fn edit_slots(ids: impl IntoIterator<Item = ParamId>) -> (EditWriter, EditReader) {
    let mut ids: Vec<ParamId> = ids.into_iter().collect();
    ids.sort_unstable();
    ids.dedup();
    let ids: Arc<[ParamId]> = ids.into();
    // The slot holds a raw `f64`, not a [`Normalized`]: the sequence cell copies
    // its payload as plain words and needs `AnyBitPattern`, which a
    // range-checked type cannot be. Every value came from a `Normalized`, and
    // the reader makes one again.
    let (writer, reader) = seq_slots::<f64>(ids.len());
    let (publisher, watch) = generation_watch();
    (
        EditWriter {
            wanted: vec![None; ids.len()],
            ids: Arc::clone(&ids),
            slots: writer,
            applied: watch,
        },
        EditReader {
            ids,
            slots: reader,
            applied: publisher,
        },
    )
}

/// The control side of the slots.
#[derive(Debug)]
pub struct EditWriter {
    ids: Arc<[ParamId]>,
    slots: SeqSlotsWriter<f64>,
    /// What the holder of the reader has published as applied.
    applied: GenerationWatch,
    /// The newest write of each slot with the mark it was made at: the wanted
    /// record, read without asking the reader ([`pending`](Self::pending)).
    wanted: Vec<Option<(Normalized, Generation)>>,
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
        self.slots.write(slot, value.get());
        self.wanted[slot] = Some((value, self.slots.written()));
        Ok(())
    }

    /// The writes not yet applied ([`has_applied`](Self::has_applied)): the
    /// newest value of each parameter whose write the reader's holder has not
    /// published, in id order. Read from the writer's own record, so it never
    /// waits for a block; a parameter whose newest write is applied is not
    /// listed.
    #[must_use]
    pub fn pending(&self) -> Vec<(ParamId, Normalized)> {
        self.ids
            .iter()
            .zip(&self.wanted)
            .filter_map(|(&id, wanted)| {
                let (value, mark) = (*wanted)?;
                (!self.has_applied(mark)).then_some((id, value))
            })
            .collect()
    }

    /// How many writes have been made, as the [`Generation`] the reader's
    /// [`swept`](EditReader::swept) reaches once a drain has collected them
    /// all.
    #[must_use]
    pub const fn written(&self) -> Generation {
        self.slots.written()
    }

    /// Whether the writes up to `mark` (a [`written`](Self::written) taken at
    /// the write) have been applied: the reader's holder called
    /// [`EditReader::publish_applied`] after the target's own call that took them
    /// returned. A write a reset made stale counts once a drain has passed it.
    /// Exact while the two are less than 2^31 writes apart.
    #[must_use]
    pub fn has_applied(&self, mark: Generation) -> bool {
        self.applied.get().has_reached(mark)
    }

    /// Makes every write made so far stale: a state or a preset load replaced
    /// the values they were written against.
    ///
    /// A caller that follows it with a change a stale write must not overwrite
    /// (a plugin's `set_state`) calls it first: it is ordered before whatever
    /// follows the call (see [`tutti_rt::SeqSlotsWriter::new_epoch`]). A block
    /// that began draining before the reset may still apply what it had
    /// collected, since it overlapped the change; every block that starts after
    /// the reset sees the new epoch and applies no write from before it.
    pub fn reset(&mut self) {
        self.slots.new_epoch();
        self.wanted.fill(None);
    }
}

/// The audio side of the slots.
#[derive(Debug)]
pub struct EditReader {
    ids: Arc<[ParamId]>,
    slots: SeqSlotsReader<f64>,
    /// Tells the writer what the target has applied.
    applied: GenerationPublisher,
}

impl EditReader {
    /// Hands `apply` each slot written since the last call, by id, with the
    /// value of its latest write, skipping what a [`reset`](EditWriter::reset)
    /// made stale. Reads each slot once and never retries: a slot a write
    /// overlaps is read on the next call. Never allocates, locks or waits.
    pub fn drain(&mut self, mut apply: impl FnMut(ParamId, Normalized)) {
        self.drain_in_epoch(|id, value, _| apply(id, value));
    }

    /// [`drain`](Self::drain), handing `apply` the epoch the drain observed
    /// beside each value, the one every write it yields was checked against. A
    /// caller that keeps a write past the drain (a chunk that has not gone yet)
    /// compares that epoch with [`epoch`](Self::epoch) later, with
    /// [`epoch_before`], and drops the write a reset has made stale since.
    pub fn drain_in_epoch(&mut self, mut apply: impl FnMut(ParamId, Normalized, u32)) {
        let ids = &self.ids;
        self.slots.drain_in_epoch(|slot, value, epoch| {
            // `clamped` is the range contract's hot-path form: every value came
            // from a `Normalized`, so it changes nothing, and the audio thread
            // must not panic.
            apply(ids[slot], Normalized::clamped(value), epoch);
        });
    }

    /// Publishes that the target applied what the drains so far collected:
    /// [`EditWriter::has_applied`] then holds for each write they covered.
    /// Called by the holder after the target's own call that took the values
    /// has returned, never at drain time; a call that never reached the
    /// target leaves the values to the next one. Wait-free.
    pub fn publish_applied(&mut self) {
        self.applied.publish(self.slots.swept());
    }

    /// The generation of the last drain no write overlapped: once it
    /// [has reached](Generation::has_reached) an [`EditWriter::written`], every
    /// write up to it has been handed to a drain's callback or made stale.
    #[must_use]
    pub const fn swept(&self) -> Generation {
        self.slots.swept()
    }

    /// The epoch now: it moves with each [`EditWriter::reset`].
    #[must_use]
    pub fn epoch(&self) -> u32 {
        self.slots.epoch()
    }

    /// A handle that reads the epoch from any thread, without the reader.
    #[must_use]
    pub fn epoch_watch(&self) -> tutti_rt::EpochWatch {
        self.slots.epoch_watch()
    }
}

/// `older` writes with `newer` laid over them: each parameter once, at the
/// newer value where both hold it, in the order each was first named. For two
/// records of one parameter's writes (a plugin's own and the host's).
#[must_use]
pub fn overlay_writes(
    mut older: Vec<(ParamId, Normalized)>,
    newer: impl IntoIterator<Item = (ParamId, Normalized)>,
) -> Vec<(ParamId, Normalized)> {
    for (id, value) in newer {
        match older.iter_mut().find(|(known, _)| *known == id) {
            Some(entry) => entry.1 = value,
            None => older.push((id, value)),
        }
    }
    older
}

/// Whether epoch `a` is before epoch `b`, as the serial numbers
/// [`EditReader::epoch`] and [`EditReader::drain_in_epoch`] give.
#[must_use]
pub const fn epoch_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
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

    /// **The epoch a drain yields a write in is before the one a reset makes
    /// current**, so a write kept past the drain is known stale.
    #[test]
    fn a_reset_moves_the_epoch_past_the_one_a_drain_observed() {
        let (mut writer, mut reader) = edit_slots([id(1)]);
        writer.write(id(1), Normalized::of(0.25)).unwrap();
        let mut seen = None;
        reader.drain_in_epoch(|_, _, epoch| seen = Some(epoch));
        let drained_in = seen.expect("the write was yielded");
        assert!(!epoch_before(drained_in, reader.epoch()), "still current");
        writer.reset();
        assert!(
            epoch_before(drained_in, reader.epoch()),
            "stale after a reset"
        );
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

    /// **A write is applied only once the reader's holder says its target's call
    /// returned**: not before any block, not after a drain whose call has not
    /// returned, and then it is, for that write and the ones before it but not
    /// one made after.
    #[test]
    fn a_write_is_applied_after_the_targets_call_returns() {
        let (mut writer, mut reader) = edit_slots([id(1), id(2)]);
        writer.write(id(1), Normalized::of(0.25)).unwrap();
        let mark = writer.written();
        assert!(!writer.has_applied(mark), "no block has run");

        assert_eq!(drained(&mut reader), [(id(1), 0.25)]);
        assert!(
            !writer.has_applied(mark),
            "the drain took it; the target's call has not returned"
        );

        writer.write(id(2), Normalized::of(0.5)).unwrap();
        let later = writer.written();
        reader.publish_applied();
        assert!(writer.has_applied(mark), "the call that took it returned");
        assert!(!writer.has_applied(later), "a write after the drain is not");

        assert_eq!(drained(&mut reader), [(id(2), 0.5)]);
        assert!(!writer.has_applied(later), "not until its own call returns");
        reader.publish_applied();
        assert!(writer.has_applied(later));
    }

    /// **A write is pending until its call returned, the newest of each
    /// parameter, from the writer's own record; a reset ends it.**
    #[test]
    fn a_write_is_pending_until_the_targets_call_returns() {
        let (mut writer, mut reader) = edit_slots([id(1), id(2)]);
        assert!(writer.pending().is_empty());
        writer.write(id(1), Normalized::of(0.25)).unwrap();
        writer.write(id(1), Normalized::of(0.5)).unwrap();
        writer.write(id(2), Normalized::of(0.75)).unwrap();
        assert_eq!(
            writer.pending(),
            [(id(1), Normalized::of(0.5)), (id(2), Normalized::of(0.75))]
        );
        assert_eq!(drained(&mut reader).len(), 2);
        assert_eq!(
            writer.pending().len(),
            2,
            "taken, the call has not returned"
        );
        reader.publish_applied();
        assert!(writer.pending().is_empty());
        writer.write(id(2), Normalized::of(0.125)).unwrap();
        writer.reset();
        assert!(writer.pending().is_empty(), "stale writes are not wanted");
    }

    /// **A drain allocates nothing**, with every slot written (the crate's test
    /// binary installs `RtAllocCheck`).
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
