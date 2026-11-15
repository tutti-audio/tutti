//! The first error an offline voice hit, latched for its fork's health probe.

use std::error::Error;
use std::sync::{Arc, OnceLock};

/// The first error latched into it, kept: whether a voice rendering offline
/// has failed in a way its output cannot say. The disk voice latches one, and
/// its fork hands the graph a `tutti_graph::ForkHealth` probe
/// ([`VoiceHealth`](super::disk_voice::VoiceHealth)) that reads it. Read on
/// the control thread, after or between rendered spans, never from the audio
/// path.
///
/// Latching takes no lock and, after the first, allocates nothing (a later
/// error is dropped unboxed). [`latch`](Self::latch) boxes its first error:
/// latch from a thread that may allocate, which an offline render's is.
/// [`latch_static`](Self::latch_static) keeps a reference to an error that
/// lives forever and allocates nothing, for a failure the audio thread finds;
/// [`fault`](Self::fault), on the control side, boxes it.
#[derive(Default)]
pub(crate) struct FaultLatch {
    first: OnceLock<Kept>,
}

/// The kept error: boxed by [`FaultLatch::latch`], or a `'static` reference
/// from [`FaultLatch::latch_static`]. One cell, so the first of either wins.
enum Kept {
    Boxed(Arc<dyn Error + Send + Sync>),
    Static(&'static (dyn Error + Send + Sync)),
}

/// A `'static` error read as an owned one.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Fixed(&'static (dyn Error + Send + Sync));

impl FaultLatch {
    /// Keep `error` unless an earlier one is already kept.
    pub(crate) fn latch(&self, error: impl Error + Send + Sync + 'static) {
        if self.first.get().is_none() {
            let _ = self.first.set(Kept::Boxed(Arc::new(error)));
        }
    }

    /// Keep `error` unless an earlier one is already kept. Allocates nothing:
    /// any thread, the audio thread included.
    pub(crate) fn latch_static(&self, error: &'static (dyn Error + Send + Sync)) {
        let _ = self.first.set(Kept::Static(error));
    }

    /// `None` while the voice renders what it describes; the first failure
    /// otherwise. Once failed, it stays failed.
    pub(crate) fn fault(&self) -> Option<Arc<dyn Error + Send + Sync>> {
        self.first.get().map(|kept| match kept {
            Kept::Boxed(e) => Arc::clone(e),
            Kept::Static(e) => Arc::new(Fixed(*e)) as Arc<dyn Error + Send + Sync>,
        })
    }
}

impl core::fmt::Debug for FaultLatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.fault() {
            Some(e) => write!(f, "FaultLatch({e})"),
            None => f.write_str("FaultLatch(ok)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct E(&'static str);
    impl core::fmt::Display for E {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl Error for E {}

    /// The first error is kept: a later one does not replace it, and an
    /// untouched latch reports nothing.
    #[test]
    fn the_first_error_is_kept() {
        let l = FaultLatch::default();
        assert!(l.fault().is_none());
        l.latch(E("first"));
        l.latch(E("second"));
        assert_eq!(l.fault().expect("latched").to_string(), "first");
    }

    /// A static error latches and reads back as the same error.
    #[test]
    fn a_static_error_is_kept() {
        static E_STATIC: E = E("fixed");
        let l = FaultLatch::default();
        l.latch_static(&E_STATIC);
        assert_eq!(l.fault().expect("latched").to_string(), "fixed");
    }

    /// The first error wins across both paths: a boxed error after a static
    /// one does not replace it, nor a static one after a boxed one.
    #[test]
    fn the_first_error_wins_across_both_paths() {
        static E_STATIC: E = E("static");
        let l = FaultLatch::default();
        l.latch_static(&E_STATIC);
        l.latch(E("boxed"));
        assert_eq!(l.fault().expect("latched").to_string(), "static");
        let l = FaultLatch::default();
        l.latch(E("boxed"));
        l.latch_static(&E_STATIC);
        assert_eq!(l.fault().expect("latched").to_string(), "boxed");
    }
}
