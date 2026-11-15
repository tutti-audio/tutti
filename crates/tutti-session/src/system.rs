//! [`SystemSession`]: a session whose blocks an OS timer thread renders, for an
//! app that plays MIDI and owns no sound card.

use std::sync::Arc;

use tutti_engine::{OutputStream, StreamFaults};

use crate::Session;

/// A session and the timer thread that steps it, as
/// [`SessionBuilder::build`](crate::SessionBuilder::build) returns them after
/// [`system_clock`](crate::SessionBuilder::system_clock).
///
/// **The timer is the clock.** The graph renders one block per timer wake-up
/// into a buffer nobody plays, the transport runs, and the session's MIDI
/// leaves on the hardware out with the lead of one block. There is no audio in
/// or out. Dropping the `SystemSession` stops the thread; keep it for as long
/// as the session should run.
///
/// The session is reached through [`session`](Self::session) and
/// [`session_mut`](Self::session_mut): this type does not dereference to it.
/// It is `Send`, so an app may build it on one thread and run it on another:
///
/// ```
/// fn is_send<T: Send>() {}
/// is_send::<tutti_session::SystemSession>();
/// ```
///
/// A discarded build is a warning (an error under `deny(unused_must_use)`):
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// fn build() -> tutti_session::SystemSession { unimplemented!() }
/// fn f() {
///     build();
/// }
/// ```
#[must_use = "dropping a SystemSession stops the timer; keep it while the session should run"]
#[derive(Debug)]
pub struct SystemSession {
    session: Session,
    stream: OutputStream,
}

impl SystemSession {
    pub(crate) fn new(session: Session, stream: OutputStream) -> Self {
        Self { session, stream }
    }

    /// The session.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The session, mutably.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// The timer's fault record: one
    /// [`Overrun`](tutti_engine::StreamFaultKind::Overrun) each time the
    /// thread fell more than four blocks behind and began its schedule again.
    /// The same handle for the session's life.
    pub fn faults(&self) -> Arc<StreamFaults> {
        self.stream.faults()
    }
}
