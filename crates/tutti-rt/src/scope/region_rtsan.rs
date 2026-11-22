//! The sanitizer seam with the `rtsan` feature on: one RealtimeSanitizer region per guard.

use rtsan_standalone::{ScopedDisabler, ScopedSanitizeRealtime, ensure_initialized};

/// A realtime region of the sanitizer, ended on drop. The sanitizer counts its
/// depth, so guards nest and drop in any order.
pub(super) struct Region(ScopedSanitizeRealtime);

impl core::fmt::Debug for Region {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Region")
    }
}

impl Region {
    pub(super) fn enter() -> Self {
        // Initialising allocates, so it happens before the region opens; later calls return at once.
        ensure_initialized();
        Self(ScopedSanitizeRealtime::default())
    }
}

/// Sanitizer error reporting off until drop. The sanitizer counts its depth, so it nests.
pub(super) struct Quiet(ScopedDisabler);

impl Quiet {
    pub(super) fn new() -> Self {
        Self(ScopedDisabler::default())
    }
}
