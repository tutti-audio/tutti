//! **A 440 Hz tone on the default device, with no ECS.** The session opens
//! the device first and builds the graph at the rate it reports; `update()`
//! commits the edits. Run (needs a sound card):
//! `cargo run -p tutti --features device --example headless_engine`
use tutti::graph::ForkByClone;
use tutti::{Amplitude, Hz, Root, SessionBuilder, SessionEvent};
use tutti_nodes::testing::Osc;

/// What `Session::update` found: faults and dropouts (a refused commit, a
/// plugin that died, a lost block). A host logs them; here they are printed.
fn report(events: Vec<SessionEvent>) {
    for event in events {
        eprintln!("session event: {event:?}");
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut live = SessionBuilder::default()
        .device(tutti::session::DeviceSelector::Default)
        .build()?;
    let tone = Osc::sine(Hz::new(440.0)).with_amplitude(Amplitude::new(0.2));
    let (tone, _) = live.graph().add(ForkByClone(tone));
    live.graph().connect(tone, Root).unwrap();
    report(live.update());
    live.play()?;
    std::thread::sleep(std::time::Duration::from_secs(2));
    println!("running: {}", live.device().is_running()); // dropping `live` stops it
    Ok(())
}
