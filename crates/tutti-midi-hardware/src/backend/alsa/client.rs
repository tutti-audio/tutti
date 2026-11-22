//! Pairing an ALSA queue clock with `Instant`.

use crate::stamp::Paired;
use tutti_alsa_ffi::{Client, QueueClock};
use tutti_platform::Instant;

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
