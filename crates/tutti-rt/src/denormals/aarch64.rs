//! aarch64: FZ in FPCR.

/// The FPCR value read on entry.
pub(super) struct Saved(u64);

/// Sets FZ and returns the previous FPCR.
#[inline]
pub(super) fn set() -> Saved {
    // SAFETY: `mrs`/`msr` on FPCR read and write only this thread's
    // floating-point control register and touch no memory; FPCR is
    // available at EL0 on aarch64. Setting FZ changes only how
    // denormals are handled in float code, and no memory safety
    // depends on an exact float result (see `# Soundness` on `with_no_denormals`).
    unsafe {
        let prev: u64;
        core::arch::asm!("mrs {}, fpcr", out(reg) prev);
        core::arch::asm!("msr fpcr, {}", in(reg) prev | (1 << 24)); // FZ
        Saved(prev)
    }
}

/// Writes the saved FPCR back.
#[inline]
pub(super) fn restore(saved: &Saved) {
    // SAFETY: writes back the FPCR value `set` read from this same thread
    // (`Restore` is `!Send`, so drop runs on it); no memory is touched.
    unsafe {
        core::arch::asm!("msr fpcr, {}", in(reg) saved.0);
    }
}
