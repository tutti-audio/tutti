//! The one shape of a call on a hosted plugin: [`Op`] is the call ([`PluginOp`])
//! by where the plugin runs, an [`InProcessOp`] the app runs on the main thread
//! or an [`OutOfProcessOp`] it awaits on any thread. [`SaveState`],
//! [`RestoreState`] and [`LoadPreset`] are the three calls.
//!
//! [`SaveState`]: crate::SaveState
//! [`RestoreState`]: crate::RestoreState
//! [`LoadPreset`]: crate::LoadPreset

use core::marker::PhantomData;
use std::fmt;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use tutti_platform::Instant;
use tutti_plugin::{Answer, PluginAnswers};
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{MainThreadMarker, PresetSupport};

use super::HostedPlugin;
use super::awaiting::Stage;

mod sealed {
    /// Keeps [`PluginOp`](super::PluginOp) to the session's own calls.
    pub trait Sealed {}
}

pub(super) use sealed::Sealed;

/// A call on a hosted plugin, which an [`Op`] runs in process or awaits out of
/// process. The session's own calls implement it ([`SaveState`](crate::SaveState),
/// [`RestoreState`](crate::RestoreState), [`LoadPreset`](crate::LoadPreset)):
/// the trait is sealed.
pub trait PluginOp: sealed::Sealed + Sized {
    /// What the plugin answers.
    type Reply;
    /// What the call yields once the plugin has answered.
    type Output;
    /// Why the call failed.
    type Error;

    /// Whether the plugin instance the call was cut from is gone.
    #[doc(hidden)]
    fn ended(&self) -> bool;

    /// The error of a call whose plugin instance is gone.
    #[doc(hidden)]
    fn lost() -> Self::Error;

    /// The error of a call whose plugin's bridge ended or whose server died
    /// before it answered.
    #[doc(hidden)]
    fn crashed() -> Self::Error;

    /// Refuses the call before the plugin is asked: a plugin whose preset
    /// `support` cannot take it.
    #[doc(hidden)]
    fn admit(&self, _support: PresetSupport) -> Result<(), Self::Error> {
        Ok(())
    }

    /// The error of a call whose plugin did not answer within `after`.
    #[doc(hidden)]
    fn stalled(after: Duration) -> Self::Error;

    /// Asks a plugin in this process, on the main thread. `Pending` while a
    /// node holds the claim a load needs, so no drain straddles it.
    #[doc(hidden)]
    fn run(
        &self,
        handle: &PluginHandle,
        main: MainThreadMarker,
    ) -> Poll<Result<Self::Reply, Self::Error>>;

    /// Queues the call with the bridge of a plugin in a `plugin-server` process.
    #[doc(hidden)]
    fn send(&self, answers: &PluginAnswers) -> Answer<Result<Self::Reply, Self::Error>>;

    /// The output of the call once the plugin answered; `answers` is the
    /// plugin's bridge when it runs out of process.
    #[doc(hidden)]
    fn finish(self, reply: Self::Reply, answers: Option<&PluginAnswers>) -> Self::Output;
}

/// A call on a plugin ([`PluginOp`]), by where the plugin runs: an
/// [`InProcessOp`] the app runs on the main thread, or an [`OutOfProcessOp`] it
/// awaits on any thread. This enum has no `poll`.
#[must_use = "work does nothing until it runs"]
#[derive(Debug)]
pub enum Op<O: PluginOp> {
    /// The plugin runs in this process.
    InProcess(InProcessOp<O>),
    /// The plugin runs in a `plugin-server` process.
    OutOfProcess(OutOfProcessOp<O>),
}

impl<O: PluginOp> Op<O> {
    /// The call `op` on the plugin `hosted` holds, by where it runs.
    pub(super) fn at(hosted: &HostedPlugin, op: O) -> Self {
        let support = hosted.handle.preset_support();
        match &hosted.answers {
            None => Self::InProcess(InProcessOp {
                op: Some(op),
                handle: Box::new(hosted.handle.clone()),
                support,
                started: None,
                main_thread: PhantomData,
            }),
            Some(answers) => Self::OutOfProcess(OutOfProcessOp {
                op,
                support,
                answers: answers.clone(),
            }),
        }
    }
}

/// A call on a plugin that runs in this process, a polled value: the app asks
/// it again, on the main thread, until it is `Ready`.
///
/// `!Send`: the plugin is called on the main thread. A save never waits for a
/// block: it states the parameter writes no block has applied beside its
/// result ([`Saved`](crate::Saved)). A load is `Pending` only while a plugin
/// node holds the claim it needs, for the length of one drain. It resolves to
/// [`stalled`](PluginOp::stalled) once [`deadline`](Self::deadline) has passed:
/// the deadline guards a hang, and nothing is waited for.
#[must_use = "work does nothing until it is polled"]
pub struct InProcessOp<O: PluginOp> {
    op: Option<O>,
    handle: Box<PluginHandle>,
    support: PresetSupport,
    /// When the first poll was made.
    started: Option<Instant>,
    main_thread: PhantomData<*const ()>,
}

