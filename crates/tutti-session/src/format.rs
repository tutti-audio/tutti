//! [`Sealed`]: the steps of a session that only the `f32` session has.
//!
//! A private module with a public trait, so [`SessionFloat`](crate::SessionFloat)
//! names it and no other crate can implement it.

#[cfg(feature = "device")]
use crate::DeviceClock;
use crate::{AppKey, Clock, PhaseCx, Session, SessionEvent};
use tutti_buffers::Float;

/// A point in a phase where an `f32` session has work an `f64` one does not.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// What the session's jobs and the pool landed, and the removals' leftovers.
    Landings,
    /// What an export left behind.
    Exports,
    /// Modulation applied.
    Modulation,
    /// The declarations re-applied after the commit.
    Reapply,
    /// The plugins served.
    Plugins,
    /// The render mode announced.
    RenderMode,
    /// The output devices' listing.
    Listing,
}

/// The part of a session that depends on its float format being `f32`: the
/// nodes the engine inserts and the phases that serve them.
pub trait Sealed: Float {
    /// The `f32`-only part of `step` of a phase: all of it on an `f32` session,
    /// none on an `f64` one.
    #[doc(hidden)]
    fn host<K: AppKey, C: Clock<Float = Self>>(
        session: &mut Session<K, C>,
        step: Step,
        cx: PhaseCx<'_>,
        out: &mut Vec<SessionEvent>,
    );

    /// Reopens the capture inputs after a device restart: the inputs are
    /// `f32` streams, so an `f32` session has them and an `f64` one has none.
    #[doc(hidden)]
    #[cfg(feature = "device")]
    fn reopen_inputs<K: AppKey>(session: &mut Session<K, DeviceClock<Self>>)
    where
        Self: crate::SessionFloat;
}

impl Sealed for f32 {
    fn host<K: AppKey, C: Clock<Float = f32>>(
        session: &mut Session<K, C>,
        step: Step,
        cx: PhaseCx<'_>,
        out: &mut Vec<SessionEvent>,
    ) {
        session.host_step(step, cx, out);
    }

    #[cfg(feature = "device")]
    fn reopen_inputs<K: AppKey>(session: &mut Session<K, DeviceClock<f32>>) {
        #[cfg(feature = "audio-io")]
        session.reopen_inputs();
        #[cfg(not(feature = "audio-io"))]
        let _ = session;
    }
}

impl Sealed for f64 {
    fn host<K: AppKey, C: Clock<Float = f64>>(
        _: &mut Session<K, C>,
        _: Step,
        _: PhaseCx<'_>,
        _: &mut Vec<SessionEvent>,
    ) {
    }

    #[cfg(feature = "device")]
    fn reopen_inputs<K: AppKey>(_: &mut Session<K, DeviceClock<f64>>) {}
}
