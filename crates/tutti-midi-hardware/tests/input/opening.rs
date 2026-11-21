//! Finishing a polled virtual endpoint in a test: each poll at the current time,
//! and between polls a wait until the opening's own deadline.

use std::task::Poll;
use std::time::Duration;
use tutti_midi_device::{DeviceEvent, DeviceInfo};
use tutti_midi_hardware::{
    Devices, InputProducerHandle, MidiHardwareError, MidiSession, OpeningVirtualEndpoint, Started,
    VirtualEndpoint, VirtualSpec,
};
use tutti_platform::{Executor, Instant, MainThreadMarker};

/// A bound that only guards a hang.
const PATIENCE: Duration = Duration::from_secs(30);

/// Polls `opening` until it is the endpoint or an error.
pub fn finished(
    mut opening: OpeningVirtualEndpoint,
    main: MainThreadMarker,
) -> Result<VirtualEndpoint, MidiHardwareError> {
    loop {
        if let Poll::Ready(made) = opening.poll(main, Instant::now()) {
            return made;
        }
        let deadline = opening.deadline();
        tutti_fixtures::wait_until("the opening's next attempt", PATIENCE, || {
            Instant::now() >= deadline
        });
    }
}

/// The endpoint `spec` describes, once the backend has started and the opening has
/// finished.
pub fn open(
    executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
    main: MainThreadMarker,
) -> Result<VirtualEndpoint, MidiHardwareError> {
    // The one way to a `Started`: a `Devices` over the platform's backend.
    let devices = Devices::open(executor)?;
    let started = tutti_fixtures::block_on(devices.start()).expect("the backend starts");
    finished(
        VirtualEndpoint::open(executor, spec, producer, started, main)?,
        main,
    )
}

/// The proof that `session`'s backend has started.
pub fn started(session: &MidiSession) -> Started {
    tutti_fixtures::block_on(session.start()).expect("the backend starts")
}

/// The first device `session` lists that `keep` accepts, waiting on the watch
/// for it when it is not listed yet: a backend's first enumeration can come at
/// the watch's first wake rather than at open (ALSA's, while clients come and
/// go), and a virtual endpoint is listed when the watch sees it.
pub fn listed(session: &MidiSession, keep: impl Fn(&DeviceInfo) -> bool) -> DeviceInfo {
    let events = session.devices().subscribe();
    if let Some(device) = events.initial().iter().find(|d| keep(d)) {
        return device.clone();
    }
    loop {
        match tutti_fixtures::block_on_within(events.recv(), PATIENCE) {
            Some(Ok(DeviceEvent::Added(device) | DeviceEvent::Changed(device)))
                if keep(&device) =>
            {
                return device;
            }
            Some(Ok(_)) => {}
            Some(Err(failure)) => panic!("the device watch ended: {failure:?}"),
            None => panic!("the device is never listed"),
        }
    }
}