impl<O: PluginOp + fmt::Debug> fmt::Debug for InProcessOp<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InProcessOp")
            .field("op", &self.op)
            .finish_non_exhaustive()
    }
}

/// How long an in-process call may stay `Pending`: a hang guard, as a node
/// holds the writes' reader for the length of one drain.
const CLAIM_LIMIT: Duration = Duration::from_secs(2);

impl<O: PluginOp> InProcessOp<O> {
    /// Calls the plugin on the main thread at `now`, unless a load finds a node
    /// draining the host's parameter writes: then `Pending`, and the app polls
    /// again. The session is not touched, and no `update` has to run.
    ///
    /// # Errors
    ///
    /// The call's lost error when the node was removed or the plugin replaced
    /// since the value was made (nothing is asked); the stalled error when it
    /// is still `Pending` at [`deadline`](Self::deadline); otherwise the
    /// plugin's own.
    ///
    /// # Panics
    ///
    /// When polled after it resolved.
    pub fn poll(
        &mut self,
        main: MainThreadMarker,
        now: Instant,
    ) -> Poll<Result<O::Output, O::Error>> {
        let op = self.op.as_ref().expect("a call resolves once");
        if op.ended() {
            self.op = None;
            return Poll::Ready(Err(O::lost()));
        }
        if let Err(error) = op.admit(self.support) {
            self.op = None;
            return Poll::Ready(Err(error));
        }
        let started = *self.started.get_or_insert(now);
        match op.run(&self.handle, main) {
            Poll::Ready(reply) => {
                let op = self.op.take().expect("a call resolves once");
                Poll::Ready(reply.map(|reply| op.finish(reply, None)))
            }
            Poll::Pending if now >= started + CLAIM_LIMIT => {
                self.op = None;
                Poll::Ready(Err(O::stalled(CLAIM_LIMIT)))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    /// When a call polled so far gives up: `None` before the first poll.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.started.map(|started| started + CLAIM_LIMIT)
    }
}

/// A call on a plugin that runs in a `plugin-server` process. It is `Send`, and
/// it is a [`Future`] through [`IntoFuture`]: awaiting it queues the call with
/// the plugin's bridge and yields its output. Nothing of it needs the main
/// thread, so any thread awaits it. There is no blocking form.
///
/// Dropping it unawaited asks nothing.
#[must_use = "work does nothing until it is awaited"]
pub struct OutOfProcessOp<O: PluginOp> {
    op: O,
    support: PresetSupport,
    answers: PluginAnswers,
}

impl<O: PluginOp + fmt::Debug> fmt::Debug for OutOfProcessOp<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutOfProcessOp")
            .field("op", &self.op)
            .finish_non_exhaustive()
    }
}

impl<O: PluginOp> IntoFuture for OutOfProcessOp<O> {
    type Output = Result<O::Output, O::Error>;
    type IntoFuture = AwaitingOp<O>;

    fn into_future(self) -> AwaitingOp<O> {
        let Self {
            op,
            support,
            answers,
        } = self;
        AwaitingOp {
            stage: Stage::Unstarted(()),
            op: Some(op),
            support,
            answers,
        }
    }
}

/// An out-of-process call being awaited ([`OutOfProcessOp`]): resolves to the
/// call's output once the server has answered.
///
/// The first poll queues the call with the plugin's bridge and awaits the
/// receiver of a oneshot. **The bridge's IPC reader thread fills the sender**
/// when the server's reply lands, which wakes the future: a poll never blocks,
/// and `Session::update` is not needed. **The bridge's control lane thread fires
/// the deadline**: a server that never answers is cut off there, at the bridge's
/// timeout, and the lane sends its stalled error into the sender with no one
/// polling.
///
/// Dropped before it resolves, the future drops the receiver. A call already
/// queued is not recalled: **the server may still apply it**, and no output
/// exists for it.
///
/// # Panics
///
/// When polled after it resolved.
#[must_use = "a call does nothing unless it is awaited"]
pub struct AwaitingOp<O: PluginOp> {
    stage: Stage<(), O::Reply, O::Error>,
    op: Option<O>,
    support: PresetSupport,
    answers: PluginAnswers,
}

impl<O: PluginOp> fmt::Debug for AwaitingOp<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwaitingOp").finish_non_exhaustive()
    }
}

// The call is never pinned: a poll moves nothing out of a pinned place.
impl<O: PluginOp> Unpin for AwaitingOp<O> {}

impl<O: PluginOp> Future for AwaitingOp<O> {
    type Output = Result<O::Output, O::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let (op, support) = (&this.op, this.support);
        let started = this.stage.poll(
            cx,
            |()| {
                let op = op.as_ref().expect("the call waits for its first poll");
                // Checked when the request is made, so a plugin the node left
                // is never asked.
                if op.ended() {
                    return Err(O::lost());
                }
                op.admit(support)?;
                Ok(op.send(&this.answers))
            },
            O::crashed(),
        );
        started.map_ok(|reply| {
            let op = this.op.take().expect("a call resolves once");
            op.finish(reply, Some(&this.answers))
        })
    }
}
