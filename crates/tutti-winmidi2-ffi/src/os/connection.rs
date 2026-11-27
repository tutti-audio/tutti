//! A connection to one endpoint: sends and receives UMP words.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use tutti_platform::Instant;
use tutti_ump::Ump;
use windows_core::{HSTRING, Ref};

use super::{Clock, Error};
use crate::bindings::Windows::Devices::Midi2::{
    IMidiMessageReceivedEventSource, MidiEndpointConnection, MidiMessageReceivedEventArgs,
    MidiSendMessageResults, MidiSession,
};
use crate::bindings::Windows::Foundation::TypedEventHandler;

/// An open connection to an endpoint.
///
/// Dropping it closes it: its message handler is removed and the session disconnects it.
/// A handler call already running finishes, and the handler is released after it.
#[derive(Debug)]
pub struct Connection {
    /// The session the connection was made on, which disconnects it.
    session: MidiSession,
    connection: MidiEndpointConnection,
    clock: Clock,
    /// The token of the registered message handler.
    token: i64,
    closed: AtomicBool,
}

impl Connection {
    /// Creates the connection, registers the handler, then opens it: the service sends
    /// nothing before the open.
    pub(super) fn open<F>(
        session: &MidiSession,
        clock: Clock,
        endpoint_id: &str,
        on_message: F,
    ) -> Result<Self, Error>
    where
        F: FnMut(Option<Instant>, Ump) + Send + 'static,
    {
        let connection = session
            .CreateEndpointConnection(&HSTRING::from(endpoint_id))
            .ok_or(Error::Refused {
                call: "CreateEndpointConnection",
            })?;
        let handler = Mutex::new(on_message);
        let received = TypedEventHandler::<
            IMidiMessageReceivedEventSource,
            MidiMessageReceivedEventArgs,
        >::new(move |_, args: Ref<MidiMessageReceivedEventArgs>| {
            if let Some(args) = args.as_ref() {
                // A panic must not unwind into the service; the payload is forgotten, since
                // dropping it here would run arbitrary code on the service's thread.
                if let Err(payload) =
                    catch_unwind(AssertUnwindSafe(|| deliver(&handler, clock, args)))
                {
                    std::mem::forget(payload);
                }
            }
            Ok(())
        });
        let token = match connection.MessageReceived(&received) {
            Ok(token) => token,
            Err(e) => {
                session.DisconnectEndpointConnection(connection.ConnectionId());
                return Err(super::error::call("MessageReceived")(e));
            }
        };
        // From here a drop closes it, so a failed open undoes the registration.
        let this = Self {
            session: session.clone(),
            connection,
            clock,
            token,
            closed: AtomicBool::new(false),
        };
        if this.connection.Open() {
            Ok(this)
        } else {
            Err(Error::Refused { call: "Open" })
        }
    }

    /// Sends `ump` at `at`, or now for `None`. The service holds a message with a time
    /// ahead of its clock until then.
    ///
    /// # Errors
    ///
    /// `Error::Closed` for a closed connection, `Error::Timestamp` for an `at` that is not
    /// a timestamp, `Error::BufferFull` when the service cannot take the message now and
    /// `Error::MessageRefused` for any other refusal.
    pub fn send(&self, at: Option<Instant>, ump: &Ump) -> Result<(), Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let timestamp = self.clock.timestamp(at)?;
        let words = ump.words();
        // A UMP holds one to four words; a count the service refuses answers as a refusal.
        let count = u8::try_from(words.len()).unwrap_or(u8::MAX);
        let result = self
            .connection
            .SendSingleMessageWordArray(timestamp, 0, count, words);
        if result.contains(MidiSendMessageResults::Succeeded) {
            Ok(())
        } else if result.contains(MidiSendMessageResults::EndpointConnectionClosedOrInvalid) {
            Err(Error::Closed)
        } else if result.contains(MidiSendMessageResults::BufferFull) {
            Err(Error::BufferFull)
        } else if result.contains(MidiSendMessageResults::TimestampOutOfRange) {
            Err(Error::Timestamp)
        } else {
            Err(Error::MessageRefused { bits: result.0 })
        }
    }

    /// Closes the connection: its handler is removed and the session disconnects it. A
    /// second close does nothing, and a closed connection refuses a send.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // A failed removal leaves the handler registered until the connection is released;
        // it then still receives nothing once the session disconnects.
        let _ = self.connection.RemoveMessageReceived(self.token);
        self.session
            .DisconnectEndpointConnection(self.connection.ConnectionId());
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

/// Hands the handler one received message.
fn deliver<F: FnMut(Option<Instant>, Ump)>(
    handler: &Mutex<F>,
    clock: Clock,
    args: &MidiMessageReceivedEventArgs,
) {
    let mut words = [0u32; 4];
    let count = usize::from(args.FillWordArray(0, &mut words));
    // A count that is not a whole message is dropped: the service sends whole messages.
    let Some(ump) = words.get(..count).and_then(Ump::from_words) else {
        return;
    };
    let at = clock.instant(args.Timestamp());
    let mut handler = handler.lock().unwrap_or_else(PoisonError::into_inner);
    (*handler)(at, ump);
}
