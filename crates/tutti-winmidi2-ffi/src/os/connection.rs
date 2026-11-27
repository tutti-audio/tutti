//! A connection to one endpoint: sends and receives UMP words.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use tutti_platform::Instant;
use tutti_ump::Ump;
use windows_core::{HSTRING, Ref};

use super::call::{abi, call, contained};
use super::{Clock, Error};
use crate::bindings::Windows::Devices::Midi2::IMidiEndpointConnectionSource;
use crate::bindings::Windows::Devices::Midi2::{
    IMidiEndpointMessageProcessingPlugin, IMidiMessageReceivedEventSource, MidiEndpointConnection,
    MidiMessageProcessingPluginAddResult, MidiMessageReceivedEventArgs, MidiSendMessageResults,
    MidiSession,
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
    /// The connection's id, which the session disconnects it by.
    id: windows_core::GUID,
    /// The token of the registered message handler.
    token: i64,
    closed: AtomicBool,
}

impl Connection {
    /// Creates the connection, adds `plugin` if there is one, registers the handler, then
    /// opens it: the service sends nothing before the open, and takes a plugin before it.
    pub(super) fn open<F>(
        session: &MidiSession,
        clock: Clock,
        endpoint_id: &str,
        plugin: Option<&IMidiEndpointMessageProcessingPlugin>,
        on_message: F,
    ) -> Result<Self, Error>
    where
        F: FnMut(Result<(Option<Instant>, Ump), Error>) + Send + 'static,
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
        // The id is read once, so that closing never has a call to fail. The API gives it only
        // through the connection, so a failure here leaves the session's unopened connection
        // registered until the session closes: with no id there is nothing to disconnect it by.
        let source: IMidiEndpointConnectionSource = windows_core::Interface::cast(&connection)
            .map_err(super::error::call("QueryInterface"))?;
        let id = call!("ConnectionId", source, ConnectionId() -> windows_core::GUID)?;
        if let Some(plugin) = plugin {
            let added = call!(
                "AddMessageProcessingPlugin",
                connection,
                AddMessageProcessingPlugin(windows_core::Interface::as_raw(plugin))
                    -> MidiMessageProcessingPluginAddResult
            );
            match added {
                Ok(MidiMessageProcessingPluginAddResult::Succeeded) => {}
                refused => {
                    disconnect(session, id);
                    return Err(refused.err().unwrap_or(Error::Refused {
                        call: "AddMessageProcessingPlugin",
                    }));
                }
            }
        }
        let handler = Mutex::new(on_message);
        let received = TypedEventHandler::<
            IMidiMessageReceivedEventSource,
            MidiMessageReceivedEventArgs,
        >::new(move |_, args: Ref<MidiMessageReceivedEventArgs>| {
            if let Some(args) = args.as_ref() {
                contained(|| deliver(&handler, clock, args));
            } else {
                contained(|| {
                    let mut handler = handler.lock().unwrap_or_else(PoisonError::into_inner);
                    (*handler)(Err(Error::Refused {
                        call: "message arguments",
                    }));
                });
            }
            Ok(())
        });
        let token = match connection.MessageReceived(&received) {
            Ok(token) => token,
            Err(e) => {
                // The registration's error is the one to report.
                disconnect(session, id);
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
        // `Ump::words` is one to four words, so both counts fit. Only the word count saturates,
        // to a count the service refuses; the array length is the slice's own, so the service
        // never reads past it (and a length that did not fit would send none).
        let count = u8::try_from(words.len()).unwrap_or(u8::MAX);
        let len = u32::try_from(words.len()).unwrap_or(0);
        let result = call!(
            "SendSingleMessageWordArray",
            self.connection,
            SendSingleMessageWordArray(
                timestamp,
                0,
                count,
                len,
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
        disconnect(&self.session, self.id);
    }
}

/// Has the session disconnect the connection `id`. A failure is dropped: it runs on the
/// paths that are already ending, where there is nothing left to undo.
fn disconnect(session: &MidiSession, id: windows_core::GUID) {
    let _ = call!(
        "DisconnectEndpointConnection",
        *session,
        DisconnectEndpointConnection(id)
    );
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

/// Hands the handler one received message, or why there is none: a failed read, or a word
/// count that is not a whole message (the service sends whole messages).
fn deliver<F: FnMut(Result<(Option<Instant>, Ump), Error>)>(
    handler: &Mutex<F>,
    clock: Clock,
    args: &MidiMessageReceivedEventArgs,
) {
    let received = read(clock, args);
    let mut handler = handler.lock().unwrap_or_else(PoisonError::into_inner);
    (*handler)(received);
}

/// The message `args` carries, with its time.
fn read(
    clock: Clock,
    args: &MidiMessageReceivedEventArgs,
) -> Result<(Option<Instant>, Ump), Error> {
    let mut words = [0u32; 4];
    let count = call!(
        "FillWordArray",
        *args,
        FillWordArray(0, 4, words.as_mut_ptr()) -> u8
    )?;
    let ump = words
        .get(..usize::from(count))
        .and_then(Ump::from_words)
        .ok_or(Error::NotAMessage { words: count })?;
    let ticks = call!("Timestamp", *args, Timestamp() -> u64)?;
    Ok((clock.instant(ticks), ump))
}
