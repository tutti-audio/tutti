//! Reading UMP from a Windows MIDI Services endpoint.

use std::sync::Arc;
use tutti_platform::Instant;

use super::os;
use crate::InputFault;
use crate::InputProducerHandle;
use crate::endpoints::InputConnection;
use crate::error::Result;
use crate::guard::Guard;
use tutti_rt::FaultLatch;
use tutti_winmidi2_ffi::{Connection, Session};

/// An open input. Dropping it closes the connection.
pub(super) struct MidiServicesInput {
    /// Raised when the callback panics.
    died: Arc<FaultLatch<InputFault>>,
    // Dropped before the session it was made on.
    _connection: Connection,
    _session: Arc<Session>,
}

impl InputConnection for MidiServicesInput {
    fn fault(&self) -> Option<InputFault> {
        self.died.cause()
    }
}

/// Connects to the endpoint with device id `endpoint` and delivers what it sends into
/// `producer`.
///
/// # Errors
///
/// The platform's error from the connection.
pub(super) fn open(
    session: Arc<Session>,
    endpoint: &str,
    mut producer: InputProducerHandle,
) -> Result<MidiServicesInput> {
    let died = Arc::new(FaultLatch::<InputFault>::new());
    let guard = Guard::new(Arc::clone(&died));
    // The service calls this on a thread of its own, and a panic must not leave it: the
    // guard catches it and ends the input. A full inlet counts the drop itself (the input's
    // `lost` cell), a message that could not be read is counted here, and a log line would
    // run on the service's thread.
    let connection = session
        .connect(endpoint, move |received| {
            guard.run(|| match received {
                Ok((at, ump)) => {
                    // A timestamp that is not an `Instant` is placed at its arrival.
                    let _ = producer.push(ump, at.unwrap_or_else(Instant::now));
                }
                Err(_) => producer.count_lost(),
            });
        })
        .map_err(os)?;
    Ok(MidiServicesInput {
        died,
        _connection: connection,
        _session: session,
    })
}
