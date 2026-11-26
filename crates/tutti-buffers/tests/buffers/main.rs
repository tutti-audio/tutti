//! The `tutti-buffers` integration tests: the sample conversions, `I24`, the
//! views, the layout conversions and the fold, each pinned to the spec's
//! figures. The binary installs `AllocDisabler`, so an allocation inside an
//! `assert_no_alloc` closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod convert;
mod float;
mod fold_no_alloc;
mod fold_to_channels;
mod fold_views;
mod i24;
#[cfg(feature = "serde")]
mod i24_serde;
mod interleaved;
mod planes;
mod sample_conversion;
mod sample_format;
mod views;
