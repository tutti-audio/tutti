//! The parts of a [`Session`](super::Session): each owns one subsystem's bookkeeping and the
//! methods that use it.

mod output;
mod pool;
mod reports;
mod transport;

pub(crate) use output::OutputPart;
pub(crate) use pool::PoolPart;
pub(crate) use reports::Reports;
pub(crate) use transport::TransportPart;
