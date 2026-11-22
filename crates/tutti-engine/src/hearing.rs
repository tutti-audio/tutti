//! The tap after the fader: "what I'm hearing".
//!
//! A tap in the plan (`tutti-graph`) copies the graph's signal before the
//! engine's declick fader and the fold to the device width. This is the other
//! kind: [`Engine`](crate::Engine) writes each chunk of its output buffer,
//! device-width and after the fader (the engine's output buffer in the graph's
//! float format, before the device layer clamps and converts it), to the rings of a [`TapReader`], the same
//! reader type. It is written by [`Engine::process`](crate::Engine::process)
//! and [`Engine::process_segment`](crate::Engine::process_segment), so a
//! device callback and an offline session's `render` feed it; an export forks
//! the graph and renders without the engine, so it never does.
//!
//! # The hand-over
//!
//! The writer reaches the audio thread over a bounded `tutti_rt::ring` of boxed
//! writers (the editor-to-executor pattern). The audio thread pops installs
//! at the start of each call, and the writer it replaces goes back over a
//! second queue to be freed on the control side: the audio thread never
//! drops one (`TapWriter` panics in debug if it is dropped inside
//! [`RtScope`](tutti_rt::RtScope), which the engine enters).
//!
//! **One pending install, latest wins.** [`open`](HeardTaps::open) and
//! [`close`](HeardTaps::close) always succeed: each sets the one pending
//! install, replacing the one not yet pushed (a replaced writer is closed on
//! the control side, and its reader ends:
//! [`Pull::Ended`](tutti_buffers::Pull::Ended) once drained), and then pushes
//! it if the queue has room. With no room (the audio thread is behind) it stays
//! pending until a later push: [`reclaim`](HeardTaps::reclaim), or another
//! open or close. Opening while a reader is alive supersedes it: the audio
//! thread swaps the writer, the old one is freed on the control side, and the
//! old reader ends after its drain.
//!
//! **When the first frame arrives.** The writes start at the first
//! [`process`](crate::Engine::process) call that begins after the install is
//! pushed, as a plan tap's start at its commit, and that call's whole block is
//! the first thing written: a call already in progress when the push ran is
//! not. A close stops the writes from the next call after its push on, the
//! same way.
//!
//! A block of another width than the tap's counts as lost, whole.

use std::sync::Weak;

use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, Interleaved};
use tutti_graph::{TapReader, TapWriter};
use tutti_rt::{Overflow, PushError, RingReader, RingWriter, ring};
use tutti_units::Samples;

/// Installs that can be queued for the audio thread at once.
const INSTALLS: usize = 2;

/// An install: a writer, or `None` to close.
type Install<F> = Option<Box<TapWriter<F>>>;

/// The control side of the engine's after-the-fader tap. The session owns it
/// ([`Engine::take_heard_taps`](crate::Engine::take_heard_taps)).
pub struct HeardTaps<F: Float = f32> {
    install: RingWriter<Install<F>>,
    retired: RingReader<Box<TapWriter<F>>>,
    /// The open tap's reader, to see when it was dropped.
    reader: Option<Weak<()>>,
    /// The install not yet in the queue: the latest open or close, waiting for
    /// room.
    pending: Option<Install<F>>,
    /// Where each tap opened takes its bell ([`HeardTaps::notify_with`]).
    bells: Option<tutti_rt::BellsHandle>,
}

impl<F: Float> std::fmt::Debug for HeardTaps<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeardTaps").finish_non_exhaustive()
    }
}

/// The audio thread's ends: installs in, retired writers out, the live one.
///
/// The rings hold boxed writers, so this is dropped with the engine on the
/// control side (`tutti_rt::ring`'s contract): an install still queued is
/// freed there.
pub(crate) struct Hearing<F: Float> {
    install: RingReader<Install<F>>,
    retire: RingWriter<Box<TapWriter<F>>>,
    writer: Option<Box<TapWriter<F>>>,
}

/// A linked pair, built with the engine.
pub(crate) fn link<F: Float>() -> (HeardTaps<F>, Hearing<F>) {
    let (install_tx, install_rx) = ring::<Install<F>>(INSTALLS, Overflow::Refuse);
    // A retired writer waits here for the control side; this holds every
    // install that can be in flight and the live one.
    let (retire_tx, retire_rx) = ring::<Box<TapWriter<F>>>(INSTALLS + 2, Overflow::Refuse);
    (
        HeardTaps {
            install: install_tx,
            retired: retire_rx,
            reader: None,
            pending: None,
            bells: None,
        },
        Hearing {
            install: install_rx,
            retire: retire_tx,
            writer: None,
        },
    )
}

impl<F: Float> HeardTaps<F> {
    /// Frees what the audio thread retired.
    fn free(&mut self) {
        while let Some(writer) = self.retired.pop() {
            writer.close();
        }
    }

    /// Makes `install` the pending one. A pending writer it replaces was never
    /// pushed, so it was never written: it is closed here, and its reader ends
    /// at once.
    fn replace_pending(&mut self, install: Install<F>) {
        if let Some(Some(old)) = self.pending.replace(install) {
            old.close();
        }
    }

    /// Moves the pending install into the queue when it has room; otherwise it
    /// stays pending.
    fn push_pending(&mut self) {
        self.free();
        let Some(install) = self.pending.take() else {
            return;
        };
        if let Err(refused) = self.install.push(install) {
            match (refused.error, refused.value) {
                (PushError::Full, install) => self.pending = Some(install),
                // The engine is gone and never takes the writer: it is closed,
                // so its reader ends.
                (PushError::Disconnected, Some(writer)) => writer.close(),
                (PushError::Disconnected, None) => {}
            }
        }
    }

