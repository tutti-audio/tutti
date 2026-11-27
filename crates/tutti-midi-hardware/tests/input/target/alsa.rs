//! The endpoints the suite runs against on Linux: the kernel's `Midi Through` port, both ways.

use std::time::Duration;
use tutti_midi_device::{DeviceEvent, DeviceInfo};
use tutti_midi_hardware::Devices;

/// A bound that only guards a hang.
const PATIENCE: Duration = Duration::from_secs(30);

/// `Midi Through`'s output.
pub(crate) fn output(devices: &Devices) -> DeviceInfo {
    through(devices, false)
}

/// `Midi Through`'s input.
pub(crate) fn input(devices: &Devices) -> DeviceInfo {
    through(devices, true)
}

/// `Midi Through`'s input or output, waiting on the watch for it when it is not listed
/// yet (ALSA's first enumeration can come at the watch's first wake), or a failure
/// that names the module that provides it.
fn through(devices: &Devices, input: bool) -> DeviceInfo {
    let keep = |d: &DeviceInfo| {
        let way = if input {
            d.direction.is_input()
        } else {
            d.direction.is_output()
        };
        way && d.name.contains("Midi Through")
    };
    let mut events = devices.subscribe();
    if let Some(device) = events.initial().iter().find(|d| keep(d)) {
        return device.clone();
    }
    loop {
        match tutti_fixtures::block_on_within(futures_lite::StreamExt::next(&mut events), PATIENCE)
        {
            Some(Some(DeviceEvent::Added(device) | DeviceEvent::Changed(device)))
                if keep(&device) =>
            {
                return device;
            }
            Some(Some(_)) => {}
            Some(None) => panic!("the device watch ended: {:?}", events.failure()),
            None => {
                panic!("the `Midi Through` output is never listed (`sudo modprobe snd-seq-dummy`)")
            }
        }
    }
}
