use super::*;

#[test]
fn it_enumerates_nothing() {
    let b = StubEndpoints::new();
    assert!(b.inputs().is_empty());
    assert!(b.outputs().is_empty());
}

/// Opening must be `Unsupported`, not a not-found error — the caller renders
/// these differently, and conflating them hides a build fact behind a
/// hardware one.
#[test]
fn opening_reports_unsupported_rather_than_not_found() {
    let b = StubEndpoints::new();
    assert!(matches!(
        b.open_output(EndpointId::from_raw(0)),
        Err(MidiHardwareError::Unsupported(_))
    ));
}

/// Where there is no backend, `Devices` opens, lists nothing and stays quiet:
/// the same "no devices" enumeration reports, with opening left `Unsupported`.
#[test]
fn devices_over_the_stub_is_empty_and_quiet() {
    let devices = crate::Devices::open_over(
        &tutti_platform::Executor::new(),
        Box::new(StubEndpoints::new()),
    )
    .unwrap();
    assert!(devices.list().is_empty());
    assert!(matches!(
        devices
            .subscribe()
            .recv_timeout(std::time::Duration::from_millis(10)),
        Err(crate::RecvTimeoutError::Timeout)
    ));
}

/// A platform with no backend publishes no virtual endpoint, and says so with
/// the named error on the first poll rather than returning an endpoint that does
/// nothing.
#[test]
fn virtual_endpoints_report_unsupported_on_the_first_poll() {
    let spec = VirtualSpec::new(
        tutti_ump::EndpointName::new("tutti-stub").unwrap(),
        tutti_fixtures::midi::device_identity(),
        None,
    );
    // The stub publishes nothing, so the stand-in marker is enough.
    let main = tutti_fixtures::main_thread();
    let inlets = tutti_midi_runtime::InputInlets::new(
        tutti_graph::EventCapacity::of(8),
        std::time::Duration::ZERO,
    );
    let ports = crate::HardwareMidiInputs::new(inlets);
    let producer = ports.open_input_port("virtual").0;
    let devices = crate::Devices::open_over(
        &tutti_platform::Executor::new(),
        Box::new(StubEndpoints::new()),
    )
    .unwrap();
    let started = tutti_fixtures::block_on(devices.start()).unwrap();
    let mut opening = open_virtual_endpoint(
        &tutti_platform::Executor::new(),
        &spec,
        producer,
        started,
        main,
    )
    .expect("the stub makes its refusal a value");
    assert!(matches!(
        opening.poll(main, tutti_platform::Instant::now()),
        std::task::Poll::Ready(Err(MidiHardwareError::Unsupported(_)))
    ));
}
