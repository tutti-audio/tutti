//! [`RtLogReaders`]: the control side of the audio thread's log.
//!
//! A node logs through `Io::log` and the executor moves what it logged into
//! a ring per pool participant ([`tutti_graph::RtLogReader`]). The registry
//! holds those readers for as long as the engine lives, outlives every plan
//! swap, and is dropped on the control side with the engine. It names no
//! logging crate: [`drain`](RtLogReaders::drain) hands each record to a
//! closure, and the session turns it into whatever its subscriber takes.
//!
//! Each sizing of the executor (its build, each
//! [`set_pool`](tutti_graph::Executor::set_pool)) makes new rings;
//! [`adopt`](RtLogReaders::adopt) takes the new readers and reads out the
//! old ones before it drops them, so a record in flight is delivered by the
//! next drain, never freed unread.

use std::sync::{Arc, Mutex, PoisonError};

use tutti_graph::{Executor, LogFilter, LogGate, RtLogReader, RtRecord};

/// The readers of an executor's log rings, shared by the engine and the
/// control thread. A clone is the same registry.
///
/// Control thread only: the audio thread never reaches it.
#[derive(Clone, Debug)]
pub struct RtLogReaders(Arc<Mutex<Registry>>);

#[derive(Debug)]
struct Registry {
    gate: LogGate,
    readers: Vec<RtLogReader>,
    /// Records read out of readers that were dropped since the last drain.
    held: Vec<RtRecord>,
    /// Losses of the readers dropped so far.
    retired_lost: u64,
    /// The total the last drain reported.
    seen_lost: u64,
    /// The records of one drain, merged.
    merged: Vec<RtRecord>,
}

impl Registry {
    fn total_lost(&self) -> u64 {
        self.retired_lost + self.readers.iter().map(RtLogReader::lost).sum::<u64>()
    }
}

impl RtLogReaders {
    /// The registry of `exec`'s rings, which it takes the readers of.
    ///
    /// Control thread; allocates.
    #[must_use]
    pub fn new(exec: &mut Executor) -> Self {
        let registry = Self(Arc::new(Mutex::new(Registry {
            gate: exec.log_gate(),
            readers: Vec::new(),
            held: Vec::new(),
            retired_lost: 0,
            seen_lost: 0,
            merged: Vec::new(),
        })));
        registry.adopt(exec);
        registry
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the readers `exec` built since the last call (after a
    /// [`set_pool`](Executor::set_pool)). The readers it replaces are read
    /// out first: what they hold is delivered by the next
    /// [`drain`](Self::drain), and their losses stay counted. Does nothing
    /// when the executor was not resized.
    ///
    /// Control thread, with the executor not rendering; allocates.
    pub fn adopt(&self, exec: &mut Executor) {
        let fresh = exec.take_log_readers();
        if fresh.is_empty() {
            return;
        }
        let mut registry = self.lock();
        let Registry {
            readers,
            held,
            retired_lost,
            ..
        } = &mut *registry;
        for mut old in readers.drain(..) {
            while let Some(record) = old.pop() {
                held.push(record);
            }
            *retired_lost += old.lost();
        }
        *readers = fresh;
    }

    /// Stores `filter` where the executor loads it (the most verbose level
    /// per target the caller will emit; it applies from the executor's next
    /// block), then hands every record the rings hold to `each`, merged by
    /// frame across rings (no order is stated within one frame). Returns how
    /// many records were lost since the last call: refused by a full ring,
    /// or past a node call's fourth.
    ///
    /// Control thread; allocates the merge buffer when it grows.
    pub fn drain(&self, filter: LogFilter, mut each: impl FnMut(&RtRecord)) -> u64 {
        let mut registry = self.lock();
        registry.gate.set(filter);
        let mut merged = std::mem::take(&mut registry.merged);
        merged.clear();
        merged.append(&mut registry.held);
        for reader in &mut registry.readers {
            while let Some(record) = reader.pop() {
                merged.push(record);
            }
        }
        // A ring whose executor sizing replaced it, once read out.
        let mut gone = 0;
        registry.readers.retain(|r| {
            let done = r.is_disconnected() && r.is_empty();
            gone += if done { r.lost() } else { 0 };
            !done
        });
        registry.retired_lost += gone;
        merged.sort_by_key(RtRecord::frame);
        for record in &merged {
            each(record);
        }
        let total = registry.total_lost();
        let rise = total.saturating_sub(registry.seen_lost);
        registry.seen_lost = total;
        registry.merged = merged;
        rise
    }
}
