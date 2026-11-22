//! Where a plugin runs is its load's `Hosting`: each value
//! loads that way, and a hosting a format cannot have is refused at the load.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod blacklist;
mod hosting;
mod scanned_link;
