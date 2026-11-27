//! Windows Multimedia has no virtual MIDI ports: opening an endpoint is refused by name.

use std::task::Poll;
use std::time::Duration;
use tutti_fixtures::main_thread;
use tutti_fixtures::midi::device_identity;
use tutti_graph::EventCapacity;
use tutti_midi_hardware::{
    Devices, HardwareMidiInputs, MidiHardwareError, VirtualEndpoint, VirtualSpec,
};
use tutti_midi_runtime::InputInlets;
use tutti_platform::{Executor, Instant};
use tutti_ump::EndpointName;

#[test]
fn a_virtual_endpoint_is_refused_as_unsupported_on_windows_multimedia() {
    let executor = Executor::new();
    let devices = Devices::open(&executor).unwrap();
    let started = tutti_fixtures::block_on(devices.start()).expect("the backend starts");
    let spec = VirtualSpec::new(
        EndpointName::new("tutti-virt-unsupported").unwrap(),
        device_identity(),
        None,
    );
    let ports = HardwareMidiInputs::new(InputInlets::new(EventCapacity::of(8), Duration::ZERO));
    let producer = ports.open_input_port("virtual").0;
    let main = main_thread();
    let mut opening = VirtualEndpoint::open(&executor, &spec, producer, started, main)
        .expect("the refusal is the first poll's");
    match opening.poll(main, Instant::now()) {
        Poll::Ready(Err(MidiHardwareError::Unsupported(_))) => {}
        other => panic!("expected Unsupported, got {other:?}"),
    }
}
