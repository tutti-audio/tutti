use tutti_fixtures::midi::GS_WAVETABLE;
use tutti_midi_device::DeviceInfo;
use tutti_midi_hardware::Devices;

/// The built-in synth's listing, or a failure that lists the outputs.
pub fn synth(devices: &Devices) -> DeviceInfo {
    let outputs: Vec<_> = devices
        .list()
        .into_iter()
        .filter(|d| d.direction.is_output())
        .collect();
    outputs
        .iter()
        .find(|d| d.name.contains(GS_WAVETABLE))
        .cloned()
        .unwrap_or_else(|| {
            let names: Vec<_> = outputs.iter().map(|d| d.name.as_str()).collect();
            panic!("the built-in GS Wavetable Synth output is missing; outputs: {names:?}")
        })
}

#[test]
fn the_synth_is_listed_as_an_output_with_a_stable_id_and_a_clean_input_list() {
    let devices = Devices::open(&tutti_platform::Executor::new()).unwrap();
    let synth = synth(&devices);
    assert!(!synth.direction.is_input(), "{synth:?}");
    // A second listing names the same device by the same id.
    let again = Devices::open(&tutti_platform::Executor::new()).unwrap();
    assert!(again.list().iter().any(|d| d.id == synth.id));
    // Inputs, of which a runner has none, are listed or not but never a
    // nameless entry.
    assert!(
        devices
            .list()
            .iter()
            .filter(|d| d.direction.is_input())
            .all(|d| !d.name.is_empty())
    );
}
