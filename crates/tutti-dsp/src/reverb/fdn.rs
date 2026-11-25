//! The feedback delay network: its design (controls to a
//! [`Network`](super::network::Network)) and the bank that runs it over buffers views.

pub(crate) mod bank;
pub(crate) mod design;
pub(crate) mod kind;
pub(crate) mod shimmer;
