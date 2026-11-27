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
use tutti_ump::Ump;
use tutti_winmidi2_ffi::{Connection, Error, Session};

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

/// The handler a connection delivers into `producer` with: a message goes in as the input
/// port's would, one that could not be read is counted, and a panic is caught by a
/// [`Guard`] that raises `died`.
///
/// The service calls it on a thread of its own, so a full inlet counts the drop itself (the
/// input's `lost` cell) and a log line would run on the service's thread.
pub(super) fn handler(
    mut producer: InputProducerHandle,
    died: Arc<FaultLatch<InputFault>>,
) -> impl FnMut(core::result::Result<(Option<Instant>, Ump), Error>) + Send + 'static {
    let guard = Guard::new(died);
    move |received| {
        guard.run(|| match received {
            Ok((at, ump)) => {
                // A timestamp that is not an `Instant` is placed at its arrival.
                let _ = producer.push(ump, at.unwrap_or_else(Instant::now));
            }
            Err(_) => producer.count_lost(),
        });
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
    producer: InputProducerHandle,
) -> Result<MidiServicesInput> {
    let died = Arc::new(FaultLatch::<InputFault>::new());
    let connection = session
        .connect(endpoint, handler(producer, Arc::clone(&died)))
        .map_err(os)?;
    Ok(MidiServicesInput {
        died,
        _connection: connection,
        _session: session,
    })
}
