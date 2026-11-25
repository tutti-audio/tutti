//! [`PrerollTable`]: a shared copy of `Plan::compensation()` for a host that
//! displays it.

use tutti_rt::Mutex;

use tutti_units::Samples;

/// The per-channel compensation of the plan sent last, indexed by output
/// channel: how much the compiler delays that output to align it with the
/// latest. The session republishes it with every plan sent, and a host reads
/// it through [`Session::compensation`](crate::Session::compensation) to
/// display. A source inside the graph needs none of it: the compiler
/// aligns it.
///
/// A plain lock, not a [`Publisher`](tutti_rt::Publisher): **no audio thread
/// reads it.** The writer is the session's commit and the reader is a
/// host, so a reader registered per thread and a retirement
/// list would buy nothing. Each call holds the lock for one copy of the
/// table, or of one entry, and [`with`](Self::with) for its closure; the
/// session's write overwrites the table in place and frees nothing once its
/// capacity covers the channel count. A clone of the `Arc` reads every later
/// plan's table.
#[derive(Debug, Default)]
pub struct PrerollTable(Mutex<Vec<Samples>>);

impl PrerollTable {
    /// Replaces the table with `channels`. Not for the audio thread: it
    /// locks, and allocates when the table grows.
    pub(crate) fn set(&self, channels: &[Samples]) {
        let mut table = self.0.lock();
        table.clear();
        table.extend_from_slice(channels);
    }

    /// `channel`'s pre-roll, or `None` past the end of the table (no
    /// compensation needed).
    pub fn preroll(&self, channel: usize) -> Option<Samples> {
        self.0.lock().get(channel).copied()
    }

    /// The whole table, copied.
    pub fn to_vec(&self) -> Vec<Samples> {
        self.0.lock().clone()
    }

    /// `f` over the table under one lock: a consistent view for a pass over
    /// several channels. Keep `f` short: the session's next write waits on it.
    pub fn with<R>(&self, f: impl FnOnce(&[Samples]) -> R) -> R {
        f(&self.0.lock())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A `set` replaces the whole table**: a shorter table leaves no
    /// entry of the longer one behind, and every reader sees the last `set`.
    #[test]
    fn a_set_replaces_the_whole_table() {
        let table = PrerollTable::default();
        table.set(&[Samples::new(200), Samples::new(0), Samples::new(100)]);
        assert_eq!(table.preroll(2), Some(Samples::new(100)));
        table.set(&[Samples::new(7)]);
        assert_eq!(table.preroll(0), Some(Samples::new(7)));
        assert_eq!(table.preroll(1), None, "the old entry is gone");
        assert_eq!(table.to_vec(), [Samples::new(7)]);
        assert_eq!(table.with(<[Samples]>::len), 1);
    }
}
