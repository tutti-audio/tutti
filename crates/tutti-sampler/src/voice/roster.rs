//! A pool's installed voices, and how a published table reaches them.
//!
//! A slot is installed from a [`Row`] (built on the control side) and matched to
//! a row of the table by its key and [`Row::same_structure`]. The audio thread
//! never builds or frees one: [`Roster::merge`] applies the table's values to the
//! slots it still matches and hands every other slot back through the retirement
//! ring, and [`Roster::place`] takes a built slot in.

use std::sync::Arc;

use std::ops::Range;
use tutti_buffers::ChannelLayout;
use tutti_dsp::PhaseVocoder;

use tutti_graph::{ForkCause, RenderMode};
use tutti_rt::{AtomicCell, CappedVec, RingWriter};
use tutti_units::{SampleRate, Tail};

use super::clock::BlockClock;
use super::home::Seat;
use super::node::{apply_loop, apply_speed, apply_steering};
use super::node_config::Held;
use super::node_controls::StreamHold;
use super::pool::{DrainCounters, RetireRoom, Retired};
use super::slot::PlaybackSlot;
use super::stretched;
use super::table::{Row, RowValues};
use super::types::{Reach, SlotId, VoiceSource};
use crate::SamplerFloat;
use crate::lanes::BlockScratch;

/// One voice of a pool: a slot built from a row, the row's structure it was
/// built from, and the values it last applied.
///
/// Built on the control side and moved whole (boxed) to the audio thread and
/// back, so neither the audio thread nor the move allocates or frees. A drop
/// returns a streamed voice's reader to its config's home, so it happens on the
/// control side.
pub(crate) struct Installed<F: SamplerFloat> {
    slot: PlaybackSlot<F>,
    /// The row this voice was built from: its structure is what the table's rows
    /// are matched against. Its values are stale as soon as the table moves, which
    /// is why `applied` holds the ones in force.
    row: Row<F>,
    applied: RowValues,
    /// How a streamed voice holds its reader; `Detached` for a resident one.
    seat: Seat<F>,
    /// Whether a streamed voice holds its reader yet, for the controls to read.
    hold: Option<Arc<AtomicCell<StreamHold>>>,
    /// No row matches the voice and it waits for a place in the retirement ring: it is
    /// not rendered.
    muted: bool,
}

impl<F: SamplerFloat> Installed<F> {
    /// The voice `row` describes at `key`, with the vocoder it wants. Control
    /// side: it allocates, and a streamed row takes its reader or a place in the
    /// queue for it.
    pub(crate) fn of(key: SlotId, row: &Row<F>, channels: ChannelLayout) -> Self {
        let applied = row.values();
        let built = row.built();
        let (seat, hold) = match built.held {
            Some(Held { seat, hold, .. }) => (seat, Some(hold)),
            None => (Seat::Detached, None),
        };
        let mut slot = PlaybackSlot::new(key, Box::new(built.voice));
        slot.stretch = applied
            .stretching
            .then(|| stretched::vocoder::<F>(channels, false));
        Self {
            slot,
            row: row.clone(),
            applied,
            seat,
            hold,
            muted: false,
        }
    }

    /// The voice a fork plays for `row`: a copy sharing nothing with the live one,
    /// with a spare vocoder (nothing sends one later).
    pub(crate) fn forked(
        key: SlotId,
        row: &Row<F>,
        channels: ChannelLayout,
        mode: RenderMode,
    ) -> Result<Self, ForkCause> {
        let applied = row.values();
        let mut slot = PlaybackSlot::new(key, Box::new(row.forked(mode)?));
        slot.stretch = applied
            .stretching
            .then(|| stretched::vocoder::<F>(channels, true));
        Ok(Self {
            slot,
            row: row.clone(),
            applied,
            seat: Seat::Detached,
            hold: None,
            muted: false,
        })
    }

    /// The key of the row this voice plays.
    pub(crate) fn key(&self) -> SlotId {
        self.slot.id
    }

    /// Adds the voice's block `range` into `out`: [`PlaybackSlot::render_into`].
    #[inline]
    pub(crate) fn render_into(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        width: usize,
        scratch: &mut BlockScratch<F>,
        out: &mut [&mut [F]],
    ) {
        self.slot.render_into(clock, range, width, scratch, out);
    }

    /// Drops the audio the voice holds mid-flight.
    pub(crate) fn flush(&mut self) {
        self.slot.flush_playhead_state();
    }

    /// The ring-out of the voice a fork holds (`Voice::ring_out`).
    pub(crate) fn ring_out(&self, reach: Reach) -> Tail {
        self.slot.voice.ring_out(reach)
    }

    /// Runs the voice's source at `rate` in `mode`.
    pub(crate) fn set_render(&mut self, rate: SampleRate, mode: RenderMode) {
        self.slot.voice.source.set_render(rate, mode);
    }