    /// Makes every tap opened from now on carry a bell from `bells`: the audio
    /// thread rings it after each block it writes and when the writer drops, and
    /// the reader awaits it ([`TapReader::notified`]). Without it a tap carries
    /// none.
    pub fn notify_with(&mut self, bells: tutti_rt::BellsHandle) {
        self.bells = Some(bells);
    }

    /// Opens the tap: `width` channels (the device's, so every output channel
    /// is kept, no fold), each lane's ring `capacity` frames. What the reader
    /// does not drain in time is dropped and counted
    /// ([`TapReader::take_lost`]).
    ///
    /// It always opens and returns the reader at once. The writer becomes the
    /// one pending install, replacing a pending install not yet pushed, and
    /// supersedes the tap that is open: the old reader answers
    /// [`Pull::Ended`](tutti_buffers::Pull::Ended) once drained. The
    /// writes start at the first render that begins after the install is
    /// pushed, as a plan tap's start at its commit: the push is made by this
    /// call when the audio thread has room for it, else by a later
    /// [`reclaim`](Self::reclaim) (the session's update, and an offline
    /// render, make it).
    pub fn open(&mut self, width: ChannelLayout, capacity: Samples) -> TapReader<F> {
        let width = usize::from(width.count());
        let (reader, writer) = match self.bells.as_ref().map(tutti_rt::BellsHandle::bell) {
            Some(bell) => TapReader::pair_notified(width, capacity, bell),
            None => TapReader::pair(width, capacity),
        };
        self.reader = Some(reader.watch());
        self.replace_pending(Some(Box::new(writer)));
        self.push_pending();
        reader
    }

    /// Closes the tap: the writes stop from the first render that begins after
    /// the close is pushed, and the reader keeps what it was written, then
    /// answers [`Pull::Ended`](tutti_buffers::Pull::Ended). It
    /// replaces the pending install. With no tap open it does nothing.
    pub fn close(&mut self) {
        if self.reader.take().is_some() {
            self.replace_pending(None);
        }
        self.push_pending();
    }

    /// Frees the writers the audio thread retired, closes the tap if its
    /// reader was dropped, and pushes the pending install if the queue has
    /// room. A control-side tick (the session's `Reclaim` phase, and each
    /// offline render).
    pub fn reclaim(&mut self) {
        if self.reader.as_ref().is_some_and(|r| r.strong_count() == 0) {
            self.reader = None;
            self.replace_pending(None);
        }
        self.push_pending();
    }
}

impl<F: Float> Hearing<F> {
    /// Takes the installs that arrived, retiring the writers they replace.
    /// Audio thread: pops and pushes boxes, drops nothing, allocates nothing.
    #[inline]
    pub(crate) fn swap(&mut self) {
        while self.retire.vacant() > 0
            && let Some(install) = self.install.pop()
        {
            if let Some(old) = std::mem::replace(&mut self.writer, install)
                && let Err(old) = self.retire.push(old)
            {
                // Full cannot happen (the vacancy was checked above). The
                // retire ring's reader is the control side's `HeardTaps`: if
                // it was dropped, nothing will free the writer, and it is
                // leaked rather than freed on the audio thread.
                std::mem::forget(old);
            }
        }
    }

    /// Writes a chunk of the (faded) output. A chunk of another width than
    /// the tap's is counted lost (`TapWriter::push_interleaved`).
    #[inline]
    pub(crate) fn write(&mut self, block: Interleaved<'_, F>) {
        if let Some(writer) = self.writer.as_mut() {
            writer.push_interleaved(block);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(width: usize) -> Box<TapWriter<f32>> {
        Box::new(TapReader::pair(width, Samples::new(8)).1)
    }

    fn width(h: &Hearing<f32>) -> Option<usize> {
        h.writer.as_ref().map(|w| w.width())
    }

    /// With the retire queue full the audio thread keeps the writer it has and
    /// leaves the install queued: it never drops the old writer (a drop in the
    /// engine's `RtScope` panics in debug, and the writer would be gone in
    /// release, which the width check sees) and loses nothing. Once the control
    /// side frees the queue, the swap goes through and the old writer is sent
    /// back. Unreachable through the public calls (each drains the retire queue
    /// first), so the queues are filled by hand.
    #[test]
    fn a_full_retire_queue_holds_the_install_and_drops_nothing() {
        let (mut taps, mut h) = link();
        h.writer = Some(writer(2));
        while h.retire.vacant() > 0 {
            assert!(h.retire.push(writer(9)).is_ok());
        }
        let full = h.retire.len();
        taps.install.push(Some(writer(3))).expect("room");
        {
            let _rt = tutti_rt::RtScope::enter();
            h.swap();
        }
        assert_eq!(width(&h), Some(2), "the writer is kept");
        assert_eq!(h.install.len(), 1, "the install waits");
        assert_eq!(h.retire.len(), full, "nothing was sent or dropped");

        taps.reclaim();
        {
            let _rt = tutti_rt::RtScope::enter();
            h.swap();
        }
        assert_eq!(width(&h), Some(3), "the install went through");
        assert_eq!(h.install.len(), 0);
        assert_eq!(h.retire.len(), 1, "the old writer was sent back");
    }
}
