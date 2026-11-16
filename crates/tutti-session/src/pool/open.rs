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
    ids: &'a mut crate::peer::ResourceIds,
    #[cfg(feature = "midi")]
    midi: crate::Midi<'a>,
}

impl<'a> OpenCx<'a> {
    /// What an open needs of `session`, of whatever clock: its rate and, with
    /// `midi`, its MIDI view. Nothing else of the session is reachable.
    pub(crate) fn new<C: Clock>(session: &'a mut Session<C>) -> Self {
        let rate = session.transport.sample_rate();
        #[cfg(feature = "midi")]
        let (midi, ids) = session.midi_and_ids();
        #[cfg(not(feature = "midi"))]
        let ids = &mut session.resource_ids;
        Self {
            rate,
            ids,
            #[cfg(feature = "midi")]
            midi,
        }
    }

    /// The id of a new resource: no other token of this session has it. The
    /// peer a stage opens takes its id here.
    #[cfg_attr(
        not(feature = "midi"),
        expect(
            dead_code,
            reason = "the pool's MIDI sinks and ports (feature `midi`) mint their ids here"
        )
    )]
    pub(crate) fn mint_resource_id(&mut self) -> crate::ResourceId {
        self.ids.mint()
    }

    /// The graph's sample rate now.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The session's MIDI: its devices, inputs and outputs.
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
