//! Where bytes come from and go to: a source that opens again and again, and a
//! destination that is written once, staged.

mod destination;
mod source;
mod staged;
mod stamp;

pub use destination::Destination;
pub use source::{Asset, Identity, Open, ReadSeek};
pub use staged::{Staged, StagedFile};
pub use stamp::Stamp;
