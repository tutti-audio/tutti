//! A saved Property Exchange state reads back as the state it was, and the document's shape
//! is the one apps persist: renaming a variant or a field breaks saved files, so the text is
//! pinned. Run with `--features serde`.
#![cfg(feature = "serde")]

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod media_type;
mod saved_state;
