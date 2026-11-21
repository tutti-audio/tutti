//! List this machine's MIDI devices and what each can carry, then watch for
//! devices coming and going.
//!
//! Prints, per device, its id, its name and the protocol the OS says it speaks,
//! and the Function Blocks it declares where the OS exposes them (ALSA does).
//! Expect `Midi1` for most of it: most hardware in the world is still MIDI 1.0,
//! and only a `Midi2` device carries per-note controllers and JR Timestamps to
//! the wire. Then it prints each change until the watch ends or the process is
//! interrupted.
//!
//! No flag is needed: this crate has no cargo features.
//!
//! ```text
//! cargo run --example list_devices
//! ```

use tutti_midi_device::Endpoint;
use tutti_midi_hardware::Devices;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executor = tutti_platform::Executor::new();
    let devices = Devices::open(&executor)?;
    // macOS makes CoreMIDI's client on a thread of its own; the list is seeded once it
    // has. Every other backend is ready at once.
    tutti_fixtures::block_on(devices.start())?;
    let events = devices.subscribe();

    println!("=== MIDI devices ===");
    let list = events.initial();
    if list.is_empty() {
        println!("  (none found)");
    }
    for d in list {
        let capability = devices.capability(&d.id)?;
        let protocol = capability.as_ref().map(|c| c.active().protocol);
        println!(
            "  {:<48} {:<32} {:?} {protocol:?}",
            d.id, d.name, d.direction
        );
        if let Some(left_out) = devices.blocks_left_out(&d.id)?.filter(|&n| n > 0) {
            println!("      ({left_out} declared blocks the spec cannot hold are left out)");
        }
        // The Function Blocks the OS exposes: which Groups, which way.
        for block in capability.iter().flat_map(Endpoint::function_blocks) {
            println!(
                "      block {}: Groups {}..+{} {:?} {}",
                block.id().get(),
                block.first_group().get(),
                block.groups().get(),
                block.direction(),
                block.name().map_or("", |n| n.as_str()),
            );
        }
    }

    println!("\nwatching; plug or unplug a device (Ctrl-C to stop)");
    loop {
        println!("  {:?}", tutti_fixtures::block_on(events.recv())?);
    }
}
