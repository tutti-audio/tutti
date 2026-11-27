//! The device watch: the service's endpoint watcher, feeding the device list.

use super::{devices, endpoint_id, os, os_endpoint};
use crate::devices::ChangeSink;
use crate::devices::reporter::Reporter;
use crate::endpoints::Watch;
use crate::error::Result;
use tutti_midi_device::{OsChange, OsDevice, PortDirection};
use tutti_winmidi2_ffi::{Change, Error, Watcher, ensure_service_available};

/// A running watch. Dropping it stops the watcher.
pub(super) struct MidiServicesWatch {
    _watcher: Watcher,
}

impl Watch for MidiServicesWatch {}

/// The device list change a watcher's `change` makes: an endpoint is present as a duplex
/// device when it is added or updated, and gone when it is removed.
pub(super) fn change_of(change: Change) -> OsChange {
    let present = |endpoint| {
        let endpoint = os_endpoint(&endpoint);
        OsChange::Present(OsDevice {
            handle: endpoint.id,
            identity: endpoint.identity,
            name: endpoint.name,
            direction: PortDirection::Duplex,
        })
    };
    match change {
        Change::Added(endpoint) | Change::Updated(endpoint) => present(endpoint),
        Change::Removed(id) => OsChange::Gone(endpoint_id(&id)),
    }
}

/// Lists the endpoints present into `sink`, then follows the service's changes.
///
/// The watcher reports each endpoint already present as added once it starts, which
/// changes nothing in the list the seed made; an endpoint that came between the list and
/// the watcher is added by it. A change the watcher cannot read, or the watcher stopping,
/// ends the watch with that cause: the list is then out of date.
///
/// # Errors
///
/// The named cause of the API or the service not being there, or the platform's own error.
pub(super) fn start(sink: ChangeSink) -> Result<Box<dyn Watch>> {
    ensure_service_available().map_err(os)?;
    let mut reporter = Reporter::new(sink.clone());
    reporter.seed(devices()?)?;
    let watcher = Watcher::start(
        move |change: core::result::Result<Change, Error>| match change {
            Ok(change) => sink.send(change_of(change)),
            Err(error) => sink.fail(os(error)),
        },
    )
    .map_err(os)?;
    // The watcher replays only what is present, so an endpoint that left between the seed and
    // its start is reported gone by the whole list.
    reporter.report(devices()?, |_| false);
    Ok(Box::new(MidiServicesWatch { _watcher: watcher }))
}
