#![doc = include_str!("../README.md")]
//! Musical time: where the playhead is, how fast it moves, and what moves it.
//!
//! Split by *who decides*. [`TransportSettings`] holds the plain values (tempo,
//! loop region, recording) that anyone may store into; [`MotionFsm`] holds the
//! state machine that may reject or defer a play/stop/locate. [`Transport`] is
//! the two halves as one handle, and [`TransportAudio`] holds the engine's
//! playhead, which the graph reads from each block's `Env`.
//!
//! The traits below are the read side: [`Timeline`] is what a reader outside
//! the graph consults (a node reads its block's `Env`)
//! and [`RenderClock`] is what a renderer drives.
//!
//! `tutti-engine`'s `Engine` holds a [`TransportAudio`], the audio-thread side
//! of a [`Transport`] (taken once): the clock, the motion machine and the
//! timed commands behind one value that private fields keep in order.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod audio;
#[cfg(test)]
mod beat_walk;
mod clock;
mod fsm;
mod handle;
mod loop_region;
mod motion;
mod offline;
mod project_meter;
mod settings;
mod state;
mod tempo_reader;
mod timed;
mod timeline;
mod walk;

use tutti_buffers::{Float, Planes, PlanesMut};

pub use audio::{Applied, AudioTaken, TransportAudio};
pub use handle::Transport;
pub use loop_region::{LoopEdit, LoopReader, LoopRegion};
pub use motion::{AfterLocate, FadeOut, MotionCommand, MotionFsm, MotionState};
pub use offline::{OfflineTimeline, OfflineTimelineConfig};
pub use project_meter::{
    ProjectMeter, TimeSignatureCell, TimeSignatureSlot, TimeSignatureSlotReader,
};
pub use settings::TransportSettings;
pub use tempo_reader::TempoReader;
pub use timed::{IMMEDIATE_RESERVE, SCHEDULE_CAPACITY, Scheduled, TimedError, TransportCommand};

pub use timeline::{OfflineClock, OfflineTransport, Timeline};
pub use walk::UpcomingTempo;

/// A clock an offline render drives, one block at a time.
///
/// [`Timeline`] is read-only — `beat`/`tempo_map`/`is_rolling` are what a reader
/// consults. Something has to move that position forward, and in a live session
/// it is the audio callback. Offline there is no callback, so the renderer does
/// it: after each block it reports how many frames it produced, and the clock
/// advances by exactly that much.
///
/// A renderer driving the graph (`tutti_graph::Executor`) must also *hand*
/// each block a transport, since
/// the graph's clock is the executor's `Env`, not a node — that is
/// [`graph_block`](Self::graph_block), which [`render_graph`] reads through
/// [`graph_piece`](Self::graph_piece) and so is the one order they are called in. It is still not a supertrait of
/// `Timeline`: a clock that does not move (`FrozenClock`) has a transport to
/// report — stopped, at beat zero — without being a timeline anything reads.
///
/// **Advance AFTER processing, never before.** The engine's clock (whose
/// transport beat-driven nodes read from `Env`) is
/// emit-then-advance: sample 0 of a block carries the block's start beat, and
/// only then does the beat increment. A clock advanced ahead of the render
/// sits one `beats_per_sample` off the transport its nodes read for the
/// entire render — a desync that reads as "the samplers are
/// slightly late" and nothing else.
pub trait RenderClock: Send + Sync {
    /// Advances the playhead by `frames` — a **frame** count, not a sample
    /// count, so it is independent of the render's channel width.
    ///
    /// Call after the block has been processed, never before.
    fn advance(&self, frames: tutti_units::Samples);

    /// The block about to be rendered as the graph takes it: the
    /// transport at the block's first frame, and the changes inside it. Read
    /// **before** the block is processed, as [`render_graph`] does through
    /// [`graph_piece`](Self::graph_piece), whose default is this over the
    /// whole block.
    ///
    /// Required, not defaulted. The obvious default — stopped, at beat zero —
    /// is right for a clock that does not move and wrong for every clock that
    /// does: a moving clock that fell back on it would advance its clip readers
    /// while every `Env` reader held beat zero, a
    /// desync that renders without an error. A clock that moves must say where
    /// it is.
    fn graph_block(
        &self,
    ) -> (
        tutti_graph::TransportPosition,
        tutti_graph::TransportChanges,
    );

