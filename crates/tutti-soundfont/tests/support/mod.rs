//! A SoundFont unit driven by hand the way a graph drives it: MIDI queued with
//! [`Hand::queue_midi`] is the next block's event input, each event on its
//! `Offset`, and a block is `Node::process` through
//! `tutti_graph::contract::Direct` (buffers built once, so the allocation
//! gates can run it).
//!
//! The node plays only its event input, so this is how a test feeds it MIDI.

#![allow(dead_code)] // each test binary uses its own subset

use std::ops::{Deref, DerefMut};
use tutti_graph::UmpAt;

pub(crate) mod font;
pub(crate) mod rig;

use tutti_graph::contract::{Direct, HAND_EVENT_CAPACITY};
use tutti_graph::{Event, InEvents, Offset, Status};
use tutti_soundfont::SoundFontUnit;
use tutti_ump::Ump;
use tutti_units::{SampleRate, Samples};

/// The block length a [`Hand`] renders: 64 frames.
pub(crate) const BLOCK: usize = 64;

/// A [`SoundFontUnit`] driven by hand. Derefs to the unit, so its own
/// methods (`note_on`, `program_change`, …) are reachable.
pub(crate) struct Hand {
    direct: Direct<SoundFontUnit<f32>>,
    input: InEvents,
    queued: Vec<Event>,
    /// What the last block returned, and so what its outputs are: a node that
    /// returns `Silent` or `Idle` wrote nothing, and the executor zeroes.
    status: Status,
    len: usize,
    silence: Vec<f32>,
}

impl Hand {
    /// `unit`, prepared at its own rate for [`BLOCK`]-frame blocks.
    pub(crate) fn new(unit: SoundFontUnit<f32>) -> Self {
        let rate = unit.sample_rate();
        Self::at(unit, rate)
    }

    /// `unit`, prepared at `rate` for [`BLOCK`]-frame blocks.
    pub(crate) fn at(unit: SoundFontUnit<f32>, rate: SampleRate) -> Self {
        let direct = Direct::new(unit, rate, BLOCK);
        Self {
            input: tutti_graph::Node::shape(&*direct.node)
                .event_input(0)
                .expect("an event input"),
            direct,
            queued: Vec::with_capacity(HAND_EVENT_CAPACITY),
            status: Status::Modified,
            len: BLOCK,
            silence: vec![0.0; BLOCK],
        }
    }

    /// The unit, with the state it has, out of its harness: a new `Hand` over
    /// it prepares it again (at another rate, say).
    pub(crate) fn into_unit(self) -> SoundFontUnit<f32> {
        *self.direct.node
    }

    /// Play `events` in the next block, each on its first frame.
    /// Allocation-free up to `HAND_EVENT_CAPACITY` a block.
    pub(crate) fn queue_midi(&mut self, events: &[Ump]) {
        for e in events {
            self.queued.push(Event::midi(Offset::ZERO, *e));
        }
    }

    /// Play `events` in the next block, each on its own `Offset`.
    /// Allocation-free up to `HAND_EVENT_CAPACITY` a block.
    pub(crate) fn queue_midi_at(&mut self, events: &[UmpAt]) {
        for (at, ump) in events {
            self.queued.push(Event::midi(*at, *ump));
        }
    }

    /// One block of `frames` (at most [`BLOCK`]): the queued events in it,
    /// stable-sorted by offset (an event past a shorter block lands on its
    /// last frame). `(left, right)`, the dry pair.
    pub(crate) fn block(&mut self, frames: usize) -> (&[f32], &[f32]) {
        self.run(frames);
        (self.plane(0), self.plane(1))
    }

    /// The status the last block returned.
    pub(crate) fn status(&self) -> Status {
        self.status
    }

    /// Output `c` of the last block, as the executor would hand it on: zeros
    /// for a node that returned `Silent` or `Idle`.
    pub(crate) fn plane(&self, c: usize) -> &[f32] {
        match self.status {
            Status::Silent | Status::Idle => &self.silence[..self.len],
            _ => self.direct.output().plane(c).samples(),
        }
    }

    /// One block of `frames`, as [`block`](Self::block), leaving the outputs to
    /// [`plane`](Self::plane).
    pub(crate) fn run(&mut self, frames: usize) -> Status {
        self.direct.set_block_len(frames);
        let last = Offset::of(Samples::new(frames - 1), Samples::new(frames));
        for e in &mut self.queued {
            if e.offset.index() >= frames {
                e.offset = last;
            }
        }
        // Stable (events on one frame keep their queued order) and
        // allocation-free: an insertion sort, not `sort_by_key`'s merge.
        for i in 1..self.queued.len() {
            let mut j = i;
            while j > 0 && self.queued[j - 1].offset > self.queued[j].offset {
                self.queued.swap(j - 1, j);
                j -= 1;
            }
        }
        self.direct.events(self.input, &self.queued);
        self.queued.clear();
        self.status = self.direct.block();
        self.len = frames;
        self.status
    }

    /// One frame (a one-frame block): `[left, right]`.
    pub(crate) fn tick(&mut self) -> [f32; 2] {
        let (l, r) = self.block(1);
        [l[0], r[0]]
    }
}

impl Deref for Hand {
    type Target = SoundFontUnit<f32>;
    fn deref(&self) -> &SoundFontUnit<f32> {
        &self.direct.node
    }
}

impl DerefMut for Hand {
    fn deref_mut(&mut self) -> &mut SoundFontUnit<f32> {
        &mut self.direct.node
    }
}
