//! Opening a UMP-mode ALSA sequencer client, through `tutti-alsa-ffi`.

use crate::error::Result;
use crate::stamp::Paired;
use tutti_alsa_ffi::{Client, QueueClock, UmpVersion};
use tutti_platform::Instant;

/// Opens a client in UMP mode speaking `version`, named `name` to other clients.
///
/// Enumeration, the watch and an output are MIDI 2.0 clients (they carry whatever
/// the graph sends); an input takes the protocol of its source (see
/// `input::open`), so that a MIDI 1.0 source is never converted by the kernel.
///
/// One client per connection, not one per backend: a shared client would have
/// every input pump contending on one handle. Enumeration opens a short-lived
/// client of its own for the same reason: it must not wait behind a pump.
pub(super) fn open(name: &str, version: UmpVersion) -> Result<Client> {
    Ok(Client::open(name, version)?)
}

/// `clock` read between two readings of `Instant`, as a pairing of the two
/// clocks ([`Paired::midpoint`]).
pub(super) fn pair(
    clock: &mut QueueClock,
    client: &Client,
) -> std::result::Result<Paired, tutti_alsa_ffi::Error> {
    let before = Instant::now();
    let reading = clock.now(client)?;
    Ok(Paired::midpoint(before, Instant::now(), reading))
}