    /// Whether the voice carries a spare vocoder.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn has_spare(&self) -> bool {
        self.slot.has_spare()
    }

    /// The slot, for a test that reads what it holds.
    #[cfg(test)]
    pub(crate) fn slot(&self) -> &PlaybackSlot<F> {
        &self.slot
    }

    /// The slot, for a test that edits what it holds.
    #[cfg(test)]
    pub(crate) fn slot_mut(&mut self) -> &mut PlaybackSlot<F> {
        &mut self.slot
    }

    /// Whether `row` is the row this voice was built from, but for its values: the
    /// one question a table's row is matched by.
    pub(crate) fn plays(&self, row: &Row<F>) -> bool {
        row.same_structure(&self.row)
    }

    /// Mutes the voice (it is not rendered), or lets it sound again.
    fn mute(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// The cell a streamed voice reports whether it holds its reader in, for the
    /// controls to read.
    pub(crate) fn hold_cell(&self) -> Option<Arc<AtomicCell<StreamHold>>> {
        self.hold.clone()
    }

    /// Whether the voice is not rendered: stopped by hand (a free-running voice), or
    /// muted because no row matches it and the retirement ring had no place for it
    /// yet.
    pub(crate) fn stopped(&self) -> bool {
        self.applied.steering.stopped || self.muted
    }

    /// A streamed voice that waited for its reader takes it between blocks, and the
    /// voice it takes is given what the template had been: the render rate, the
    /// gain and the speed. Wait-free.
    #[inline]
    pub(crate) fn receive(&mut self, rate: SampleRate, mode: RenderMode) {
        let VoiceSource::Disk(disk) = &mut self.slot.voice.source else {
            return;
        };
        if !self.seat.receive(disk) {
            return;
        }
        disk.set_render(rate, mode);
        disk.store_gain(self.slot.voice.play.gain);
        disk.set_speed(self.slot.voice.play.speed);
        if let Some(hold) = &self.hold {
            hold.store(StreamHold::Holding);
        }
    }

    /// Applies what `now` asks over what this voice last applied, a value at a
    /// time and only the ones that differ. A streamed voice's loop and direction
    /// are its stream's cells, written by the controls, so only a resident voice
    /// takes them here. Allocation-free and free-free.
    fn apply(&mut self, now: &RowValues) {
        let was = self.applied;
        if *now == was {
            return;
        }
        let voice = &mut *self.slot.voice;
        if now.play.gain != was.play.gain {
            voice.source.apply_gain(now.play.gain);
            voice.play.gain = now.play.gain;
        }
        if now.play.speed != was.play.speed {
            apply_speed(voice, now.play.speed);
        }
        if self.hold.is_none() {
            if now.play.loop_ != was.play.loop_ {
                apply_loop(voice, now.play.loop_);
            }
            voice.play.direction = now.play.direction;
        }
        if let Some(window) = now.window.filter(|w| Some(*w) != was.window) {
            voice.source.apply_window(window);
        }
        if now.steering != was.steering {
            apply_steering(voice, &was.steering, &now.steering);
        }
        if now.stretching {
            voice.play.stretch = now.play.stretch;
            voice.play.pitch = now.play.pitch;
        }
        self.applied = *now;
    }
}

/// A node returns a streamed voice's reader to the config it was built from. A
/// voice is dropped on the control side (the controls collect what the audio
/// thread retired, or a pool is dropped with its graph unit), where the blocking
/// `Mutex` is allowed.
impl<F: SamplerFloat> Drop for Installed<F> {
    fn drop(&mut self) {
        let seat = std::mem::replace(&mut self.seat, Seat::Detached);
        if let VoiceSource::Disk(disk) = &mut self.slot.voice.source {
            seat.release(disk);
        }
    }
}

/// Where the audio thread hands what it must not drop.
pub(crate) struct Exit<F: SamplerFloat> {
    retired: RingWriter<Retired<F>>,
    /// Values retired after every control was dropped, when the ring has no
    /// reader: held until the pool is dropped, which is on the control side.
    stranded: CappedVec<Retired<F>>,
    pub(crate) room: Arc<RetireRoom>,
    counters: Arc<DrainCounters>,
}

impl<F: SamplerFloat> Exit<F> {
    pub(crate) fn new(
        retired: RingWriter<Retired<F>>,
        stranded: usize,
        room: Arc<RetireRoom>,
        counters: Arc<DrainCounters>,
    ) -> Self {
        Self {
            retired,
            stranded: CappedVec::with_capacity(stranded),
            room,
            counters,
        }
    }

    /// Hands `value` to the control thread. The ring always has room: the controls
    /// reserved a place in it for every slot and spare they sent. A push the ring
    /// refuses can only be a disconnected ring, and the value is parked in
    /// `stranded`, freed with the pool on the control side.
    pub(crate) fn retire(&mut self, value: Retired<F>) {
        let Err(returned) = self.retired.push(value) else {
            return;
        };
        let disconnected = self.retired.is_disconnected();
        debug_assert!(
            disconnected,
            "the retirement ring has room for every reservation"
        );
        if let Err(last) = self.stranded.push_within_capacity(returned.value) {
            // Unreachable: a reservation exists for each value, and the park holds as
            // many as the ring does.
            debug_assert!(false, "more retirements after disconnect than reservations");
            DrainCounters::bump(&self.counters.leaked);
            std::mem::forget(last);
        }
    }

