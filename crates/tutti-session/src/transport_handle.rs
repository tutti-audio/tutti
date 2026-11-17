//! [`TransportHandle`]: the transport's verbs from any thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tutti_engine::BlockClockReader;
use tutti_transport::{FadeOut, LoopRegion, MotionEvent, Then, TimedError, Transport};
use tutti_types::{At, Beat};

use crate::Playhead;

/// A session's transport verbs, owned by no thread: `play`, `stop`, `locate`
/// and `schedule` as [`Session`](crate::Session) has them, from a handle that
/// is `Send` and `Clone`.
///
/// Every clone sends into the session's one motion queue, and every clone
/// shares the control side's record of whether the last motion sent left the
/// transport rolling, so [`TimedError::BeatWhileStopped`] is refused on any
/// thread as on the session. With senders on several threads at once, the
/// record follows the order their sends updated it, which may differ from the
/// order the queue holds them. Take one with
/// [`Session::transport_handle`](crate::Session::transport_handle).
///
/// A handle holds no borrow of its session: once the session is dropped, its
/// sends go into a queue nothing drains, and succeed until that queue is full.
#[derive(Clone, Debug)]
pub struct TransportHandle {
    transport: Transport,
    /// The engine's block clock; `None` detached, where nothing renders.
    clock: Option<BlockClockReader>,
    /// Whether the last motion sent for the next block left the transport
    /// rolling (a play, a rolling locate) or stopped. The control side's own
    /// word, which the playhead (one block old) cannot give.
    rolling_sent: Arc<AtomicBool>,
}

impl TransportHandle {
    pub(crate) fn new(transport: Transport) -> Self {
        Self {
            transport,
            clock: None,
            rolling_sent: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn set_clock(&mut self, clock: BlockClockReader) {
        self.clock = Some(clock);
    }

    /// The playhead as the engine's last block wrote it; the default while no
    /// engine renders.
    pub(crate) fn playhead(&self) -> Playhead {
        self.clock
            .as_ref()
            .map_or_else(Playhead::default, BlockClockReader::playhead)
    }

    /// Starts the transport rolling from where it stands, at the next block.
    /// A no-op while rolling.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`] when the transport's command queue is full; the
    /// event is handed back in it.
    pub fn play(&self) -> Result<(), TimedError<MotionEvent>> {
        self.transport.motion().try_send(MotionEvent::Play)?;
        self.rolling_sent.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Stops the transport where it stands, at the next block, after the
    /// [`FadeOut`] it names.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`], as [`play`](Self::play).
    pub fn stop(&self, fade: FadeOut) -> Result<(), TimedError<MotionEvent>> {
        self.transport
            .motion()
            .try_send(MotionEvent::Stop { fade })?;
        self.rolling_sent.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Jumps to `beat` at the next block, keeping whatever the transport was
    /// doing (a locate while rolling keeps rolling), with a declick fade.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`], as [`play`](Self::play).
    pub fn locate(&self, beat: Beat) -> Result<(), TimedError<MotionEvent>> {
        self.transport
            .motion()
            .try_send(MotionEvent::locate(beat))?;
        Ok(())
    }

    /// Sends a motion `event` to take effect at `at`: a frame, a beat, or the
    /// next block (`At`, or a `Beat` or `Frame` as it stands). The one timed
    /// verb: `schedule(b, MotionEvent::Play)` is play at beat `b`, and every
    /// [`MotionEvent`] (a locate that then rolls, a stop that returns to a
    /// beat) is timed the same way. The untimed verbs are `At::NextBlock`.
    ///
    /// A beat is reached by playback: it has a frame only while the transport
    /// rolls. A command that starts the transport rolling ([`MotionEvent::Play`],
    /// or a locate that then rolls) timed at a beat while the transport is
    /// stopped could only fire after the playback it starts, so it is refused
    /// where it is made. Stopped is the control side's word: the last motion
    /// sent through the session or any handle (a `play` just before is
    /// rolling), not the playhead, which is a block old. A command that does
    /// not start playback (a stop, a plain locate) waits for the playback to
    /// reach its beat, as any beat command does.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`] when the timed-command list is full, and
    /// [`TimedError::BeatWhileStopped`] for the command above.
    pub fn schedule(
        &self,
        at: impl Into<At>,
        event: MotionEvent,
    ) -> Result<(), TimedError<MotionEvent>> {
        let at = at.into();
        let starts = matches!(
            event,
            MotionEvent::Play
                | MotionEvent::Locate {
                    then: Then::Roll,
                    ..
                }
        );
        // Rolling as far as the control side knows: the last motion sent for
        // the next block, or what the engine last rendered (a transport
        // started through `Session::transport` directly).
        let rolling = self.rolling_sent.load(Ordering::Relaxed) || self.playhead().playing();
        if let At::Beat(_) = at
            && starts
            && !rolling
        {
            return Err(TimedError::BeatWhileStopped(event));
        }
        self.transport.motion().schedule(at, event)?;
        if at == At::NextBlock {
            match event {
                MotionEvent::Play
                | MotionEvent::Locate {
                    then: Then::Roll, ..
                } => self.rolling_sent.store(true, Ordering::Relaxed),
                MotionEvent::Stop { .. } => self.rolling_sent.store(false, Ordering::Relaxed),
                _ => {}
            }
        }
        Ok(())
    }

    /// The loop region the engine last published: its bounds and whether it is
    /// armed.
    #[must_use]
    pub fn loop_region(&self) -> LoopRegion {
        self.transport.motion().loop_region()
    }

    /// Drops every timed command not yet fired, from the next block. The
    /// rolling record is unchanged: a cancelled play leaves it as it was sent.
    pub fn cancel_scheduled(&self) {
        self.transport.motion().cancel_scheduled();
    }

    /// How many timed commands are waiting to fire.
    #[must_use]
    pub fn scheduled_outstanding(&self) -> usize {
        self.transport.motion().scheduled_outstanding()
    }
}
