//! Where bytes come from and go to: a source that opens again and again, and a
//! destination that is written once.

mod destination;
mod source;
mod stamp;

pub use destination::Destination;
pub use source::{Asset, Identity, Open, ReadSeek};
pub use stamp::Stamp;
