//! Waiting in a test for a session to list a device.

use std::time::Duration;
use tutti_midi_device::{DeviceEvent, DeviceInfo};
use tutti_midi_hardware::MidiSession;

/// A bound that only guards a hang.
const PATIENCE: Duration = Duration::from_secs(30);

/// The first device `session` lists that `keep` accepts, waiting on the watch
/// for it when it is not listed yet: a backend's first enumeration can come at
/// the watch's first wake rather than at open (ALSA's, while clients come and
/// go), and a virtual endpoint is listed when the watch sees it. `what` names
/// the device in the panic when it never is.
pub fn listed(session: &MidiSession, what: &str, keep: impl Fn(&DeviceInfo) -> bool) -> DeviceInfo {
    let mut events = session.devices().subscribe();
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
            None => panic!("{what} is never listed"),
        }
    }
}
