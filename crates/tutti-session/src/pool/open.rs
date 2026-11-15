//! What a session-thread stage is given.

use std::fmt;

use tutti_types::SampleRate;

use crate::{Clock, Session};

/// What [`Load::on_session`](crate::Load::on_session)'s stage may reach: the
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
pub struct OpenCx<'a> {
    rate: SampleRate,
    #[cfg(feature = "midi")]
    midi: crate::Midi<'a>,
    _session: std::marker::PhantomData<&'a mut ()>,
}

impl<'a> OpenCx<'a> {
    /// What an open needs of `session`, of whatever clock: its rate and, with
    /// `midi`, its MIDI view. Nothing else of the session is reachable.
    pub(crate) fn new<C: Clock>(session: &'a mut Session<C>) -> Self {
        Self {
            rate: session.transport.sample_rate(),
            #[cfg(feature = "midi")]
            midi: session.midi(),
            _session: std::marker::PhantomData,
        }
    }

    /// The graph's sample rate now.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The session's MIDI: its devices, inputs and the hardware out.
    #[cfg(feature = "midi")]
    pub fn midi(&mut self) -> crate::Midi<'_> {
        self.midi.reborrow()
    }
}

impl fmt::Debug for OpenCx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenCx").finish_non_exhaustive()
    }
}
