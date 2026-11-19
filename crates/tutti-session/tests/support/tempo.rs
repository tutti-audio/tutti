//! A constant project tempo, written through the session's `tempo` target.

use tutti_session::{Session, ValueError};
use tutti_tempo::TempoMap;
use tutti_units::Bpm;

/// Writes `bpm` as the project's tempo and updates the session, which
/// publishes it: one step, from the next block.
pub fn set_tempo(session: &mut Session, bpm: Bpm) -> Result<(), ValueError<&'static str>> {
    let map = TempoMap::constant(bpm).expect("a test tempo is a map");
    session.set(session.transport_handle().tempo().to(map))?;
    let _published = session.update();
    Ok(())
}
