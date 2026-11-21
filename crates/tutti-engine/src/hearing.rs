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
//! [`RtScope`](tutti_rt::RtScope), which the engine enters). The control ends
//! of both queues sit behind a `Mutex` that only control threads take.
//!
//! **When the first frame arrives.** An install is seen at the start of the
//! next [`process`](crate::Engine::process) call that begins after
//! [`open`](OutputTaps::open) returns, and that call's whole block is the first
//! thing written: a call already in progress when `open` ran is not. A
//! [`close`](OutputTaps::close) stops the writes from the next call on, the
//! same way. Opens and closes are applied in the order they took the lock, so
//! racing ones leave the state the last of them asked for.
//!
//! A block of another width than the tap's counts as lost, whole.

use std::sync::{Arc, Mutex, Weak};

use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, Interleaved};
use tutti_graph::{TapReader, TapWriter};
use tutti_rt::{LockExt, Overflow, PushError, RingReader, RingWriter, ring};
use tutti_units::Samples;

/// Opens that can be in flight at once. The install queue holds one more: a
/// slot kept for a close, so closing a tap never fails for want of room while
/// opens fill the queue (the audio thread behind).
const INSTALLS: usize = 4;

/// An install: a writer, or `None` to close.
type Install<F> = Option<Box<TapWriter<F>>>;

/// Why [`OutputTaps::open`] or [`OutputTaps::close`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum OutputTapError {
    /// A reader of the engine's tap is alive. Close it first (or drop it:
    /// the session's reclaim phase closes a tap whose reader is gone).
    #[error("the heard tap already has a reader; close it before opening again")]
    Busy,
    /// [`OutputTaps::close`] found no tap open (none was opened, or it was
    /// closed, or its dropped reader was reclaimed).
    #[error("no heard tap is open")]
    NotOpen,
}

/// What [`OutputTaps::open`] did: opened the tap, or found the audio thread
/// behind. Not a [`OutputTapError`]: that says a call was refused for good (a
/// reader is alive, nothing is open), and this one is a moment that passes.
#[derive(Debug)]
#[must_use = "a Retry opened nothing"]
pub enum OutputTapOpen<F: Float = f32> {
    /// The tap is open: this reads it. It flows from the first render that
    /// begins after the call returned.
    Opened(TapReader<F>),
    /// The audio thread has not yet taken the last installs (it is not
    /// rendering). Nothing was opened or changed; call again once it has.
    Retry,
}

impl<F: Float> OutputTapOpen<F> {
    /// The reader, or `None` for a [`Retry`](Self::Retry).
    pub fn reader(self) -> Option<TapReader<F>> {
        match self {
            Self::Opened(reader) => Some(reader),
            Self::Retry => None,
        }
    }
}

/// The control side of the engine's after-the-fader tap: cheap to clone,
/// usable from any control thread. Get it from
/// [`Engine::output_taps`](crate::Engine::output_taps).
#[derive(Clone, Debug)]
pub struct OutputTaps<F: Float = f32>(Arc<Mutex<Control<F>>>);

struct Control<F: Float> {
    install: RingWriter<Install<F>>,
    retired: RingReader<Box<TapWriter<F>>>,
    /// The open tap's reader, to see when it was dropped.
    reader: Option<Weak<()>>,
    /// A close the install queue had no room for, owed to the audio thread:
    /// sent on the next open, close or reclaim, whichever comes first.
    owed_close: bool,
    /// Where each tap opened takes its bell ([`OutputTaps::notify_with`]).
    bells: Option<tutti_rt::BellsHandle>,
}

impl<F: Float> std::fmt::Debug for Control<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

impl<F: Float> Control<F> {
    /// Frees what the audio thread retired.
    fn free(&mut self) {
        while self.retired.pop().is_some() {}
    }

    fn alive(&self) -> bool {
        self.reader.as_ref().is_some_and(|r| r.strong_count() > 0)
    }

    /// Sends a close the audio thread must see: now if the queue has room,
    /// else it stays owed and the next control call sends it. The old writer
    /// keeps feeding its reader (which no longer exists for the caller) until
    /// then.
    fn send_close(&mut self) {
        self.owed_close = self.install.push(None).is_err();
    }

