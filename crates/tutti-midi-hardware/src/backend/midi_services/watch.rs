//! The device watch: the service's endpoint watcher, feeding the device list.

use std::sync::Arc;

use super::{devices, endpoint_id, os, os_endpoint};
use crate::devices::ChangeSink;
use crate::endpoints::Watch;
use crate::error::Result;
use tutti_midi_device::{OsChange, OsDevice, PortDirection};
use tutti_rt::Mutex;
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

/// What the watcher's callback does with a change.
enum Phase {
    /// The list is not read yet: changes wait, in order.
    Buffering(Vec<core::result::Result<Change, Error>>),
    /// The list is seeded: changes go to the sink.
    Live,
}

/// The list changes of `buffered`, in order, up to the first that is an error, and that
/// error. A change that predates the list it is applied after is idempotent in the table
/// (present again what is present, gone again what is gone), and one that postdates it is
/// the truth.
pub(super) fn replayed(
    buffered: Vec<core::result::Result<Change, Error>>,
) -> (Vec<OsChange>, Option<Error>) {
    let mut changes = Vec::new();
    for change in buffered {
        match change {
            Ok(change) => changes.push(change_of(change)),
            Err(error) => return (changes, Some(error)),
        }
    }
    (changes, None)
}

/// Follows the service's changes into `sink`, after listing the endpoints present.
///
/// The watcher starts first and its callback only buffers while the list is read, under the
/// callback's lock. The list is seeded from that read, then the buffered changes are applied
/// in order and the callback goes live, so no change is lost to the gap and none is applied
/// out of order: an endpoint removed after the list was read is gone, never resurrected. A
/// change the watcher cannot read, or the watcher stopping, ends the watch with that cause:
/// the list is then out of date.
///
/// # Errors
///
/// The named cause of the API or the service not being there, or the platform's own error.
pub(super) fn start(sink: ChangeSink) -> Result<Box<dyn Watch>> {
    ensure_service_available().map_err(os)?;
    let phase = Arc::new(Mutex::new(Phase::Buffering(Vec::new())));
    let live = Arc::clone(&phase);
    let live_sink = sink.clone();
    let watcher =
        Watcher::start(move |change: core::result::Result<Change, Error>| {
            match &mut *live.lock() {
                Phase::Buffering(buffered) => buffered.push(change),
                Phase::Live => match change {
                    Ok(change) => live_sink.send(change_of(change)),
                    Err(error) => live_sink.fail(os(error)),
                },
            }
        })
        .map_err(os)?;
    let mut phase = phase.lock();
    sink.seed(devices()?)?;
    if let Phase::Buffering(buffered) = std::mem::replace(&mut *phase, Phase::Live) {
        let (changes, error) = replayed(buffered);
        sink.send_all(changes);
        if let Some(error) = error {
            sink.fail(os(error));
        }
    }
    drop(phase);
    Ok(Box::new(MidiServicesWatch { _watcher: watcher }))
}
