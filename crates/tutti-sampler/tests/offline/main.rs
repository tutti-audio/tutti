//! A sampler voice forked for an offline render.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod offline_disk_voice;
mod offline_fork_env;