    /// Whether the ring has a place for one more value.
    pub(crate) fn has_room(&self) -> bool {
        !self.retired.is_full()
    }

    /// A spare vocoder the slot kept: the reservation made for it is free again.
    pub(crate) fn kept(&self) {
        self.room.release(1);
    }
}

/// The voices a pool holds, in key order, and the door they leave by.
pub(crate) struct Roster<F: SamplerFloat> {
    /// Reserved at the pool's voices: a slot is taken in only where a row of the
    /// table holds a place for it, so a push never reallocates.
    pub(crate) voices: CappedVec<Box<Installed<F>>>,
    pub(crate) exit: Exit<F>,
    /// The places `voices` was reserved with.
    pub(crate) capacity: usize,
    /// A voice no row matches is waiting for a place in the retirement ring: the next
    /// block merges again.
    pub(crate) stuck: bool,
}

impl<F: SamplerFloat> Roster<F> {
    pub(crate) fn new(voices: usize, exit: Exit<F>) -> Self {
        Self {
            voices: CappedVec::with_capacity(voices),
            exit,
            capacity: voices,
            stuck: false,
        }
    }

    /// Makes the slots what `rows` say, in one pass over both (O(rows + slots)):
    /// a slot with a row of its key and structure takes the row's values, and any
    /// other is gone from this block on (it is not rendered) and goes back through
    /// the retirement ring. A row with no slot renders nothing until its voice
    /// arrives ([`place`](Self::place)). Allocation-free and free-free.
    pub(crate) fn merge(&mut self, rows: &[(SlotId, Row<F>)]) {
        self.stuck = false;
        let mut row = 0;
        let mut i = 0;
        while i < self.voices.len() {
            let key = self.voices.as_slice()[i].key();
            while row < rows.len() && rows[row].0.0 < key.0 {
                row += 1;
            }
            let matched = rows
                .get(row)
                .filter(|(held, wanted)| *held == key && self.voices.as_slice()[i].plays(wanted));
            match matched {
                Some((_, wanted)) => {
                    self.voices.as_mut_slice()[i].mute(false);
                    self.voices.as_mut_slice()[i].apply(&wanted.values());
                    i += 1;
                }
                None if !self.exit.has_room() => {
                    // Unreachable while the controls' reservations hold. The voice is
                    // not rendered and is retired by a later merge; it is never
                    // dropped here.
                    self.voices.as_mut_slice()[i].mute(true);
                    self.stuck = true;
                    i += 1;
                }
                None => {
                    let gone = self.voices.remove(i);
                    self.exit.retire(Retired::Slot(gone));
                }
            }
        }
    }

    /// Takes in a slot the controls built: at its place by key, at the values of the
    /// row it matches. A slot no row matches any more (the table moved on while it
    /// was on its way), or one a matching slot already holds, goes back through the
    /// retirement ring.
    pub(crate) fn place(
        &mut self,
        rows: &[(SlotId, Row<F>)],
        mut slot: Box<Installed<F>>,
        rate: SampleRate,
        mode: RenderMode,
    ) {
        let key = slot.key();
        let wanted = rows
            .binary_search_by_key(&key.0, |(held, _)| held.0)
            .ok()
            .map(|at| &rows[at].1)
            .filter(|row| slot.plays(row));
        let Some(wanted) = wanted else {
            self.exit.retire(Retired::Slot(slot));
            return;
        };
        slot.set_render(rate, mode);
        slot.apply(&wanted.values());
        // A voice the controls built again for a row a slot already plays (the table
        // went to another structure under the key and came back before a block) is
        // the controls' record of the voice at the key: it replaces the older slot,
        // which goes back through the retirement ring, so the controls' hold and
        // spare flags describe the voice that sounds.
        if let Some(old) = self
            .voices
            .iter_mut()
            .find(|v| v.key() == key && v.plays(wanted))
        {
            let old = std::mem::replace(old, slot);
            self.exit.retire(Retired::Slot(old));
            return;
        }
        let at = self
            .voices
            .iter()
            .take_while(|v| v.key().0 <= key.0)
            .count();
        if let Err(slot) = self.voices.insert_within_capacity(at, slot) {
            self.exit.retire(Retired::Slot(slot));
        }
    }

    /// Gives the slot at `id` a spare vocoder (`Some`), or takes its spare away
    /// (`None`); what it does not keep is retired, never dropped here.
    pub(crate) fn set_spare(&mut self, id: SlotId, spare: Option<Box<PhaseVocoder<F>>>) {
        // By key alone: at most one slot of a key matches the table at any time (`merge`
        // retires the rest and `place` keeps one), and the controls send a spare only
        // for the voice at the key they last sent.
        let left = match self.voices.iter_mut().find(|v| v.key() == id) {
            Some(v) => v.slot.set_spare(spare),
            None => spare,
        };
        match left {
            Some(unit) => self.exit.retire(Retired::Spare(unit)),
            None => self.exit.kept(),
        }
    }
}
