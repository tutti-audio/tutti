//! Where bytes come from and go to: a source that opens again and again, and a
//! target that is written once, staged.

mod file_stamp;
mod source;
mod staged;
mod write_target;

pub use file_stamp::FileStamp;
pub use source::{Asset, Identity, Open, ReadSeek};
pub use staged::{Staged, StagedCommitError, StagedFile, StagedKeep};
pub use write_target::WriteTarget;
