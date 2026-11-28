//! The control plane of the voice pool: the controls a session writes its table
//! through, and the private ring they send what the table cannot carry through.
//!
//! A pool's voices are the rows of a published [`VoiceTable`]: the audio thread lends
//! it once per block, so a row's presence and every one of its values reach the
//! pool with no message. What a row needs that the audio thread cannot build (its
//! voice, its stretch vocoder, its seat on a stream's reader, a loop-wrap spare) is
//! built here, on the control side, and sent: [`Message`]. A standalone voice node's
//! controls are [`VoiceNodeControls`](super::node_controls::VoiceNodeControls).

use std::collections::HashMap;
use std::sync::Arc;

use crate::streamer::{ControlCell, StreamHandle};

use super::node_controls::StreamHold;
use super::pool::{DrainCounters, PoolDrops, RetireRoom, Retired, retirement_capacity};
use super::roster::Installed;
use super::slot::window_covers;
use super::stretched;
use super::table::{Row, VoiceTable};
use super::types::SlotId;
use crate::SamplerFloat;
use tutti_buffers::ChannelLayout;
use tutti_dsp::PhaseVocoder;
use tutti_graph::Published;
use tutti_music::LoopRange;
use tutti_rt::{AtomicCell, Mutex, Publisher, Reclaim, RingReader, RingWriter};
use tutti_units::{Amplitude, Beat};

/// The voices a pool holds when its config does not say.
pub(super) const MAX_RESIDENT_VOICES: usize = 64;

/// What the controls send the audio thread: the two things a table cannot carry.
pub(crate) enum Message<F: SamplerFloat> {
    /// A voice built for a row: the slot (its voice, its vocoder when the row
    /// stretches, its seat), boxed so the audio thread moves a pointer in and frees
    /// nothing. The slot is taken in only where the table still has its row.
    Install(Box<Installed<F>>),
    /// Give the voice at `id` a spare vocoder (`Some`), or take its spare away
    /// (`None`): the vocoder a voice under the transport's loop primes for a wrap
    /// with. What the slot does not keep leaves through the retirement ring.
    SetSpare {
        id: SlotId,
        spare: Option<Box<PhaseVocoder<F>>>,
    },
}

/// Publishes `value` to `cell` when it differs from the one in it: the
/// request a stream's streamer reads.
pub(super) fn ask<T: Copy + PartialEq>(cell: &ControlCell<T>, value: T) {
    if *cell.current() != value {
        cell.publish(value);
    }
}

/// A gain the sampler refuses: a voice's gain is a finite multiplier, and a
/// NaN or an infinity is a mistake, not a mute (silence is
/// [`Amplitude::SILENT`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum GainError {
    /// The gain is NaN or infinite.
    #[error("the gain is not finite")]
    NotFinite,
}

impl GainError {
    pub(crate) fn check(gain: Amplitude) -> Result<(), Self> {
        if gain.get().is_finite() {
            Ok(())
        } else {
            Err(Self::NotFinite)
        }
    }
}

/// The start of the transport's loop while looping is armed, as the pool last rendered it.
///
/// Shared by a pool and its controls. The pool's audio side stores the start its block's
/// transport carries ([`publish`](Self::publish), one atomic store per block); the controls
/// read it on their reclaim tick, where they decide which voices carry a spare vocoder. So
/// the start is the loop of the audio actually rendered, and a pool that renders nothing
/// leaves it as it was.
#[derive(Debug)]
pub(crate) struct LoopFeed {
    start: AtomicCell<Option<Beat>>,
}

impl Default for LoopFeed {
    fn default() -> Self {
        Self {
            start: AtomicCell::new(None),
        }
    }
}

impl LoopFeed {
    /// The loop's start as the pool's last block carried it.
    pub(crate) fn start(&self) -> Option<Beat> {
        self.start.load()
    }

    /// Records `looping`'s start (`None`: not looping). Audio thread.
    pub(crate) fn publish(&self, looping: Option<LoopRange>) {
        self.start.store(looping.map(|range| range.start()));
    }
}

/// What the controls know of a voice they sent: the row it was built from, and
/// what they hold of it.
struct Entry<F: SamplerFloat> {
    /// The structure the voice was built at: a row of the table matches the voice
    /// only if it has this structure.
    row: Row<F>,
    /// Whether the controls sent the voice a spare vocoder.
    spare: bool,
    /// Whether a streamed voice holds its stream's reader; `None` for a resident one.
    hold: Option<Arc<AtomicCell<StreamHold>>>,
    /// A streamed voice's stream, whose cells the sweep reclaims.
    stream: Option<Arc<Mutex<StreamHandle<F>>>>,
}

