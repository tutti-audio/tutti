//! x86_64: FTZ and DAZ in MXCSR.

/// The MXCSR value read on entry.
pub(super) struct Saved(u32);

/// Sets FTZ|DAZ and returns the previous MXCSR.
#[inline]
pub(super) fn set() -> Saved {
    // SAFETY: `stmxcsr`/`ldmxcsr` read and write only this thread's
    // MXCSR register through a pointer to a live local `u32` (4 bytes,
    // as `stmxcsr` stores and `ldmxcsr` loads); SSE is part of the
    // x86_64 baseline, and `nostack` holds because neither touches the
    // stack. Setting FTZ|DAZ changes no memory, only how denormals
    // round in float code (see `# Soundness` on the caller: no memory
    // safety depends on an exact float result).
    unsafe {
        let mut prev: u32 = 0;
        core::arch::asm!("stmxcsr [{}]", in(reg) &mut prev, options(nostack));
        let new = prev | 0x8040; // FTZ | DAZ
        core::arch::asm!("ldmxcsr [{}]", in(reg) &new, options(nostack));
        Saved(prev)
    }
}

/// Writes the saved MXCSR back.
#[inline]
pub(super) fn restore(saved: &Saved) {
    // SAFETY: restores the MXCSR value `set` read from this same thread
    // (`Restore` is `!Send`, so drop runs on it) through a pointer to the
    // live `u32` field; the value came from `stmxcsr`, so it has no
    // reserved bit set that `ldmxcsr` would fault on.
    unsafe {
        core::arch::asm!("ldmxcsr [{}]", in(reg) &saved.0, options(nostack));
    }
}
