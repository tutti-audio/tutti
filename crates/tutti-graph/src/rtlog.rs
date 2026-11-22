//! Logging from the audio thread: [`RtRecord`], what a node writes through
//! [`Io::log`](crate::Io::log), and the rings the executor moves it into.
//!
//! The audio thread never calls a logger: a subscriber may allocate, lock or
//! write to a file. A node hands `Io::log` a fixed-size record; the
//! participant that ran the node moves its records into a ring of its own
//! after the node's `process` returns, and a control-side reader formats and
//! emits them ([`RtLogReaders`]). Counts stay the API
//! (`Counter<Losses>`, `Counter<Faults>`); the log is diagnostics, and
//! nothing reads it back.
//!
//! # The record
//!
//! A record is `Copy` and fixed-size: a [`RtLevel`], a [`RtTarget`], the call
//! site's `&'static str`, the node it came from (filled by the executor, never
//! by the node), the frame, up to [`MAX_FIELDS`] named [`RtValue`]s, an inline
//! text of at most [`TEXT_CAPACITY`] bytes (a plugin's own line) and the
//! number of times the same record fired again in the same run. A record
//! carries no pointer a reader dereferences other than its `&'static str`s.
//!
//! # Slots, rings and the gate
//!
//! Each pool participant owns a `LogSlots` (four records) and a ring (256
//! records, newest dropped and counted when full). Both belong to the
//! executor, sized when it is built and in
//! [`Executor::set_pool`](crate::Executor::set_pool). A call that finds the
//! same site, level and field values in a filled slot adds to its `repeats`;
//! a record with text never coalesces; a fifth distinct record in one node
//! call is counted lost. After the node's `process` the participant moves
//! its slots into its ring and clears them.
//!
//! The level gate is one [`LogFilter`] (the most verbose level per target)
//! the control side stores through [`RtLogReaders::drain`]; the executor loads it once per
//! block, and a call below its target's level returns at once. A new
//! executor's gate is off.

use std::cell::Cell;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use tutti_rt::{
    ClaimTable, Counter, CounterWriter, Mutex, Overflow, PackedAtomic, RingReader, RingWriter,
    counter, ring,
};
use tutti_units::{Frame, Samples};

use crate::NodeKey;

/// Fields a record holds.
pub const MAX_FIELDS: usize = 3;

/// Bytes of text a record holds.
pub const TEXT_CAPACITY: usize = 96;

/// Records one participant's ring holds.
pub(crate) const RING_CAPACITY: usize = 256;

/// Records one node call can log before the rest are counted lost.
const SLOTS: usize = 4;

/// How loud a record is, from [`Error`](Self::Error) to [`Trace`](Self::Trace).
/// A later variant is more verbose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum RtLevel {
    /// Something failed.
    Error = 1,
    /// Something is wrong but the node carried on.
    Warn = 2,
    /// Something a reader of the log wants to know happened.
    Info = 3,
    /// Detail for a developer.
    Debug = 4,
    /// More detail than that.
    Trace = 5,
}

impl RtLevel {
    /// The level with this raw value, `None` for 0 or past `Trace`.
    const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Error),
            2 => Some(Self::Warn),
            3 => Some(Self::Info),
            4 => Some(Self::Debug),
            5 => Some(Self::Trace),
            _ => None,
        }
    }
}

/// Whose record it is: the graph's own nodes, or a hosted plugin's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RtTarget {
    /// A node of tutti's (`Io::log`).
    Rt,
    /// A plugin's line, through its node (`Io::log_plugin`, `Io::log_line`).
    Plugin,
}

/// A value a record carries.
#[derive(Clone, Copy, Debug)]
pub enum RtValue {
    /// A signed integer.
    I64(i64),
    /// An unsigned integer.
    U64(u64),
    /// A float, compared by its bits (so a NaN equals itself).
    F64(f64),
    /// A flag.
    Bool(bool),
}

