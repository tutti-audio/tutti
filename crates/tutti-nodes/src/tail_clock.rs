//! [`TailClock`]: how long a node still owes its ring-out, and how many frames of a block
//! may still sound.
//!
//! A node that declares a finite or unbounded tail holds itself to it: the clock counts the
//! frames since the last non-silent input frame, keeps the longest ring-out the settings in
//! force since may still need (never past the declared ceiling), and says how much of the
//! next block is live. What it does with the answer (report it, cut the output, park) is the
//! node's. The clock is shared by [`KernelNode`](crate::KernelNode) and
//! [`FamilyNode`](crate::FamilyNode).

use tutti_graph::MAX_NODE_TAIL;
use tutti_types::{Samples, Tail};

/// A tail of this many frames never ends.
pub(crate) const UNBOUNDED: u64 = u64::MAX;

/// A tail in whole frames: [`UNBOUNDED`] when it never ends or is not known.
pub(crate) fn tail_frames(tail: Tail) -> u64 {
    match tail {
        Tail::Finite(s) => s.get() as u64,
        Tail::None => 0,
        Tail::Unknown | Tail::Unbounded => UNBOUNDED,
    }
}

/// A tail from whole frames: unbounded for [`UNBOUNDED`] and for a figure past
/// [`MAX_NODE_TAIL`], which the graph refuses as a finite tail and counts as never decaying
/// where it is not refused. Nothing the clock reports past the bound is finite.
pub(crate) fn frames_tail(frames: u64) -> Tail {
    if frames == UNBOUNDED || frames > MAX_NODE_TAIL.get() as u64 {
        Tail::Unbounded
    } else {
        Tail::Finite(Samples(usize::try_from(frames).unwrap_or(usize::MAX)))
    }
}

/// What one block asks of the clock: where its last non-silent input frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Heard {
    /// The block's frame count.
    pub size: usize,
    /// Its last frame whose input is not zero, if any.
    pub last_input: Option<usize>,
}

/// What the clock answers for a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Owed {
    /// The ring-out to report, held at the longer figure until a shorter one has been heard
    /// out.
    pub tail: Tail,
    /// How many frames of the block may still sound.
    pub live: usize,
}

/// The ring-out a node owes, in frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TailClock {
    /// What `Shape::tail` declares: the most it can ring.
    ceiling: u64,
    /// The processing latency: a tail starts after it.
    latency: u64,
    /// Frames of ring-out after the last non-silent input frame that the settings in force
    /// since may still need.
    ring: u64,
    /// Frames since the last non-silent input frame, up to the end of the previous block
    /// ([`UNBOUNDED`] when there never was one).
    since: u64,
}

impl TailClock {
    /// The clock of a node that declares `tail` with `latency` frames of processing latency,
    /// or `None` when the tail is only declared ([`Tail::Unknown`], [`Tail::None`]): the node
    /// then neither narrows, cuts nor parks.
    pub(crate) fn of(tail: Tail, latency: u64) -> Option<Self> {
        let ceiling = match tail {
            Tail::Finite(s) => s.get() as u64,
            Tail::Unbounded => UNBOUNDED,
            Tail::Unknown | Tail::None => return None,
        };
        Some(Self::new(ceiling, latency))
    }

    /// A clock under `ceiling` frames (`UNBOUNDED` for none), with no ring-out owed and no
    /// input heard.
    pub(crate) fn new(ceiling: u64, latency: u64) -> Self {
        Self {
            ceiling,
            latency,
            ring: 0,
            since: UNBOUNDED,
        }
    }

    /// No ring-out owed, no input heard.
    pub(crate) fn clear(&mut self) {
        self.ring = 0;
        self.since = UNBOUNDED;
    }

    /// The ceiling, in frames.
    pub(crate) fn ceiling(&self) -> u64 {
        self.ceiling
    }

