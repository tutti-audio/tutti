//! The lane pool the stream assemblers share: [`SysexAssembler`](crate::SysexAssembler),
//! [`Sysex8Assembler`](crate::Sysex8Assembler), [`FlexTextAssembler`](crate::FlexTextAssembler)
//! and [`MixedDataAssembler`](crate::MixedDataAssembler).

use crate::{MidiGroup, Ump};

/// The most Groups a UMP stream has (M2-104 §2.1.2), so the most slots a pool
/// has and the most lanes it lends.
pub(crate) const MAX_LANES: usize = 16;

/// A run of the stream `key` (a SysEx8 Stream ID, an MDS id) of a Group: the slot of an assembler keyed by
/// stream.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry<K, Q> {
    pub(crate) group: MidiGroup,
    pub(crate) key: K,
    pub(crate) run: Q,
}

/// A caller's buffer cut into `lanes` slots of `lane_len` bytes at its end,
/// sixteen run slots, and the count of messages dropped whole.
///
/// A slot is a lane's entry for an assembler keyed by stream (the slot index
/// is the lane), or a Group's for one keyed by Group (the run names its lane).
/// The bytes before the lanes are the owner's.
#[derive(Debug)]
pub(crate) struct LanePool<B, R> {
    buf: B,
    base: usize,
    lane_len: usize,
    lanes: usize,
    slots: [Option<R>; MAX_LANES],
    /// Whether a run is collecting a message, so ending it loses the message;
    /// a run skipping a message already dropped (and counted) is not.
    assembling: fn(&R) -> bool,
    dropped: u64,
}

impl<B: AsRef<[u8]> + AsMut<[u8]>, R: Copy> LanePool<B, R> {
    /// A pool over `buf` of up to `lanes` lanes (at most sixteen) of
    /// `lane_len` bytes: as many as `buf` holds.
    pub(crate) fn new(buf: B, lane_len: usize, lanes: usize, assembling: fn(&R) -> bool) -> Self {
        let room = buf.as_ref().len();
        let fit = room.checked_div(lane_len).unwrap_or(0);
        let lanes = lanes.min(MAX_LANES).min(fit);
        Self {
            buf,
            base: room - lanes * lane_len,
            lane_len,
            lanes,
            slots: [None; MAX_LANES],
            assembling,
            dropped: 0,
        }
    }

    /// Slot `i`'s run.
    pub(crate) const fn slot(&self, i: usize) -> Option<R> {
        self.slots[i]
    }

    /// Slot `i` holds `run`.
    pub(crate) fn set(&mut self, i: usize, run: R) {
        self.slots[i] = Some(run);
    }

    /// Slot `i`'s run is over with its message handed on or skipped to its
    /// end: nothing is counted.
    pub(crate) fn clear(&mut self, i: usize) {
        self.slots[i] = None;
    }

    /// Slot `i`'s run, leaving the slot empty.
    pub(crate) fn take(&mut self, i: usize) -> Option<R> {
        self.slots[i].take()
    }

    /// A message was dropped whole.
    pub(crate) fn count_drop(&mut self) {
        self.dropped += 1;
    }

    /// Messages dropped whole.
    pub(crate) const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The length of a lane, which is the longest message the pool holds.
    pub(crate) const fn lane_len(&self) -> usize {
        self.lane_len
    }

    /// The buffer's length before the lanes.
    pub(crate) const fn base(&self) -> usize {
        self.base
    }

    /// The whole buffer.
    pub(crate) fn buf(&self) -> &[u8] {
        self.buf.as_ref()
    }

    /// The whole buffer, writable.
    pub(crate) fn buf_mut(&mut self) -> &mut [u8] {
        self.buf.as_mut()
    }

    /// Where lane `lane` starts in the buffer.
    pub(crate) const fn lane_start(&self, lane: usize) -> usize {
        self.base + lane * self.lane_len
    }

    /// Lane `lane`.
    pub(crate) fn lane(&self, lane: usize) -> &[u8] {
        &self.buf.as_ref()[self.lane_start(lane)..][..self.lane_len]
    }

    /// Lane `lane`, writable.
    pub(crate) fn lane_mut(&mut self, lane: usize) -> &mut [u8] {
        let start = self.lane_start(lane);
        &mut self.buf.as_mut()[start..][..self.lane_len]
    }

    /// The first slot below the lane count that holds a run `matches` accepts.
    pub(crate) fn find(&self, matches: impl Fn(&R) -> bool) -> Option<usize> {
        (0..self.lanes).find(|&i| self.slots[i].as_ref().is_some_and(&matches))
    }

    /// The first slot below the lane count that is empty.
    pub(crate) fn free(&self) -> Option<usize> {
        (0..self.lanes).find(|&i| self.slots[i].is_none())
    }

    /// The first lane no run holds, `held` naming the lane a run holds if it
    /// holds one.
    pub(crate) fn free_lane(&self, held: impl Fn(&R) -> Option<usize>) -> Option<usize> {
        (0..self.lanes).find(|&lane| {
            !self
                .slots
                .iter()
                .flatten()
                .any(|run| held(run) == Some(lane))
        })
    }

    /// Slot `i`'s run ends without its final packet: counted if it was
    /// collecting a message.
    pub(crate) fn end(&mut self, i: usize) {
        if self.slots[i].as_ref().is_some_and(self.assembling) {
            self.dropped += 1;
        }
        self.slots[i] = None;
    }

    /// `ump` is not a packet of the stream: it ends its Group's run, unless it
    /// is a System Real Time message, which may sit inside a message (M2-104
    /// §7.7.1; a Flex text message takes a JR Clock message too, §7.5.2, which
    /// has no Group and ends nothing here).
    pub(crate) fn interrupt(&mut self, ump: Ump) {
        if let Some(group) = ump.group()
            && !ump.is_system_real_time()
        {
            self.end(group.index());
        }
    }

    /// Every run ends: an upstream queue dropped what followed the packets
    /// just pushed, or the consumer was reset.
    pub(crate) fn lost_tail(&mut self) {
        for i in 0..MAX_LANES {
            self.end(i);
        }
    }
}

impl<B: AsRef<[u8]> + AsMut<[u8]>, K: Copy + PartialEq, Q: Copy> LanePool<B, Entry<K, Q>> {
    /// The lane of the stream `key` of `group`.
    pub(crate) fn find_key(&self, group: MidiGroup, key: K) -> Option<usize> {
        self.find(|e| e.group == group && e.key == key)
    }

    /// Lane `i` holds `run` of the stream `key` of `group`.
    pub(crate) fn set_key(&mut self, i: usize, group: MidiGroup, key: K, run: Q) {
        self.slots[i] = Some(Entry { group, key, run });
    }
}