/// The slots the controls have sent, and the writer of the ring that carries them,
/// under one lock.
///
/// The lock covers the table's compare and publish (`Publisher` has no
/// compare-and-publish), the build of a voice, and one ring push (which defers when
/// the ring is full, never waits). Control threads only: the audio thread never takes it.
struct Inner<F: SamplerFloat> {
    /// The voices sent and not dropped from the table since: a row of the table
    /// with an entry of its structure is not sent again.
    slots: HashMap<SlotId, Entry<F>>,
    commands: RingWriter<Message<F>>,
}

/// What a pool is steered through: the table a session writes ([`Published`]), the
/// ring the voices it needs travel on, and the collection point for what the audio
/// thread retires ([`Reclaim`]).
///
/// The node's one published entry: the session's write of the table is
/// [`replace`](Published::replace), which publishes it to the cell the pool lends and
/// builds and sends the voices its new rows need, and the session's sweep is
/// [`reclaim`](Reclaim::reclaim), which frees what the audio thread retired and sends
/// what a full ring held back. Cheap to clone; every clone reaches the same pool.
/// Control thread only.
///
/// # Installs are not atomic
///
/// A table's values and removals reach the pool at one block. Its new voices do not: the
/// ring holds two messages for each voice, and a table
/// that needs more (or a retirement ring with no room for another slot) sends the rest at
/// the next reclaim that has room. A row sounds from the block after its voice arrives,
/// silent until then, never wrong.
#[derive(Clone)]
pub struct VoicePoolControls<F: SamplerFloat> {
    /// The table, which the pool lends.
    table: Publisher<VoiceTable<F>>,
    inner: Arc<Mutex<Inner<F>>>,
    /// Slots and spares the audio thread has handed back to be freed here.
    retired: Arc<Mutex<RingReader<Retired<F>>>>,
    /// The pool's width, which a vocoder is built at, on the control thread.
    channels: ChannelLayout,
    /// The pool's voices, which size its rings.
    voices: usize,
    /// What the audio thread dropped or the controls deferred; read by [`drops`](Self::drops).
    counters: Arc<DrainCounters>,
    /// The retirement slots reserved of `retired`.
    room: Arc<RetireRoom>,
    /// The transport's loop as the pool last rendered it, which decides which voices
    /// carry a spare vocoder.
    loops: Arc<LoopFeed>,
}

impl<F: SamplerFloat> std::fmt::Debug for VoicePoolControls<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoicePoolControls")
            .field("voices", &self.voices)
            .finish_non_exhaustive()
    }
}

