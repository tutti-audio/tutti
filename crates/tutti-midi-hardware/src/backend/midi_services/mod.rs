//! The Windows backend: Windows MIDI Services, native UMP.
//!
//! Every call into the service is `tutti-winmidi2-ffi`'s, behind safe handles; this crate
//! forbids `unsafe`. The service carries UMP words as they are in both directions, so
//! nothing here translates a message, and it has no thread of ours behind it.
//!
//! - **Enumeration** is the service's list of endpoints. An endpoint is one object that
//!   sends and receives, so each is listed once as a duplex device, named by its device
//!   interface path, which survives a replug. An [`EndpointId`] is a 64-bit FNV-1a digest
//!   of that path, never a list position, and a listing is read afresh to resolve one:
//!   opening an endpoint that has left fails with
//!   [`NotFound::Endpoint`](crate::NotFound::Endpoint). Each endpoint is described as MIDI 1.0,
//!   the conservative reading of an endpoint whose properties are not read.
//! - **Input** is a message-received callback on the service's own thread, behind a
//!   [`Guard`](crate::guard::Guard). A message the service delivered that cannot be read is
//!   counted in the port's losses.
//! - **Output** sends each packet through the connection, each at its time: the service
//!   holds a message with a time ahead of its clock until then.
//! - **The device watch** is the service's endpoint watcher: an endpoint added, removed or
//!   updated reaches the list from the notification, with no re-list. A watcher that
//!   stops, or a change it cannot read, ends the watch with the cause.
//! - **Virtual endpoints** are
//!   [`MidiHardwareError::Unsupported`](crate::MidiHardwareError::Unsupported) here.
//! - **Availability.** One session with the service is made on first use and shared by the
//!   connections. A PC without the API, or with the service stopped, fails at open
//!   ([`MidiHardwareError::MidiServicesApiMissing`],
//!   [`MidiHardwareError::MidiServicesNotRunning`]), never with a fallback to another
//!   backend.

mod input;
mod output;
mod watch;

use std::sync::Arc;

use crate::InputProducerHandle;
use crate::capability::{OsEndpoint, legacy};
use crate::devices::ChangeSink;
use crate::devices::reporter::os_devices;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{MidiHardwareError, NotFound, Result};
use crate::start::Started;
use tutti_midi_device::{EndpointId, OsDevice, OsEndpointIdentity};
use tutti_rt::Mutex;
use tutti_winmidi2_ffi::{Endpoint, Error, Session, endpoints, ensure_service_available};

/// The name of the engine's session with the service.
const SESSION: &str = "tutti";

/// `error` as this crate reports it: the API or the service not being there is the named
/// cause, anything else the platform's own.
fn os(error: Error) -> MidiHardwareError {
    match error {
        Error::ApiMissing { code } => MidiHardwareError::MidiServicesApiMissing { code },
        Error::ServiceUnavailable => MidiHardwareError::MidiServicesNotRunning,
        other => MidiHardwareError::Os(other),
    }
}

/// The [`EndpointId`] of the endpoint with device id `id`: a 64-bit FNV-1a digest of the
/// id read without regard to case (device interface paths compare so), the same in every
/// listing. A collision would name one endpoint as another, at odds of 2^-64 a pair.
fn endpoint_id(id: &str) -> EndpointId {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let hash = id
        .bytes()
        .map(|b| b.to_ascii_lowercase())
        .fold(OFFSET, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME));
    EndpointId::from_raw(hash)
}

/// An endpoint the service lists, as the endpoint it describes.
fn os_endpoint(endpoint: &Endpoint) -> OsEndpoint {
    OsEndpoint {
        id: endpoint_id(&endpoint.id),
        identity: OsEndpointIdentity::MidiServices(endpoint.id.clone()),
        name: endpoint.name.clone(),
        capability: legacy(),
        blocks_left_out: 0,
    }
}

/// The endpoints the service lists now.
fn list() -> Result<Vec<OsEndpoint>> {
    Ok(endpoints().map_err(os)?.iter().map(os_endpoint).collect())
}

/// Every device now: each endpoint sends and receives, so each is one duplex device.
fn devices() -> Result<Vec<OsDevice>> {
    let listed = list()?;
    Ok(os_devices(&listed, &listed))
}

/// The endpoint `id` names now; [`NotFound::Endpoint`] when the service lists none.
fn resolve(id: EndpointId) -> Result<Endpoint> {
    endpoints()
        .map_err(os)?
        .into_iter()
        .find(|endpoint| endpoint_id(&endpoint.id) == id)
        .ok_or_else(|| NotFound::Endpoint(id).into())
}

/// The Windows [`MidiEndpoints`] implementation. Holds the one session with the service,
/// made on first use.
pub(super) struct MidiServicesEndpoints {
    session: Mutex<Option<Arc<Session>>>,
}

impl core::fmt::Debug for MidiServicesEndpoints {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MidiServicesEndpoints")
            .finish_non_exhaustive()
    }
}

impl MidiServicesEndpoints {
    /// The backend. No call is made into the service until one is needed.
    pub(super) fn new() -> Self {
        Self {
            session: Mutex::new(None),
        }
    }

    /// The session with the service, made if there is none. A connection keeps its clone,
    /// so it outlives the backend that opened it.
    ///
    /// # Errors
    ///
    /// The named cause of the API or the service not being there, or the platform's own
    /// error from making the session.
    fn session(&self) -> Result<Arc<Session>> {
        let mut held = self.session.lock();
        if let Some(session) = &*held {
            return Ok(Arc::clone(session));
        }
        ensure_service_available().map_err(os)?;
        let session = Arc::new(Session::create(SESSION).map_err(os)?);
        *held = Some(Arc::clone(&session));
        Ok(session)
    }
}

impl MidiEndpoints for MidiServicesEndpoints {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        watch::start(sink)
    }

    fn inputs(&self) -> Result<Vec<OsEndpoint>> {
        list()
    }

    fn outputs(&self) -> Result<Vec<OsEndpoint>> {
        list()
    }

    fn open_input(
        &self,
        _started: Started,
        id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        let endpoint = resolve(id)?;
        Ok(Box::new(input::open(
            self.session()?,
            &endpoint.id,
            producer,
        )?))
    }

    fn open_output(&self, _started: Started, id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        let endpoint = resolve(id)?;
        Ok(Box::new(output::open(self.session()?, &endpoint.id)?))
    }
}

#[cfg(test)]
mod tests;
