//! The output the suite runs against on Windows: the built-in GS Wavetable Synth.

use tutti_fixtures::midi::GS_WAVETABLE;
use tutti_midi_device::DeviceInfo;
use tutti_midi_hardware::Devices;

/// The built-in synth's listing, or a failure that lists the outputs.
pub(crate) fn output(devices: &Devices) -> DeviceInfo {
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