    /// Takes a block in which `heard` says where the input was, under settings that ring
    /// `current` frames (`UNBOUNDED` for ever): the tail owed measured from the last
    /// non-silent input frame, and how many frames of the block are live.
    pub(crate) fn advance(&mut self, current: u64, heard: Heard) -> Owed {
        let Heard { size, last_input } = heard;
        // A tail that was unbounded (a freeze) and is not now rings out from here: the energy
        // it held is in the loop, owed to the settings in force.
        if self.ring == UNBOUNDED && current != UNBOUNDED {
            self.ring = current;
            self.since = 0;
        }
        let since = self.since;
        match last_input {
            Some(i) => {
                self.ring = self.ring.saturating_sub(since.saturating_add(1 + i as u64));
                self.since = (size - 1 - i) as u64;
            }
            None => self.since = since.saturating_add(size as u64),
        }
        self.ring = self.ring.max(current).min(self.ceiling);

        // The latency before the tail starts (a tail excludes latency).
        let owed = if self.ring == UNBOUNDED {
            UNBOUNDED
        } else {
            self.ring.saturating_add(self.latency)
        };
        let live = if owed == UNBOUNDED {
            size as u64
        } else {
            match last_input {
                Some(i) => (i as u64).saturating_add(owed).saturating_add(1),
                None => owed.saturating_sub(since),
            }
        };
        Owed {
            tail: frames_tail(self.ring),
            live: live.min(size as u64) as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silent(size: usize) -> Heard {
        Heard {
            size,
            last_input: None,
        }
    }

    /// A tail only declared leaves the node alone; a finite or unbounded one is held to.
    #[test]
    fn only_a_finite_or_unbounded_tail_has_a_clock() {
        assert_eq!(TailClock::of(Tail::Unknown, 0), None);
        assert_eq!(TailClock::of(Tail::None, 0), None);
        assert_eq!(
            TailClock::of(Tail::Finite(Samples(5)), 2),
            Some(TailClock::new(5, 2))
        );
        assert_eq!(
            TailClock::of(Tail::Unbounded, 0),
            Some(TailClock::new(UNBOUNDED, 0))
        );
    }

    /// With no input ever heard nothing is owed: the first block is already dead.
    #[test]
    fn a_node_that_has_heard_nothing_owes_nothing() {
        let mut c = TailClock::new(1_000, 0);
        let owed = c.advance(0, silent(64));
        assert_eq!(owed.live, 0);
        assert_eq!(owed.tail, Tail::Finite(Samples(0)));
    }

    /// An impulse on a block's frame 0 with a tail of 100 and a latency of 10 sounds for the
    /// latency, the tail and the impulse's own frame: 111 frames, across two blocks of 64.
    /// The tail is excluded from the latency, so the reported figure is the 100.
    #[test]
    fn the_live_frames_are_the_latency_the_tail_and_the_impulse() {
        let mut c = TailClock::new(1_000, 10);
        let first = c.advance(
            100,
            Heard {
                size: 64,
                last_input: Some(0),
            },
        );
        assert_eq!(first.tail, Tail::Finite(Samples(100)));
        assert_eq!(first.live, 64);
        let second = c.advance(100, silent(64));
        assert_eq!(second.live, 111 - 64);
        let third = c.advance(100, silent(64));
        assert_eq!(third.live, 0);
    }

    /// The last input in the middle of a block starts the tail from there: frame 20 of 64,
    /// a tail of 100 and no latency is live to frame 121, so all of this block and 57 of
    /// the next.
    #[test]
    fn the_tail_runs_from_the_last_input_frame_of_a_block() {
        let mut c = TailClock::new(1_000, 0);
        let first = c.advance(
            100,
            Heard {
                size: 64,
                last_input: Some(20),
            },
        );
        assert_eq!(first.live, 64);
        let second = c.advance(100, silent(64));
        assert_eq!(second.live, 121 - 64);
        assert_eq!(c.advance(100, silent(64)).live, 0);
    }

    /// A tail never goes past its ceiling, and a shorter decay does not shorten what is
    /// already owed: the longer figure is held until it has been heard out.
    #[test]
    fn a_shorter_decay_keeps_the_longer_tail_until_it_is_heard_out() {
        let mut c = TailClock::new(150, 0);
        let first = c.advance(
            900,
            Heard {
                size: 64,
                last_input: Some(63),
            },
        );
        assert_eq!(first.tail, Tail::Finite(Samples(150)), "the ceiling");
        let second = c.advance(20, silent(64));
        assert_eq!(second.tail, Tail::Finite(Samples(150)), "held");
        assert_eq!(second.live, 64);
        assert_eq!(c.advance(20, silent(64)).live, 64);
        assert_eq!(c.advance(20, silent(64)).live, 150 - 128);
    }

    /// A tail that was unbounded (a freeze) and is not now rings out from the thaw: its
    /// figure is the new one, counted from there.
    #[test]
    fn a_thaw_rings_out_from_the_block_it_is_seen_in() {
        let mut c = TailClock::new(UNBOUNDED, 0);
        let held = c.advance(UNBOUNDED, silent(64));
        assert_eq!(held.tail, Tail::Unbounded);
        assert_eq!(held.live, 64);
        let thaw = c.advance(100, silent(64));
        assert_eq!(thaw.tail, Tail::Finite(Samples(100)));
        assert_eq!(thaw.live, 64);
        assert_eq!(c.advance(100, silent(64)).live, 100 - 64);
        assert_eq!(c.advance(100, silent(64)).live, 0);
    }

    /// A ring-out past what a graph takes as finite is reported unbounded, whatever the
    /// ceiling: the bound itself is still finite, one frame past it is not, and the node's
    /// own count (what is live) goes on in frames.
    #[test]
    fn nothing_past_the_graphs_bound_is_reported_finite() {
        let bound = MAX_NODE_TAIL.get() as u64;
        let mut c = TailClock::new(UNBOUNDED, 0);
        let at = c.advance(
            bound,
            Heard {
                size: 64,
                last_input: Some(0),
            },
        );
        assert_eq!(at.tail, Tail::Finite(MAX_NODE_TAIL));
        let past = c.advance(bound + 1, silent(64));
        assert_eq!(past.tail, Tail::Unbounded);
        assert_eq!(past.live, 64, "it still rings, frame by frame");
        assert_eq!(frames_tail(bound + 1), Tail::Unbounded);
        assert_eq!(frames_tail(bound), Tail::Finite(MAX_NODE_TAIL));
    }

    /// `clear` forgets what was owed and heard.
    #[test]
    fn a_cleared_clock_owes_nothing() {
        let mut c = TailClock::new(1_000, 0);
        c.advance(
            500,
            Heard {
                size: 64,
                last_input: Some(0),
            },
        );
        c.clear();
        assert_eq!(c, TailClock::new(1_000, 0));
        assert_eq!(c.advance(0, silent(64)).live, 0);
    }
}
