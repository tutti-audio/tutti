//! What a session-thread stage is given.

use std::fmt;

#[cfg(not(feature = "midi"))]
use std::marker::PhantomData;

use tutti_buffers::Float;
use tutti_units::SampleRate;

use crate::{Clock, Session};

/// What [`PoolLoad::on_session`](crate::PoolLoad::on_session)'s stage may reach: the
/// parts of the session an open needs, and nothing of the pool.
///
/// The stage runs inside [`Session::update`](crate::Session::update), on the
/// thread that owns the session, so it may use what a worker cannot (a MIDI
/// device is `!Send`).
///
/// A panic in the stage is caught and fails the entry
/// ([`LoadError::Job`](crate::LoadError::Job)), but it unwinds through the
/// session's own state: a panic inside a call on `midi` can leave
/// the MIDI state half changed.
pub struct OpenCx<'a, S: Float = f32> {
    rate: SampleRate,
    #[cfg(feature = "midi")]
    midi: crate::Midi<'a, S>,
    #[cfg(not(feature = "midi"))]
    _float: PhantomData<(&'a (), fn() -> S)>,
}

impl<'a, S: Float> OpenCx<'a, S> {
    /// What an open needs of `session`, of whatever clock: its rate and, with
    /// `midi`, its MIDI view. Nothing else of the session is reachable.
    pub(crate) fn new<K: crate::AppKey, C: Clock<Float = S>>(
        session: &'a mut Session<K, C>,
    ) -> Self {
        let rate = session.transport.sample_rate();
        Self {
            rate,
            #[cfg(feature = "midi")]
            midi: session.midi(),
            #[cfg(not(feature = "midi"))]
            _float: PhantomData,
        }
    }

    /// The graph's sample rate now.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The session's MIDI: its devices, inputs and outputs.
    #[cfg(feature = "midi")]
    pub fn midi(&mut self) -> crate::Midi<'_, S> {
        self.midi.reborrow()
    }
}

impl<S: Float> fmt::Debug for OpenCx<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenCx").finish_non_exhaustive()
    }
}