    /// The first piece of a render of `frames`: the transport at its first
    /// frame, the changes inside it, and how many frames it covers. The
    /// default is [`graph_block`](Self::graph_block) over all `frames`; a
    /// clock whose block holds only so many changes covers fewer, and
    /// [`render_graph`] asks again for the rest after
    /// advancing.
    fn graph_piece(
        &self,
        frames: usize,
    ) -> (
        tutti_graph::TransportPosition,
        tutti_graph::TransportChanges,
        usize,
    ) {
        let (transport, changes) = self.graph_block();
        (transport, changes, frames)
    }

    /// The meter the graph's nodes read through `Cx::meter` while it renders
    /// under this clock. `None` (the default) is no meter: 4/4 from beat 0.
    fn meter(&self) -> Option<&tutti_music::MeterMap> {
        None
    }
}

/// Renders one block through `exec` under `clock`, then advances the clock by
/// it: [`graph_piece`](RenderClock::graph_piece),
/// `Executor::apply_pending` and `Executor::render_applied` (lending the
/// clock's [`meter`](RenderClock::meter)), then [`advance`](RenderClock::advance),
/// again for each piece — the one order that keeps every reader of the clock
/// on the frame the graph renders (emit-then-advance, as above). The block is
/// the frames of `outputs`, so it needs at least one output plane (a view
/// has none to give otherwise); a block of zero frames renders nothing and
/// leaves the clock where it was. `inputs` holds at least as many per channel, or
/// is [`Planes::default`] for a graph that reads none. A clock whose block
/// has more changes than the executor takes splits it into pieces, each
/// narrowed from the views as the executor's slice tables, held in fixed
/// arrays (the executor ignores slices past the plan's channels), so nothing
/// allocates; one that does not hands the executor the whole block, and a
/// node reads a transport change inside it from its `Env`.
///
/// # Panics
///
/// As `Executor::process_with_changes`: if a piece is past the executor's
/// prepared maximum block.
pub fn render_graph<F: Float>(
    clock: &dyn RenderClock,
    exec: &mut tutti_graph::Executor<F>,
    inputs: Planes<'_, F>,
    mut outputs: PlanesMut<'_, '_, F>,
) {
    use tutti_graph::MAX_PORTS;
    let frames = outputs.frames().get();
    let mut done = 0;
    while done < frames {
        let (transport, changes, n) = clock.graph_piece(frames - done);
        let piece = tutti_units::Samples::new(done)..tutti_units::Samples::new(done + n);
        // A graph with no inputs hands none.
        let ins = if inputs.channels() == 0 {
            Planes::default()
        } else {
            inputs.slice(piece.clone())
        };
        let mut outs = outputs.slice_mut(piece);
        let mut table_in: [&[F]; MAX_PORTS] = [&[]; MAX_PORTS];
        for (to, from) in table_in.iter_mut().zip(ins.iter()) {
            *to = from.samples();
        }
        let (ni, no) = (
            ins.channels().min(MAX_PORTS),
            outs.channels().min(MAX_PORTS),
        );
        let mut from = outs.iter_mut();
        let mut table_out: [&mut [F]; MAX_PORTS] = std::array::from_fn(|_| {
            from.next()
                .map_or(Default::default(), |o| o.into_samples_mut())
        });
        exec.apply_pending();
        exec.render_applied(
            n,
            &transport,
            &changes,
            &table_in[..ni],
            &mut table_out[..no],
            clock.meter(),
        );
        clock.advance(tutti_units::Samples::new(n));
        done += n;
    }
}

/// A clock that does not move.
///
/// For rendering a graph with no time-dependent nodes — a synth patch, a test
/// tone, an impulse response. It exists so that "this graph has no transport"
/// is something a caller *states* rather than something they omit: a renderer
/// takes a clock, so forgetting one is a compile error instead of a silently
/// silent render.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrozenClock;

impl RenderClock for FrozenClock {
    fn advance(&self, _frames: tutti_units::Samples) {}

    /// Stopped at beat zero, every block: "this graph has no transport".
    fn graph_block(
        &self,
    ) -> (
        tutti_graph::TransportPosition,
        tutti_graph::TransportChanges,
    ) {
        (
            tutti_graph::TransportPosition::default(),
            tutti_graph::TransportChanges::NONE,
        )
    }
}

#[cfg(test)]
mod error_bounds {
    //! Each error type this crate returns is an `Error` a caller can box and move
    //! (`?` into `Box<dyn Error + Send + Sync>`); the bound is checked where the
    //! function is written, so a non-`Send` field fails the build.

    fn err<T: std::error::Error + Send + Sync + 'static>() {}

    #[allow(dead_code)]
    fn the_errors_are_boxable() {
        err::<crate::AudioTaken>();
        err::<crate::TimedError<crate::TransportCommand>>();
    }
}
