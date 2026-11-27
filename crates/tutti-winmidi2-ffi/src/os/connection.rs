//! A connection to one endpoint: sends and receives UMP words.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use tutti_platform::Instant;
use tutti_ump::Ump;
use windows_core::{HSTRING, Ref};

use super::call::{abi, call};
use super::{Clock, Error};
use crate::bindings::Windows::Devices::Midi2::IMidiEndpointConnectionSource;
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
    /// The connection's id, which the session disconnects it by.
    id: windows_core::GUID,
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
        let endpoint_id = HSTRING::from(endpoint_id);
        let connection = call!(
            "CreateEndpointConnection",
            *session,
            CreateEndpointConnection(abi(&endpoint_id)) -> Option<MidiEndpointConnection>
        )?
        .ok_or(Error::Refused {
            call: "CreateEndpointConnection",
        })?;
        // The id is read once, so that closing never has a call to fail.
        let source: IMidiEndpointConnectionSource = windows_core::Interface::cast(&connection)
            .map_err(super::error::call("QueryInterface"))?;
        let id = call!("ConnectionId", source, ConnectionId() -> windows_core::GUID)?;
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
                // Best effort: the registration's error is the one to report.
                let _ = call!(
                    "DisconnectEndpointConnection",
                    *session,
                    DisconnectEndpointConnection(id)
                );
                return Err(super::error::call("MessageReceived")(e));
            }
        };
        // From here a drop closes it, so a failed open undoes the registration.
        let this = Self {
            session: session.clone(),
            connection,
            clock,
            id,
            token,
            closed: AtomicBool::new(false),
        };
        // A failure returns with `this` dropped, which undoes the registration.
        if call!("Open", this.connection, Open() -> bool)? {
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
        // `Ump::words` is one to four words, so the count fits a `u8`; the saturation is the
        // count the service refuses, never one that is sent short.
        let count = u8::try_from(words.len()).unwrap_or(u8::MAX);
        let result = call!(
            "SendSingleMessageWordArray",
            self.connection,
            SendSingleMessageWordArray(
                timestamp,
                0,
                count,
                u32::from(count),
                words.as_ptr()
            ) -> MidiSendMessageResults
        )?;
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
        // Failures are dropped on purpose. `close` runs from `Drop`, possibly while unwinding and
        // possibly after the session closed or the service went away, where there is nothing
        // left to undo and a failure must neither panic nor be reported. A handler whose removal
        // failed receives nothing once the session has disconnected the connection.
        let _ = self.connection.RemoveMessageReceived(self.token);
        let _ = call!(
            "DisconnectEndpointConnection",
            self.session,
            DisconnectEndpointConnection(self.id)
        );
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
    // A failed read has no caller to report to: the message is dropped, as is a count that is
    // not a whole message (the service sends whole messages).
    let Ok(count) = call!(
        "FillWordArray",
        *args,
        FillWordArray(0, 4, words.as_mut_ptr()) -> u8
    ) else {
        return;
    };
    let Some(ump) = words.get(..usize::from(count)).and_then(Ump::from_words) else {
        return;
    };
    let Ok(ticks) = call!("Timestamp", *args, Timestamp() -> u64) else {
        return;
    };
    let at = clock.instant(ticks);
    let mut handler = handler.lock().unwrap_or_else(PoisonError::into_inner);
    (*handler)(at, ump);
}
