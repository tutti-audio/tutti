//! The tests are the architecture's: `flush.rs` where a control register is
//! flushed, `other.rs` where the call only runs its closure.

#[cfg_attr(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    path = "tests/flush.rs"
)]
#[cfg_attr(
    not(any(target_arch = "x86_64", target_arch = "aarch64")),
    path = "tests/other.rs"
)]
mod arch;
