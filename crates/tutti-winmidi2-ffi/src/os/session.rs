//! A session with the service.

use tutti_platform::Instant;
use tutti_ump::Ump;
use windows_core::HSTRING;

use super::call::{abi, call, statics};
use super::connection::Connection;
use super::{Clock, Error};
use crate::bindings::Windows::Devices::Midi2::{IMidiSessionStatics, MidiSession};

/// A session: the app's identity with the service, which its endpoint connections belong to.
///
/// Dropping it closes it, and with it every connection made from it.
#[derive(Debug)]
pub struct Session {
    pub(super) session: MidiSession,
    pub(super) clock: Clock,
    closed: bool,
}

impl Session {
    /// Creates a session named `name` with the service.
    ///
    /// # Errors
    ///
    /// `Error::ApiMissing` when the API is not present, `Error::Call` when a read fails,
    /// `Error::Refused` when the service creates no session (it is not running) and
    /// `Error::Timestamp` for a clock that reports no frequency.
    pub fn create(name: &str) -> Result<Self, Error> {
        let clock = Clock::new()?;
        let statics = statics::<MidiSession, IMidiSessionStatics>()?;
        let name = HSTRING::from(name);
        let session =
            call!("MidiSession.Create", statics, Create(abi(&name)) -> Option<MidiSession>)?
                .ok_or(Error::Refused {
                    call: "MidiSession.Create",
                })?;
        Ok(Self {
            session,
            clock,
            closed: false,
        })
    }

    /// Opens a connection to the endpoint with device id `endpoint_id` (an `Endpoint::id`).
    /// `on_message` receives each message the endpoint sends: its time on the service's
    /// clock as an `Instant`, `None` where the timestamp is not one, and the message. A
    /// message the service delivered that cannot be read is the `Err` it receives in its
    /// place (`Error::Call` for a failed read, `Error::NotAMessage` for a word count that is
    /// not a whole message), so a caller counts each drop. It runs on a thread of the
    /// service, one message at a time, and a panic in it is caught and dropped. `on_message` is dropped on whichever thread releases the service's
    /// handler last, which is a thread of the service when the connection closes while a
    /// message is delivered.
    ///
    /// # Errors
    ///
    /// `Error::Refused` when the service creates no connection (no such endpoint) or does
    /// not open it, and `Error::Call` when the handler's registration fails.
    pub fn connect<F>(&self, endpoint_id: &str, on_message: F) -> Result<Connection, Error>
    where
        F: FnMut(Result<(Option<Instant>, Ump), Error>) + Send + 'static,
    {
        Connection::open(&self.session, self.clock, endpoint_id, None, on_message)
    }

    /// Closes the session and its connections.
    ///
    /// # Errors
    ///
    /// `Error::Call` when the close fails; the session is closed as far as it can be.
    pub fn close(mut self) -> Result<(), Error> {
        self.closed = true;
        self.session
            .Close()
            .map_err(super::error::call("MidiSession.Close"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.closed {
            // A failed close leaves nothing to do: the session is released next.
            let _ = self.session.Close();
        }
    }
}