    /// Settles what is owed before anything new is queued behind it.
    fn settle(&mut self) {
        self.free();
        if self.owed_close {
            self.send_close();
        }
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
pub(crate) fn link<F: Float>() -> (OutputTaps<F>, Hearing<F>) {
    let (install_tx, install_rx) = ring::<Install<F>>(INSTALLS + 1, Overflow::Refuse);
    // A retired writer waits here for the control side; this holds every
    // install that can be in flight and the live one.
    let (retire_tx, retire_rx) = ring::<Box<TapWriter<F>>>(INSTALLS + 3, Overflow::Refuse);
    (
        OutputTaps(Arc::new(Mutex::new(Control {
            install: install_tx,
            retired: retire_rx,
            reader: None,
            owed_close: false,
            bells: None,
        }))),
        Hearing {
            install: install_rx,
            retire: retire_tx,
            writer: None,
        },
    )
}

impl<F: Float> OutputTaps<F> {
    fn control(&self) -> std::sync::MutexGuard<'_, Control<F>> {
        self.0.lock_recover()
    }

    /// Makes every tap opened from now on carry a bell from `bells`: the audio
    /// thread rings it after each block it writes and when the writer drops, and
    /// the reader awaits it ([`TapReader::notified`]). Without it a tap carries
    /// none.
    pub fn notify_with(&self, bells: tutti_rt::BellsHandle) {
        self.control().bells = Some(bells);
    }

    /// Opens the tap: `width` channels (the device's, so every output channel
    /// is kept, no fold), each lane's ring `capacity` frames. What the reader
    /// does not drain in time is dropped and counted
    /// ([`TapReader::take_lost`]). Flows from the first call of the engine's
    /// render that begins after this returns.
    ///
    /// Returns [`OutputTapOpen::Retry`], opening nothing, while the audio thread
    /// has not taken the last installs.
    ///
    /// # Errors
    ///
    /// [`OutputTapError::Busy`] while another reader of the tap is alive.
    pub fn open(
        &self,
        width: ChannelLayout,
        capacity: Samples,
    ) -> Result<OutputTapOpen<F>, OutputTapError> {
        let mut c = self.control();
        c.settle();
        if c.alive() {
            return Err(OutputTapError::Busy);
        }
        // One slot stays free for a close.
        if c.owed_close || c.install.vacant() <= 1 {
            return Ok(OutputTapOpen::Retry);
        }
        let width = usize::from(width.count());
        let (reader, writer) = match c.bells.as_ref().map(tutti_rt::BellsHandle::bell) {
            Some(bell) => TapReader::pair_notified(width, capacity, bell),
            None => TapReader::pair(width, capacity),
        };
        // Full cannot happen (the vacancy was checked). With the engine gone the
        // writer is dropped here and the reader stays open, as one opened just
        // before the engine went would be.
        if let Err(refused) = c.install.push(Some(Box::new(writer))) {
            debug_assert_eq!(
                refused.error,
                PushError::Disconnected,
                "the queue has a free slot"
            );
        }
        c.reader = Some(reader.watch());
        Ok(OutputTapOpen::Opened(reader))
    }

    /// Closes the tap from the next call of the engine's render on. The
    /// reader keeps what it was written.
    ///
    /// # Errors
    ///
    /// [`OutputTapError::NotOpen`] when no tap is open; nothing is sent.
    pub fn close(&self) -> Result<(), OutputTapError> {
        let mut c = self.control();
        c.settle();
        if c.reader.take().is_none() {
            return Err(OutputTapError::NotOpen);
        }
        // A full queue means the audio thread is behind: the close stays owed
        // and goes out on the next open, close or [`reclaim`](Self::reclaim).
        c.send_close();
        Ok(())
    }

    /// Frees the writers the audio thread retired, and closes the tap if its
    /// reader was dropped. A control-side tick (the session's `Reclaim`
    /// phase).
    pub fn reclaim(&self) {
        let mut c = self.control();
        c.settle();
        if c.reader.is_some() && !c.alive() {
            c.reader = None;
            c.send_close();
        }
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
                // retire ring's reader is the control side's `OutputTaps`: if
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
        let (taps, mut h) = link();
        h.writer = Some(writer(2));
        while h.retire.vacant() > 0 {
            assert!(h.retire.push(writer(9)).is_ok());
        }
        let full = h.retire.len();
        taps.control().install.push(Some(writer(3))).expect("room");
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