impl<F: SamplerFloat> VoicePoolControls<F> {
    #[allow(
        clippy::too_many_arguments,
        reason = "the pool's one construction site"
    )]
    pub(super) fn new(
        table: Publisher<VoiceTable<F>>,
        commands: RingWriter<Message<F>>,
        retired: RingReader<Retired<F>>,
        channels: ChannelLayout,
        voices: usize,
        room: Arc<RetireRoom>,
        counters: Arc<DrainCounters>,
        loops: Arc<LoopFeed>,
    ) -> Self {
        Self {
            table,
            inner: Arc::new(Mutex::new(Inner {
                slots: HashMap::new(),
                commands,
            })),
            retired: Arc::new(Mutex::new(retired)),
            channels,
            voices,
            counters,
            room,
            loops,
        }
    }

    /// The table cell, for the fork source.
    pub(super) fn table(&self) -> &Publisher<VoiceTable<F>> {
        &self.table
    }

    /// What the audio thread has dropped or leaked, and the controls deferred, since
    /// the pool was built. Poll it as often as convenient: the counts only grow.
    pub fn drops(&self) -> PoolDrops {
        self.counters.snapshot()
    }

    /// Whether the streamed voice at `key` holds its stream's reader yet: `None` for a
    /// resident row, and for a key the controls have sent no voice for.
    #[must_use]
    pub fn hold(&self, key: SlotId) -> Option<StreamHold> {
        self.inner
            .lock()
            .slots
            .get(&key)?
            .hold
            .as_ref()
            .map(|hold| hold.load())
    }

    /// Sends the voice of every row of the table that has none: what the pool
    /// starts with, and what a reclaim retries when the ring was full.
    pub(super) fn send_missing(&self) {
        let table = self.table.current();
        let gone = {
            let mut inner = self.inner.lock();
            self.sync(&mut inner, &table)
        };
        drop(gone);
    }

    /// Brings the entries to `table`: forgets those whose row left or changed
    /// structure (the audio thread retires their slots when it lends the table), and
    /// builds and sends the voice of each row without one. A row the ring or the
    /// retirement room has no place for is left for the next call. Returns what
    /// must be dropped after the lock: a row's old entry, and a voice the ring
    /// refused.
    fn sync(&self, inner: &mut Inner<F>, table: &VoiceTable<F>) -> Dropped<F> {
        let mut dropped = Dropped::default();
        let rows = table.rows();
        let kept = |key: &SlotId, entry: &Entry<F>| {
            rows.binary_search_by_key(&key.0, |(held, _)| held.0)
                .is_ok_and(|at| rows[at].1.same_structure(&entry.row))
        };
        let gone: Vec<SlotId> = inner
            .slots
            .iter()
            .filter(|(key, entry)| !kept(key, entry))
            .map(|(key, _)| *key)
            .collect();
        for key in gone {
            dropped.entries.extend(inner.slots.remove(&key));
        }
        for (key, row) in rows {
            if inner.slots.contains_key(key) {
                continue;
            }
            if inner.commands.is_full() || !self.room.has_room(retirement_capacity(self.voices)) {
                break;
            }
            let built = Installed::of(*key, row, self.channels);
            let entry = Entry {
                row: row.clone(),
                spare: false,
                hold: built.hold_cell(),
                stream: row.stream(),
            };
            match self.queue(&mut inner.commands, Message::Install(Box::new(built))) {
                Ok(()) => {
                    inner.slots.insert(*key, entry);
                }
                Err(Message::Install(built)) => {
                    dropped.built.push(*built);
                    break;
                }
                Err(Message::SetSpare { .. }) => unreachable!("an install is queued"),
            }
        }
        dropped
    }

    /// Frees everything the audio thread has retired since the last call, and gives
    /// the room it held back. Returns how many values were freed.
    fn collect_retired(&self) -> usize {
        let mut freed = 0;
        for value in self.retired.lock().drain() {
            drop(value);
            freed += 1;
        }
        self.room.release(freed);
        freed
    }

    /// Queues `message` for the audio thread with a place reserved in the retirement
    /// ring for what it may retire, or hands it back when the ring or the room is full.
    fn queue(
        &self,
        writer: &mut RingWriter<Message<F>>,
        message: Message<F>,
    ) -> Result<(), Message<F>> {
        if writer.is_full() || !self.room.reserve(retirement_capacity(self.voices)) {
            return Err(message);
        }
        match writer.push(message) {
            Ok(()) => Ok(()),
            Err(returned) => {
                self.room.release(1);
                Err(returned.value)
            }
        }
    }

    /// Sends each voice the spare its window and the loop ask for, and takes the spare of
    /// each that no longer wants one; a voice the ring has no room for stays as it was.
    /// At most one spare per voice, which is what the retirement ring is sized for.
    fn reconcile_spares(&self) {
        let start = self.loops.start();
        let table = self.table.current();
        let mut unused = Vec::new();
        {
            let mut inner = self.inner.lock();
            let Inner { slots, commands } = &mut *inner;
            for (key, row) in table.rows() {
                let Some(entry) = slots
                    .get_mut(key)
                    .filter(|entry| row.same_structure(&entry.row))
                else {
                    continue;
                };
                let values = row.values();
                let wants = values.stretching
                    && values
                        .window
                        .zip(start)
                        .is_some_and(|(window, start)| window_covers(window, start));
                if wants == entry.spare {
                    continue;
                }
                // Nothing is built or queued while the ring or the retirement room is
                // full: the refusal is counted and the next tick retries.
                if commands.is_full() || !self.room.has_room(retirement_capacity(self.voices)) {
                    DrainCounters::bump(&self.counters.spares_deferred);
                    continue;
                }
                let spare = wants.then(|| stretched::spare_vocoder::<F>(self.channels));
                match self.queue(commands, Message::SetSpare { id: *key, spare }) {
                    Ok(()) => entry.spare = wants,
                    Err(refused) => {
                        DrainCounters::bump(&self.counters.spares_deferred);
                        unused.push(refused);
                    }
                }
            }
        }
        // What was not queued is dropped after the lock: a spare's rings are large.
        drop(unused);
    }

    /// How many voices carry a spare vocoder the controls sent them: what the
    /// transport's loop asks of the pool, which the audio thread has taken in or is
    /// about to.
    #[must_use]
    pub fn spares(&self) -> usize {
        self.inner
            .lock()
            .slots
            .values()
            .filter(|entry| entry.spare)
            .count()
    }

    /// Reserves every retirement slot but `left`, as uncollected retirements would hold them.
    #[cfg(test)]
    pub(crate) fn fill_room(&self, left: usize) {
        while self.room.reserve(retirement_capacity(self.voices) - left) {}
    }

    /// Gives `n` retirement slots back.
    #[cfg(test)]
    pub(crate) fn free_room(&self, n: usize) {
        self.room.release(n);
    }

    /// The transport's loop, as a test sets it for the controls to read.
    #[cfg(test)]
    pub(crate) fn loop_feed(&self) -> &Arc<LoopFeed> {
        &self.loops
    }
}

