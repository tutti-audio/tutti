//! Finishing a polled virtual endpoint in a test: each poll at the current time,
//! and between polls a wait until the opening's own deadline.

use std::task::Poll;
use std::time::Duration;
use tutti_midi_hardware::{
    Devices, InputProducerHandle, MidiHardwareError, OpeningVirtualEndpoint, VirtualEndpoint,
    VirtualSpec,
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
    let started = tutti_fixtures::block_on(devices.start())?;
    finished(
        VirtualEndpoint::open(executor, spec, producer, started, main)?,
        main,
    )
}
