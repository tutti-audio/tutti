//! Where bytes come from and go to: a source that opens again and again, and a
//! destination that is written once, staged.

mod destination;
mod file_stamp;
mod source;
mod staged;

pub use destination::Destination;
pub use file_stamp::FileStamp;
pub use source::{Asset, Identity, Open, ReadSeek};
pub use staged::{Staged, StagedCommitError, StagedFile, StagedKeep};