impl PartialEq for RtValue {
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (Self::I64(a), Self::I64(b)) => a == b,
            (Self::U64(a), Self::U64(b)) => a == b,
            (Self::F64(a), Self::F64(b)) => a.to_bits() == b.to_bits(),
            (Self::Bool(a), Self::Bool(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for RtValue {}

macro_rules! value_from {
    ($($t:ty => $variant:ident as $wide:ty),* $(,)?) => {$(
        impl From<$t> for RtValue {
            fn from(v: $t) -> Self {
                Self::$variant(<$wide>::from(v))
            }
        }
    )*};
}

value_from! {
    i8 => I64 as i64, i16 => I64 as i64, i32 => I64 as i64, i64 => I64 as i64,
    u8 => U64 as u64, u16 => U64 as u64, u32 => U64 as u64, u64 => U64 as u64,
    f32 => F64 as f64, f64 => F64 as f64, bool => Bool as bool,
}

impl From<usize> for RtValue {
    fn from(v: usize) -> Self {
        Self::U64(v as u64)
    }
}

/// One line from the audio thread.
///
/// Built by [`Io::log`](crate::Io::log) (and its siblings) and read back by
/// [`RtLogReaders`]; a plugin's own line is built with [`line`](Self::line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtRecord {
    level: RtLevel,
    target: RtTarget,
    message: &'static str,
    source: NodeKey,
    /// The offset inside the block until the executor flushes the record,
    /// then the frame on the graph's clock.
    frame: Frame,
    fields: [(&'static str, RtValue); MAX_FIELDS],
    field_count: u8,
    text: [u8; TEXT_CAPACITY],
    text_len: u8,
    /// Whether it is a line (`RtRecord::line`), empty or not.
    line: bool,
    repeats: u32,
}

impl RtRecord {
    /// A record of `level` for `target`: the call site's `message` and up to
    /// [`MAX_FIELDS`] of `fields` (the rest are not kept).
    #[must_use]
    pub fn new(
        level: RtLevel,
        target: RtTarget,
        message: &'static str,
        fields: &[(&'static str, RtValue)],
    ) -> Self {
        let mut record = Self {
            level,
            target,
            message,
            source: NodeKey(0),
            frame: Frame::ZERO,
            fields: [("", RtValue::Bool(false)); MAX_FIELDS],
            field_count: 0,
            text: [0; TEXT_CAPACITY],
            text_len: 0,
            line: false,
            repeats: 0,
        };
        let kept = fields.len().min(MAX_FIELDS);
        record.fields[..kept].copy_from_slice(&fields[..kept]);
        record.field_count = kept as u8;
        record
    }

    /// A line of `text` for `target`, cut at the last character boundary
    /// within [`TEXT_CAPACITY`] bytes. A record with text never coalesces.
    #[must_use]
    pub fn line(level: RtLevel, target: RtTarget, text: &str) -> Self {
        let mut record = Self::new(level, target, "", &[]);
        let mut end = text.len().min(TEXT_CAPACITY);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        record.text[..end].copy_from_slice(&text.as_bytes()[..end]);
        record.text_len = end as u8;
        record.line = true;
        record
    }

    /// How loud it is.
    #[must_use]
    pub fn level(&self) -> RtLevel {
        self.level
    }

    /// Whose it is.
    #[must_use]
    pub fn target(&self) -> RtTarget {
        self.target
    }

    /// The call site's text; empty for a record with [`text`](Self::text).
    #[must_use]
    pub fn message(&self) -> &'static str {
        self.message
    }

    /// The node that logged it: set by the executor, never by the node.
    #[must_use]
    pub fn source(&self) -> NodeKey {
        self.source
    }

    /// Where it was logged on the graph's clock: the block's first frame
    /// plus the offset the call gave. Before the executor has flushed the
    /// record, the offset alone.
    #[must_use]
    pub fn frame(&self) -> Frame {
        self.frame
    }

    /// The named values, in call order.
    #[must_use]
    pub fn fields(&self) -> &[(&'static str, RtValue)] {
        &self.fields[..usize::from(self.field_count)]
    }

    /// The inline text of a plugin's line (empty for an empty line).
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        if !self.line {
            return None;
        }
        std::str::from_utf8(&self.text[..usize::from(self.text_len)]).ok()
    }

    /// How many more times the same record fired in the same run.
    #[must_use]
    pub fn repeats(&self) -> u32 {
        self.repeats
    }

    /// Whether a second call of the same record adds to this one's repeats.
    fn coalesces_with(&self, other: &Self) -> bool {
        !self.line
            && !other.line
            && std::ptr::eq(self.message, other.message)
            && self.level == other.level
            && self.target == other.target
            && self.fields() == other.fields()
    }
}

/// The most verbose level a target logs at, `None` for off: the control
/// side's view of what `tracing` would emit, stored where the executor loads
/// it once per block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct LogFilter {
    rt: u8,
    plugin: u8,
}

impl LogFilter {
    /// Nothing is logged.
    pub const OFF: Self = Self::new(None, None);

    /// Every record at or below `rt` for [`RtTarget::Rt`] and `plugin` for
    /// [`RtTarget::Plugin`].
    #[must_use]
    pub const fn new(rt: Option<RtLevel>, plugin: Option<RtLevel>) -> Self {
        Self {
            rt: match rt {
                Some(l) => l as u8,
                None => 0,
            },
            plugin: match plugin {
                Some(l) => l as u8,
                None => 0,
            },
        }
    }

    /// The most verbose level `target` logs at.
    #[must_use]
    pub const fn level(self, target: RtTarget) -> Option<RtLevel> {
        RtLevel::from_raw(match target {
            RtTarget::Rt => self.rt,
            RtTarget::Plugin => self.plugin,
        })
    }

    fn allows(self, target: RtTarget, level: RtLevel) -> bool {
        let max = match target {
            RtTarget::Rt => self.rt,
            RtTarget::Plugin => self.plugin,
        };
        (level as u8) <= max
    }
}

/// Where the control side stores the [`LogFilter`] an executor loads. A
/// clone shares the cell.
#[derive(Clone, Debug)]
pub(crate) struct LogGate(Arc<PackedAtomic<LogFilter>>);

impl LogGate {
    fn off() -> Self {
        Self(Arc::new(PackedAtomic::new(LogFilter::OFF)))
    }

    /// Stores `filter`: the executor reads it at the start of its next block.
    pub(crate) fn set(&self, filter: LogFilter) {
        self.0.store(filter);
    }

    /// The filter last stored.
    pub(crate) fn get(&self) -> LogFilter {
        self.0.load()
    }
}

/// One participant's slots, filled by `Io::log` while a node runs and
/// flushed after it.
pub(crate) struct LogSlots {
    slots: [Cell<RtRecord>; SLOTS],
    len: Cell<u8>,
    /// Records past the fourth, since the last flush.
    lost: Cell<u32>,
    /// The block's filter, copied from the executor's gate at its start.
    filter: Cell<LogFilter>,
}

impl LogSlots {
    fn new() -> Self {
        let blank = RtRecord::new(RtLevel::Error, RtTarget::Rt, "", &[]);
        Self {
            slots: std::array::from_fn(|_| Cell::new(blank)),
            len: Cell::new(0),
            lost: Cell::new(0),
            filter: Cell::new(LogFilter::OFF),
        }
    }

    /// Whether the block's filter lets a record of `level` for `target` in.
    #[inline]
    pub(crate) fn allows(&self, target: RtTarget, level: RtLevel) -> bool {
        self.filter.get().allows(target, level)
    }

    /// Takes `record` at `offset`: onto a slot it coalesces with, else the
    /// next free one, else counted lost. Below the filter, nothing.
    #[inline]
    pub(crate) fn push(&self, mut record: RtRecord, offset: u32) {
        if !self.allows(record.target, record.level) {
            return;
        }
        record.frame = Frame::new(u64::from(offset));
        let len = usize::from(self.len.get());
        for slot in &self.slots[..len] {
            let mut held = slot.get();
            if held.coalesces_with(&record) {
                held.repeats = held.repeats.saturating_add(1);
                slot.set(held);
                return;
            }
        }
        if len == SLOTS {
            self.lost.set(self.lost.get().saturating_add(1));
            return;
        }
        self.slots[len].set(record);
        self.len.set(len as u8 + 1);
    }

    /// Whether a flush has anything to move.
    #[inline]
    fn is_idle(&self) -> bool {
        self.len.get() == 0 && self.lost.get() == 0
    }

    /// Moves the slots into `sink` as records of `source` in the block that
    /// starts at `block`, and clears them.
    #[inline]
    pub(crate) fn flush(&self, source: NodeKey, block: Frame, sink: &mut LogSink) {
        if self.is_idle() {
            return;
        }
        self.flush_filled(source, block, sink);
    }

    #[inline(never)]
    fn flush_filled(&self, source: NodeKey, block: Frame, sink: &mut LogSink) {
        for slot in &self.slots[..usize::from(self.len.get())] {
            let mut record = slot.get();
            record.source = source;
            record.frame = block + Samples::new(record.frame.get() as usize);
            // A full ring counts the record itself (`Overflow::DropNewest`).
            let _ = sink.ring.push(record);
        }
        self.len.set(0);
        let lost = self.lost.replace(0);
        if lost != 0 {
            sink.lost.add(u64::from(lost));
        }
    }

    /// Drops what the slots hold, counting the records already lost first.
    #[inline]
    pub(crate) fn discard(&self, sink: &mut LogSink) {
        let lost = self.lost.replace(0);
        if lost != 0 {
            sink.lost.add(u64::from(lost));
        }
        self.len.set(0);
    }

    /// Takes the block's filter.
    #[inline]
    pub(crate) fn set_filter(&self, filter: LogFilter) {
        self.filter.set(filter);
    }
}

/// Where a participant's flushed records go, and the slot losses it counts.
pub(crate) struct LogSink {
    ring: RingWriter<RtRecord>,
    lost: CounterWriter<u64>,
}

/// One participant's slots and sink.
pub(crate) struct LogLane {
    pub(crate) slots: LogSlots,
    pub(crate) sink: LogSink,
}

impl LogLane {
    /// A lane and the reader of its ring.
    fn new() -> (Self, RtLogReader) {
        let (writer, reader) = ring(RING_CAPACITY, Overflow::DropNewest);
        let (lost, slots_lost) = counter();
        (
            Self {
                slots: LogSlots::new(),
                sink: LogSink { ring: writer, lost },
            },
            RtLogReader {
                ring: reader,
                slots_lost,
            },
        )
    }
}

/// One lane and its reader, for a driver that renders blocks by hand: the
/// slots a block's `Io` writes into, flushed after the block through the
/// executor's own path.
#[cfg(feature = "contract")]
pub(crate) struct LogCapture {
    lane: LogLane,
    reader: RtLogReader,
    filter: LogFilter,
}

#[cfg(feature = "contract")]
impl LogCapture {
    /// A capture that keeps what `filter` lets in.
    pub(crate) fn new(filter: LogFilter) -> Self {
        let (lane, reader) = LogLane::new();
        Self {
            lane,
            reader,
            filter,
        }
    }

    /// The slots of the block about to run, under the capture's filter.
    pub(crate) fn begin_block(&self) -> &LogSlots {
        self.lane.slots.set_filter(self.filter);
        &self.lane.slots
    }

    /// Moves the block's records, as `source`'s at the block starting at
    /// `block`, onto the end of `out`.
    pub(crate) fn end_block(&mut self, source: NodeKey, block: Frame, out: &mut Vec<RtRecord>) {
        self.lane.slots.flush(source, block, &mut self.lane.sink);
        while let Some(record) = self.reader.pop() {
            out.push(record);
        }
    }

    /// Records past a node call's fourth, since the capture was built.
    pub(crate) fn lost(&self) -> u64 {
        self.reader.lost()
    }
}

/// The control side of one participant's ring: what it holds, and what was
/// lost on the way (a full ring, or a fifth record in one node call).
#[derive(Debug)]
pub(crate) struct RtLogReader {
    ring: RingReader<RtRecord>,
    slots_lost: Counter<u64>,
}

impl RtLogReader {
    /// The oldest record not yet taken.
    pub(crate) fn pop(&mut self) -> Option<RtRecord> {
        self.ring.pop()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Whether the executor that wrote this ring no longer holds it: what is
    /// queued is all there will be.
    #[must_use]
    pub(crate) fn is_disconnected(&self) -> bool {
        self.ring.is_disconnected()
    }

    /// Records lost since the ring was built: refused by a full ring, plus
    /// those past a node call's fourth.
    #[must_use]
    pub(crate) fn lost(&self) -> u64 {
        self.ring.lost() + self.slots_lost.get()
    }
}

/// The control side of an executor's log, kept for as long as the executor
/// lives: it outlives every plan swap and is dropped on the control side
/// with it. It names no logging crate: [`drain`](Self::drain) hands each
/// record to a closure, and the session turns it into whatever its
/// subscriber takes.
///
/// Each sizing of the executor (its build, each
/// [`set_pool`](crate::Executor::set_pool)) makes new rings;
/// [`adopt`](Self::adopt) takes the new readers and reads out the old ones
/// before it drops them, so a record in flight is delivered by the next
/// drain, never freed unread. A clone is the same registry.
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
    pub fn new<F: tutti_buffers::Float>(exec: &mut crate::Executor<F>) -> Self {
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
        self.0.lock()
    }

    /// Takes the readers `exec` built since the last call (after a
    /// [`set_pool`](crate::Executor::set_pool)). The readers it replaces are read
    /// out first: what they hold is delivered by the next
    /// [`drain`](Self::drain), and their losses stay counted. Does nothing
    /// when the executor was not resized.
    ///
    /// Control thread, with the executor not rendering; allocates.
    pub fn adopt<F: tutti_buffers::Float>(&self, exec: &mut crate::Executor<F>) {
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

/// An executor's logging state: a lane per participant, the claim table the
/// parallel path claims them through, the gate, and the readers of the
/// current rings not yet taken by the control side.
pub(crate) struct Logs {
    pub(crate) lanes: Vec<LogLane>,
    pub(crate) claims: ClaimTable,
    gate: LogGate,
    fresh: Vec<RtLogReader>,
}

impl Logs {
    /// For `participants` (one for a serial executor). Allocates.
    pub(crate) fn new(participants: usize) -> Self {
        let mut logs = Self {
            lanes: Vec::new(),
            claims: ClaimTable::new(0),
            gate: LogGate::off(),
            fresh: Vec::new(),
        };
        logs.size(participants);
        logs
    }

    /// Replaces the lanes with `participants` new ones. The old rings'
    /// readers not yet taken stay in the hand-off, so what they hold is
    /// still read. Control side; allocates.
    pub(crate) fn size(&mut self, participants: usize) {
        let n = participants.max(1);
        self.lanes = (0..n)
            .map(|_| {
                let (lane, reader) = LogLane::new();
                self.fresh.push(reader);
                lane
            })
            .collect();
        self.claims = ClaimTable::new(n);
    }

    /// Loads the gate once for the block: every lane's slots take it.
    #[inline]
    pub(crate) fn begin_block(&mut self) {
        let filter = self.gate.get();
        for lane in &mut self.lanes {
            // What a node left when it panicked mid-call belongs to no one:
            // the losses are counted, the records dropped.
            lane.slots.discard(&mut lane.sink);
            lane.slots.set_filter(filter);
        }
    }

    pub(crate) fn gate(&self) -> &LogGate {
        &self.gate
    }

    pub(crate) fn take_readers(&mut self) -> Vec<RtLogReader> {
        std::mem::take(&mut self.fresh)
    }
}

impl std::fmt::Debug for Logs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logs")
            .field("lanes", &self.lanes.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(filter: LogFilter) -> LogSlots {
        let s = LogSlots::new();
        s.set_filter(filter);
        s
    }

    const ALL: LogFilter = LogFilter::new(Some(RtLevel::Trace), Some(RtLevel::Trace));

    fn rec(message: &'static str, v: i64) -> RtRecord {
        RtRecord::new(
            RtLevel::Warn,
            RtTarget::Rt,
            message,
            &[("channel", RtValue::I64(v))],
        )
    }

    #[test]
    fn a_line_is_cut_at_a_character_boundary_within_the_capacity() {
        // 95 ASCII bytes then a two-byte character straddling the 96-byte limit.
        let text = format!("{}é tail", "a".repeat(95));
        let r = RtRecord::line(RtLevel::Info, RtTarget::Plugin, &text);
        assert_eq!(r.text(), Some("a".repeat(95).as_str()));
    }

    #[test]
    fn a_line_within_the_capacity_is_kept_whole() {
        let r = RtRecord::line(RtLevel::Info, RtTarget::Plugin, "héllo");
        assert_eq!(r.text(), Some("héllo"));
    }

    #[test]
    fn a_float_field_equals_itself_by_bits_even_as_a_nan() {
        assert_eq!(RtValue::F64(f64::NAN), RtValue::F64(f64::NAN));
        assert_ne!(RtValue::F64(0.0), RtValue::F64(-0.0));
    }

    #[test]
    fn the_same_site_and_values_add_to_one_slots_repeats() {
        let s = slots(ALL);
        static SITE: &str = "non-finite sample";
        for _ in 0..3 {
            s.push(rec(SITE, 1), 7);
        }
        assert_eq!(s.len.get(), 1);
        assert_eq!(s.slots[0].get().repeats(), 2);
    }

    #[test]
    fn a_differing_field_value_takes_another_slot() {
        let s = slots(ALL);
        static SITE: &str = "non-finite sample";
        s.push(rec(SITE, 1), 0);
        s.push(rec(SITE, 2), 0);
        assert_eq!(s.len.get(), 2);
        assert_eq!(s.slots[1].get().repeats(), 0);
    }

    #[test]
    fn a_record_with_text_never_coalesces() {
        let s = slots(ALL);
        let line = RtRecord::line(RtLevel::Warn, RtTarget::Plugin, "same line");
        s.push(line, 0);
        s.push(line, 0);
        assert_eq!(s.len.get(), 2);
    }

    #[test]
    fn a_fifth_distinct_record_is_counted_lost_and_a_repeat_of_a_held_one_is_not() {
        let s = slots(ALL);
        static SITE: &str = "x";
        for v in 0..5 {
            s.push(rec(SITE, v), 0);
        }
        assert_eq!((s.len.get(), s.lost.get()), (4, 1));
        s.push(rec(SITE, 0), 0);
        assert_eq!((s.len.get(), s.lost.get()), (4, 1));
        assert_eq!(s.slots[0].get().repeats(), 1);
    }

    #[test]
    fn the_same_site_at_another_level_or_target_takes_another_slot() {
        static SITE: &str = "site";
        let s = slots(ALL);
        let at = |level, target| RtRecord::new(level, target, SITE, &[]);
        s.push(at(RtLevel::Warn, RtTarget::Rt), 0);
        s.push(at(RtLevel::Error, RtTarget::Rt), 0);
        s.push(at(RtLevel::Warn, RtTarget::Plugin), 0);
        assert_eq!(s.len.get(), 3);
    }

    #[test]
    fn equal_fields_at_another_message_address_take_another_slot() {
        // Two statics of different text: the compiler cannot merge them.
        static ONE: &str = "one";
        static TWO: &str = "two";
        let s = slots(ALL);
        s.push(rec(ONE, 1), 0);
        s.push(rec(TWO, 1), 0);
        assert_eq!(s.len.get(), 2);
    }

    #[test]
    fn an_empty_line_is_a_line_that_never_coalesces() {
        let s = slots(ALL);
        let empty = RtRecord::line(RtLevel::Info, RtTarget::Plugin, "");
        assert_eq!(empty.text(), Some(""));
        s.push(empty, 0);
        s.push(empty, 0);
        assert_eq!(s.len.get(), 2);
    }

    #[test]
    fn a_new_block_drops_what_a_panicked_node_left_and_keeps_its_counted_losses() {
        let mut logs = Logs::new(1);
        logs.gate().set(ALL);
        logs.begin_block();
        let lane = &logs.lanes[0];
        for v in 0..5 {
            lane.slots.push(rec("x", v), 0);
        }
        // The node panicked here: nothing flushed. The next block begins.
        logs.begin_block();
        let lane = &mut logs.lanes[0];
        assert!(
            lane.slots.is_idle(),
            "nothing is left to flush under the next node"
        );
        let mut readers = logs.take_readers();
        assert_eq!(
            readers[0].lost(),
            1,
            "the fifth record's loss was counted first"
        );
        assert!(readers[0].pop().is_none());
    }

    #[test]
    fn a_record_below_its_targets_level_writes_nothing() {
        let s = slots(LogFilter::new(Some(RtLevel::Error), None));
        s.push(rec("x", 0), 0);
        s.push(RtRecord::line(RtLevel::Error, RtTarget::Plugin, "p"), 0);
        assert!(s.is_idle());
    }

    #[test]
    fn the_filter_gates_each_target_by_its_own_level() {
        let f = LogFilter::new(Some(RtLevel::Warn), Some(RtLevel::Trace));
        assert!(f.allows(RtTarget::Rt, RtLevel::Warn));
        assert!(!f.allows(RtTarget::Rt, RtLevel::Info));
        assert!(f.allows(RtTarget::Plugin, RtLevel::Trace));
        assert_eq!(f.level(RtTarget::Rt), Some(RtLevel::Warn));
        assert_eq!(LogFilter::OFF.level(RtTarget::Plugin), None);
    }
}