/// What a sync leaves to drop once the lock is released.
struct Dropped<F: SamplerFloat> {
    entries: Vec<Entry<F>>,
    built: Vec<Installed<F>>,
}

impl<F: SamplerFloat> Default for Dropped<F> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            built: Vec::new(),
        }
    }
}

/// The table a session writes: an equal one changes nothing. Otherwise the table is
/// published to the cell the pool lends, a streamed row's loop and direction are
/// written to its stream's cells, and the voices the new rows need are built and sent.
impl<F: SamplerFloat> Published for VoicePoolControls<F> {
    type Value = Arc<VoiceTable<F>>;

    fn current(&self) -> Arc<VoiceTable<F>> {
        self.table.current()
    }

    fn replace(&self, value: &Arc<VoiceTable<F>>) {
        let gone = {
            let mut inner = self.inner.lock();
            if *self.table.current() == **value {
                return;
            }
            self.table.publish(Arc::clone(value));
            for (_, row) in value.rows() {
                row.steer_stream();
            }
            self.sync(&mut inner, value)
        };
        drop(gone);
    }
}

/// Frees what the audio thread retired (the slots no row matched, the spares it
/// did not keep) and the tables a replacement left behind, sends what a full ring held
/// back, and reclaims the cells of the pool's streams; control thread, from the
/// owner's tick.
impl<F: SamplerFloat> Reclaim for VoicePoolControls<F> {
    fn reclaim(&self) -> usize {
        self.collect_retired();
        let pinned = self.table.reclaim();
        // The voices a full ring held back, and the spares the loop now asks for.
        self.send_missing();
        self.reconcile_spares();
        // Each stream's cells, cloned under the lock and reclaimed after it.
        let streams: Vec<_> = self
            .inner
            .lock()
            .slots
            .values()
            .filter_map(|entry| entry.stream.clone())
            .collect();
        pinned
            + streams
                .iter()
                .map(|stream| {
                    let stream = stream.lock();
                    stream.looping_cell().reclaim() + stream.direction_cell().reclaim()
                })
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::placed_config;
    use crate::voice::memory_source::VoiceWindow;
    use crate::voice::node_config::VoiceNodeConfig;
    use crate::voice::pool::VoicePool;
    use crate::voice::table::VoicePoolConfig;
    use tutti_units::{SampleRate, StretchFactor};

    /// A full retirement room refuses a spare on the control side: `SetSpare` reserves a
    /// retirement slot like every message that may retire, so with none left nothing is
    /// queued, the refusal is counted (`PoolDrops::spares_deferred`), and the next tick
    /// sends it once a slot is free. No box reaches the audio thread to be dropped there.
    #[test]
    fn a_full_retirement_room_defers_the_spare_and_the_next_tick_sends_it() {
        let wave = Arc::new(tutti_buffers::Wave::<f32>::new(2, SampleRate::SR_48K));
        let source = placed_config(wave, VoiceWindow::from(Beat::new(0.0)));
        let row = VoiceNodeConfig::placed(source)
            .expect("a finite gain on a placed source")
            .stretch(StretchFactor::new(1.5));
        let config = VoicePoolConfig::<f32>::new(ChannelLayout::STEREO)
            .expect("stereo")
            .row(SlotId(1), row)
            .expect("one row in a free key");
        let (_pool, controls) = VoicePool::from(config).controlled();
        // Every retirement slot taken, as uncollected retirements would hold them.
        controls.fill_room(0);
        let range = LoopRange::new(Beat::new(0.0), Beat::new(1.0)).expect("a loop");
        controls.loop_feed().publish(Some(range));
        controls.reclaim();
        assert_eq!(controls.spares(), 0, "nothing was sent");
        let deferred = controls.drops().spares_deferred;
        assert!(deferred >= 1, "the refusal is counted");
        controls.reclaim();
        assert!(
            controls.drops().spares_deferred > deferred,
            "a tick without room defers again"
        );
        assert_eq!(controls.spares(), 0, "still nothing sent");
        controls.free_room(1);
        controls.reclaim();
        assert_eq!(controls.spares(), 1, "the next tick with room sends it");
    }
}
