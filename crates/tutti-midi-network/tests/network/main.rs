//! Network MIDI 2.0 (M2-124): the wire format, the session machines over a
//! simulated network, and the driver over real sockets.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod auth;
mod discovery;
mod props;
mod roles;
mod sessions;
mod sim;
mod udp;
mod wire;
