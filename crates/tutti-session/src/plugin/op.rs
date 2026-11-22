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

use tutti_plugin::{Answer, PluginAnswers};
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::MainThreadMarker;

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

    /// Refuses the call before the plugin is asked: a plugin that cannot take it.
    #[doc(hidden)]
    fn admit(&self, _handle: &PluginHandle) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Asks a plugin in this process, on the main thread.
    #[doc(hidden)]
    fn run(
        &self,
        handle: &PluginHandle,
        main: MainThreadMarker,
    ) -> Result<Self::Reply, Self::Error>;

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
/// awaits on any thread. This enum has no `run`.
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
        let handle = Box::new(hosted.handle.clone());
        match &hosted.answers {
            None => Self::InProcess(InProcessOp {
                op,
                handle,
                main_thread: PhantomData,
            }),
            Some(answers) => Self::OutOfProcess(OutOfProcessOp {
                op,
                handle,
                answers: answers.clone(),
            }),
        }
    }
}

/// A call on a plugin that runs in this process.
///
/// `!Send`: the plugin is called on the main thread.
#[must_use = "work does nothing until it runs"]
pub struct InProcessOp<O: PluginOp> {
    op: O,
    handle: Box<PluginHandle>,
    main_thread: PhantomData<*const ()>,
}

impl<O: PluginOp + fmt::Debug> fmt::Debug for InProcessOp<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InProcessOp")
            .field("op", &self.op)
            .finish_non_exhaustive()
    }
}

impl<O: PluginOp> InProcessOp<O> {
    /// Calls the plugin, on the main thread. The session is not touched, and no
    /// `update` has to run.
    ///
    /// # Errors
    ///
    /// The call's lost error when the node was removed or the plugin replaced
    /// since the value was made (nothing is asked); otherwise the plugin's own.
    pub fn run(self, main: MainThreadMarker) -> Result<O::Output, O::Error> {
        if self.op.ended() {
            return Err(O::lost());
        }
        self.op.admit(&self.handle)?;
        let reply = self.op.run(&self.handle, main)?;
        Ok(self.op.finish(reply, None))
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
    handle: Box<PluginHandle>,
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
            handle,
            answers,
        } = self;
        AwaitingOp {
            stage: Stage::Unstarted(()),
            op: Some(op),
            handle,
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
    handle: Box<PluginHandle>,
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
        let (op, handle) = (&this.op, &this.handle);
        let started = this.stage.poll(
            cx,
            |()| {
                let op = op.as_ref().expect("the call waits for its first poll");
                // Checked when the request is made, so a plugin the node left
                // is never asked.
                if op.ended() {
                    return Err(O::lost());
                }
                op.admit(handle)?;
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
