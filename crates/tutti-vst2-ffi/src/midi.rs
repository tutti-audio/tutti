//! MIDI wire codec between a UMP (`tutti_ump::Ump`) and `crate::api::MidiEvent`.
//!
//! VST2's on-the-wire format is plain 3-byte MIDI-1 plus a `delta_frames`
//! sample offset: `tutti_ump::Midi1Parser` reads it and
//! `tutti_midi_protocol::midi1` writes it, so the event-level helpers are
//! thin structural conversions.
//!
//! The flexible-array `api::Events` header has no safe constructor in
//! the `vst` crate, so [`MidiSendBuffer`] hand-assembles it into an
//! aligned `[u64]`. The buffer's storage is fixed at construction and reused across
//! calls, so the audio thread never allocates.
//!
//! SysEx, which has no 3-byte form, goes to the plugin as a `kVstSysExType`
//! event: [`MidiSendBuffer`] reassembles the UMP SysEx7 packets
//! ([`SysexAssembler`]) and hands the plugin one complete `0xF0 … 0xF7`
//! message at its `END` packet's frame. A message may span blocks; one past
//! [`SYSEX_MESSAGE_CAPACITY`](tutti_plugin_types::SYSEX_MESSAGE_CAPACITY) is
//! dropped whole and counted.

use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use tutti_graph::Offset;
use tutti_graph::UmpAt;
use tutti_midi_protocol::{Midi1Translator, Midi2Translator};
use tutti_ump::{Message, MidiGroup, Ump, parse_exact};

use crate::host::RenderHandle;
use tutti_plugin_types::{MIDI_STACK_CAPACITY, MidiEventVec, WireMidi, sysex_assembler};
use tutti_rt::{Counter, CounterWriter, RtVec, Tally, TallyWriter, counter};
use tutti_ump::{SysexAssembler, SysexRef, SysexStep};

/// A count of lost items: the audio-side writer, stored, and the reader the control side
/// takes. Minted together on the control side.
struct Drops {
    writer: CounterWriter<u64>,
    reader: Counter<u64>,
}

impl Drops {
    fn new() -> Self {
        let (writer, reader) = counter();
        Self { writer, reader }
    }

    fn add(&mut self, by: u64) {
        self.writer.add(by);
    }
}

/// Per-block MIDI plumbing for one `Vst2Processor` (in `tutti-vst2-host`): the host→plugin
/// staging buffer, the plugin→host inbox, and the pooled out-drain returned
/// to callers. All RT-reused so steady-state processing never allocates.
pub struct MidiIo {
    /// Host→plugin staging: rebuilt in place each `process` call.
    pub send: MidiSendBuffer,
    /// Plugin→host inbox, fed by the `process_events` callback.
    ///
    /// Bounded: the callback runs on the audio thread, so the push must
    /// neither allocate nor grow. See `host.rs`'s *Why the queues are
    /// bounded* for the drop policy and the counter that reports it.
    pub out_rx: Arc<ArrayQueue<WireMidi>>,
    /// Pooled drain of `out_rx`, refilled and borrowed back each block. A block's pieces all
    /// drain into it, so a split block could ask for more than it holds: the rest is counted
    /// as overflow.
    pub out: MidiEventVec,
    /// Events dropped because `out` was full, since load. A tally the host's own drops also
    /// add to.
    out_overflow: TallyWriter<u64>,
    /// Translates the plugin's MIDI 1.0 output into UMP, keeping its
    /// parameter-number and bank runs across blocks: VST2 has one output
    /// stream, so one translator.
    translator: Midi2Translator,
}

impl std::fmt::Debug for MidiIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiIo")
            .field("send", &self.send)
            .field("out", &self.out.len())
            .finish_non_exhaustive()
    }
}

impl MidiIo {
    /// Constructs with the inbox queue; both reusable buffers are fixed at construction so
    /// the audio thread never allocates.
    ///
    /// `out_overflow` is the tally the events refused for want of room are added to.
    pub fn new(out_rx: Arc<ArrayQueue<WireMidi>>, out_overflow: &Tally<u64>) -> Self {
        Self {
            send: MidiSendBuffer::new(),
            out_rx,
            out: MidiEventVec::new(),
            out_overflow: out_overflow.writer(),
            translator: Midi2Translator::new(),
        }
    }
}

impl MidiIo {
    /// Forgets the output's parameter-number and bank runs, sending nothing: the
    /// plugin's output has a discontinuity (a reset, a deactivation).
    pub fn reset_out(&mut self) {
        self.translator.reset();
    }

    /// Moves everything in the plugin→host inbox into [`out`](Self::out), in
    /// order, each MIDI 1.0 message through the output's translator (a message
    /// that ends no run is absorbed), for the piece of a block that starts `start` frames into it: the
    /// plugin's offsets are relative to its call, the returned ones to the
    /// block. Allocation-free: `out` is a fixed-capacity list, and what does not fit is counted. A
    /// parameter run (select, Data Entry MSB) with no LSB waits for one: the
    /// translator never flushes a live output, which has no end (M2-104
    /// Appendix D.3.3).
    pub fn drain_from(&mut self, start: usize) {
        let start = u32::try_from(start).unwrap_or(u32::MAX);
        while let Some(ev) = self.out_rx.pop() {
            // Translated before the room check: the translator sees every packet
            // of the stream, so a run whose data entry arrives while the list is
            // full still assembles for the block after.
            let Some(ump) = self.translator.translate(ev.ump) else {
                continue;
            };
            // Never past the list's capacity: a split block drains once a piece.
            let event = WireMidi {
                offset: ev.offset.saturating_add(start),
                ump,
            };
            if self.out.try_push(event).is_err() {
                self.out_overflow.add(1);
            }
        }
    }
}

/// Parses a VST2 `crate::api::MidiEvent` (MIDI 1.0 wire bytes) into a MIDI 1.0
/// [`WireMidi`], tagged with the original `delta_frames`. The packet stays
/// MIDI 1.0: [`MidiIo::drain_from`] runs it through the output's translator,
/// which needs the stream in order (RPN, NRPN and Bank Select span messages).
pub fn to_midi(event: &crate::api::MidiEvent) -> Option<WireMidi> {
    let frame = event.delta_frames.max(0) as u32;
    parse_exact(MidiGroup::FIRST, &event.midi_data).map(|message| WireMidi {
        offset: frame,
        ump: Ump::from(message),
    })
}

/// Serializes a Tutti UMP [`WireMidi`] to a VST2 `crate::api::MidiEvent`.
///
/// MIDI 1.0 boundary. VST2 is a MIDI-1-only host: `midi1_of` translates a MIDI
/// 2.0 channel voice message with the spec's Min-Center-Max downscale
/// (`tutti_midi_protocol::midi1`), so no precision is lost beyond MIDI-1's
/// inherent width. Translated: NoteOn/Off, CC, channel pitch bend,
/// channel/poly pressure, program change (everything with a 3-byte MIDI-1
/// form). SysEx is not a `api::MidiEvent` in VST2: `MidiSendBuffer::stage_piece`
/// reassembles it into a `SysExEvent`.
/// Returns `None` when the packet is not exactly one MIDI 1.0 message, on a
/// stream that has sent nothing before it: per-note pitch bend, per-note
/// controllers and management (no MIDI-1 analogue), a Program Change with a
/// bank (three messages) and an RPN or NRPN (four). [`MidiSendBuffer`] sends all
/// of them, through a [`Midi1Translator`] that keeps the stream's state.
pub fn from_midi(event: &WireMidi) -> Option<crate::api::MidiEvent> {
    let mut messages = [[0u8; 3]; MAX_MESSAGES];
    match midi1_messages(&mut Midi1Translator::new(), event.ump, &mut messages) {
        1 => Some(midi_event(event.offset, messages[0], true)),
        _ => None,
    }
}

/// The most MIDI 1.0 messages one UMP stands for: an RPN or NRPN's select and
/// data.
const MAX_MESSAGES: usize = 4;

/// The MIDI 1.0 messages `ump` stands for through `midi1_translator`, in send order, into
/// `out`; how many. None when it has no MIDI 1.0 form (a per-note controller, a
/// relative controller) or does not decode.
fn midi1_messages(
    midi1_translator: &mut Midi1Translator,
    ump: Ump,
    out: &mut [[u8; 3]; MAX_MESSAGES],
) -> usize {
    let mut n = 0;
    if let Ok(message) = ump.decode() {
        midi1_translator.feed(message, |bytes| {
            if n < MAX_MESSAGES {
                // VST2's `midi_data` is a fixed `[u8; 3]`: a short message is zero-padded.
                out[n] = bytes.padded();
                n += 1;
            }
        });
    }
    n
}

/// One MIDI-1 message as a VST2 event at `frame_offset`.
fn midi_event(frame_offset: u32, midi_data: [u8; 3], live: bool) -> crate::api::MidiEvent {
    use crate::api;
    use std::mem;

    // A message with no 3-byte form (SysEx) never reaches here: the MIDI 1.0 translator has
    // none for it, and `stage_piece` sends SysEx as a `SysExEvent`.

    // `frame_offset` is `u32`; VST2's `delta_frames` is `i32`. A value past
    // `i32::MAX` wraps negative, and the plugin offsets into its own buffers
    // with it — an out-of-bounds access inside the plugin, out of reach of any
    // check on this side. Saturating keeps it in range; the inbound mirror
    // (`to_midi`) guards the same hazard with `.max(0)`.
    let delta_frames = i32::try_from(frame_offset).unwrap_or(i32::MAX);

    api::MidiEvent {
        event_type: api::EventType::Midi,
        byte_size: mem::size_of::<api::MidiEvent>() as i32,
        delta_frames,
        // `kVstMidiEventIsRealtime` marks an event the host plays live. It is set for a block
        // rendered in realtime and clear for an offline render. The engine's events do not
        // say whether they are live input or sequenced playback, so within a realtime block
        // every event is marked.
        flags: if live {
            api::MidiEventFlags::REALTIME_EVENT.bits()
        } else {
            0
        },
        note_length: 0,
        note_offset: 0,
        midi_data,
        _midi_reserved: 0,
        detune: 0,
        note_off_velocity: 0,
        _reserved1: 0,
        _reserved2: 0,
    }
}

/// Pre-allocated, reusable storage for a VST2 `api::Events` flexible-
/// array struct.
///
/// One instance lives on each [`MidiIo`]; `Self::stage_piece`
/// rebuilds the contents in place for the next process call. Every list holds a block's
/// whole MIDI cap ([`MIDI_STACK_CAPACITY`]: no caller hands it more) in a fixed
/// allocation made at construction: an event past it is refused and counted
/// ([`events_dropped`](Self::events_dropped)), so staging never allocates.
pub struct MidiSendBuffer {
    /// Whether the events staged are marked `kVstMidiEventIsRealtime`: true for a block
    /// rendered in realtime, false for an offline render.
    live: bool,
    /// Events refused past `room` since the buffer was made.
    events_dropped: Drops,
    /// Plugin-side `api::MidiEvent` storage (the structs the pointer
    /// table points at).
    api_events: Box<RtVec<crate::api::MidiEvent, MIDI_STACK_CAPACITY>>,
    /// Reassembled SysEx messages, whose `system_data` points into
    /// `sysex`'s buffer.
    sysex_events: Box<RtVec<crate::api::SysExEvent, MIDI_STACK_CAPACITY>>,
    /// The block's events in staging order, as indices into `api_events`
    /// or `sysex_events`: the pointer table is built from it once every
    /// event is in place.
    order: Box<RtVec<Staged, MIDI_STACK_CAPACITY>>,
    /// Reassembles SysEx7 packets and owns the bytes a `SysExEvent` points
    /// at. Reserved at construction; an open message carries across blocks.
    sysex: SysexAssembler<Box<[u8]>>,
    /// The MIDI 2.0 to MIDI 1.0 state of what the plugin has been sent: which RPN,
    /// NRPN and bank each channel has had selected. VST2 has one MIDI input, so
    /// only Group 0's channels are in it.
    midi1_translator: Midi1Translator,
    /// Channel voice messages of a Group other than 0, which VST2's one MIDI
    /// input cannot tell from Group 0's.
    groups_dropped: Drops,
    /// SysEx messages the assembler dropped whole, as its own count last stood at the
    /// block's staging or a reset.
    sysex_dropped: Drops,
    /// The assembler's count already added to `sysex_dropped`.
    sysex_seen: u64,
    /// The SysEx messages the block completed, each with the index of its
    /// `END` packet and the frame (clamped to the block) it lands on, filled once at the block's first piece and
    /// selected from by every piece. A message is assembled once per block: a
    /// run is open across blocks, not pieces.
    completed: Box<RtVec<(usize, usize, SysexRef), MIDI_STACK_CAPACITY>>,
    /// Backing storage for the `api::Events` header + flexible-array
    /// tail, with one `*mut api::Event` per event. `u64`s for 8-byte alignment
    /// (`api::Events` requires `isize` alignment).
    header: Box<[u64]>,
}

impl std::fmt::Debug for MidiSendBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiSendBuffer")
            .field("events_dropped", &self.events_dropped.reader.get())
            .finish_non_exhaustive()
    }
}

/// Where one staged event lives.
#[derive(Clone, Copy)]
enum Staged {
    Midi(usize),
    SysEx(usize),
}

// SAFETY: the raw pointers inside `header` point into this struct's own boxed
// `api_events` / `sysex_events`, and the ones in `sysex_events` into
// `sysex`'s buffer, so moving the whole struct moves what they point at with it
// (a box's heap block does not move). Not `Sync`: nothing shares one.
unsafe impl Send for MidiSendBuffer {}

impl Default for MidiSendBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiSendBuffer {
    /// A buffer for a block's whole MIDI cap: every event a plugin node
    /// delivers (out of process, the wire adapter refuses a longer list). Control thread:
    /// allocates.
    pub fn new() -> Self {
        Self {
            live: true,
            events_dropped: Drops::new(),
            api_events: RtVec::boxed(),
            sysex_events: RtVec::boxed(),
            order: RtVec::boxed(),
            sysex: sysex_assembler(),
            midi1_translator: Midi1Translator::new(),
            groups_dropped: Drops::new(),
            sysex_dropped: Drops::new(),
            sysex_seen: 0,
            completed: RtVec::boxed(),
            header: vec![0; header_words(MIDI_STACK_CAPACITY)].into_boxed_slice(),
        }
    }

    /// Stage the events of `midi_events` that fall in the piece of a block
    /// `start..start + len` (of `total` frames) into the reusable buffers and
    /// return a raw pointer to the populated `api::Events` header. Never
    /// allocates: an event past the buffer's room is refused and counted.
    ///
    /// A block's pieces are staged in order, the first (`start == 0`) first:
    /// it runs every SysEx packet of the block through the assembler once, so
    /// a message is never spliced and one whose `END` falls in a later piece
    /// is delivered in that piece.
    ///
    /// What the plugin is handed is the VST2 contract, enforced here rather
    /// than trusted of the caller: each event's `deltaFrames` is clamped to
    /// the block (`0..total`, as JUCE clamps with `jlimit(0, numSamples - 1,
    /// ..)`), rebased to the piece, and the events are delivered in
    /// `deltaFrames` order (stable among equals), whatever order they came in.
    ///
    /// Returns `None` if no events convert successfully (in which case
    /// the caller skips dispatch entirely).
    ///
    /// # Panics
    /// Never in practice: the one assertion is on the header's size, which
    /// `new` fixed for a block's whole MIDI cap.
    ///
    /// # Safety
    /// The returned pointer is valid until the next
    /// `Self::stage_piece` call. Caller must hand it to
    /// the plugin's `effProcessEvents` and not retain it past
    /// that call.
    fn stage_piece(
        &mut self,
        midi_events: &[UmpAt],
        start: usize,
        len: usize,
        total: usize,
    ) -> Option<*mut crate::api::Events> {
        use crate::api;

        if midi_events.is_empty() {
            return None;
        }

        // The frame, in this piece, an event at `offset` of the block lands on;
        // `None` for one that belongs to another piece. The clamp is to the
        // block, so an event past its end lands on its last frame, in the last
        // piece.
        let last = total.saturating_sub(1);
        let delta_in_piece = |offset: Offset| {
            let at = offset.index().min(last);
            (start..start + len)
                .contains(&at)
                .then(|| (at - start) as i32)
        };

        // Reuse the storage. The last block's SysEx bytes are released here: the plugin had to
        // copy them before its `process_events` returned.
        self.api_events.clear();
        self.sysex_events.clear();
        self.order.clear();
        // The block's SysEx is assembled once, at its first piece: every packet
        // reaches the assembler exactly once, in order, so a message whose
        // `END` falls in a later piece is still completed (and a message left
        // open at the block's end is counted dropped at most once). The later
        // pieces select the completed messages that fall in them.
        if start == 0 {
            self.sysex.begin_block();
            self.completed.clear();
            for (index, (offset, ump)) in midi_events.iter().enumerate() {
                if let SysexStep::Complete(message) = self.sysex.push(*ump) {
                    let at = offset.index().min(last);
                    if !self.completed.push((index, at, message)) {
                        self.events_dropped.add(1);
                    }
                }
            }
            self.count_sysex_drops();
        }
        // Staged in input order (a completed SysEx at its `END` packet), so the
        // stable sort below keeps input order at equal frames.
        let mut next_sysex = 0;
        for (index, (offset, ev)) in midi_events.iter().enumerate() {
            // Type 3 is System Exclusive (7-bit) (M2-104 Table 4).
            if ev.message_type() == 0x3 {
                let Some(&(end, at, message)) = self.completed.as_slice().get(next_sysex) else {
                    continue;
                };
                if end != index {
                    continue;
                }
                next_sysex += 1;
                if !(start..start + len).contains(&at) {
                    continue;
                }
                // `order` holds one entry per staged event, so room in it is room in the
                // lists it indexes.
                if self.order.is_full() {
                    self.events_dropped.add(1);
                    continue;
                }
                let event = self.sysex_event((at - start) as i32, message);
                self.order.push(Staged::SysEx(self.sysex_events.len()));
                self.sysex_events.push(event);
                continue;
            }
            let Some(delta) = delta_in_piece(*offset) else {
                continue;
            };
            // VST2 has one MIDI input, which is Group 0's: a channel voice
            // message of another Group names channels this plugin cannot tell
            // from Group 0's (M2-104 section 2.1.2), so it is dropped and counted,
            // never heard on them. A message with no Group's channels (System,
            // Utility) is not a Group's.
            if ev.group().is_some_and(|group| group != MidiGroup::FIRST)
                && matches!((*ev).decode(), Ok(Message::Midi1(..) | Message::Midi2(..)))
            {
                self.groups_dropped.add(1);
                continue;
            }
            // One UMP can be several MIDI-1 messages (a Program Change with a
            // bank is CC 0, CC 32, PC; an RPN or NRPN is CC 101 and 100 or 99
            // and 98, then CC 6 and 38): each takes a slot at the same frame,
            // and all of them fit or none is staged (a bank is not sent
            // without its program).
            let mut messages = [[0u8; 3]; MAX_MESSAGES];
            let n = midi1_messages(&mut self.midi1_translator, *ev, &mut messages);
            if self.order.remaining() < n {
                self.events_dropped.add(n as u64);
                continue;
            }
            for message in &messages[..n] {
                let mut api = midi_event(offset.get(), *message, self.live);
                api.delta_frames = delta;
                self.order.push(Staged::Midi(self.api_events.len()));
                self.api_events.push(api);
            }
        }
        if self.order.is_empty() {
            return None;
        }

        // In `deltaFrames` order, stably: an in-order list (the common case)
        // costs one comparison per event and no moves.
        {
            let (midi, sysex) = (&self.api_events, &self.sysex_events);
            tutti_plugin_types::insertion_sort_by_key(self.order.as_mut_slice(), |staged| {
                match *staged {
                    Staged::Midi(i) => midi[i].delta_frames,
                    Staged::SysEx(i) => sysex[i].delta_frames,
                }
            });
        }

        let num_events = self.order.len();

        // The header holds `api::Events` and one pointer per event; it was sized for a block's
        // whole MIDI cap, which `num_events` does not pass (the refusals above).
        let words = header_words(num_events);
        assert!(words <= self.header.len());
        // Zero only the bytes that will be touched. (The header carries 2 i32s and a pointer
        // table; the rest doesn't matter because the plugin reads `num_events` first.)
        for slot in self.header.iter_mut().take(words) {
            *slot = 0;
        }

        let events_offset = std::mem::offset_of!(api::Events, events);

        // SAFETY: `header` is `[u64]`, so its base is 8-aligned, which
        // is `api::Events`' alignment (its largest field is a pointer);
        // the cast starts from `*mut u64`, not `*mut u8`, so the compiler
        // sees that. The header holds at least `words` u64s (asserted above),
        // enough for the struct and `num_events` table entries, and
        // `&mut self` makes the writes exclusive. Each table entry points at an
        // event in the boxed `api_events` or `sysex_events`, which nothing
        // changes until the next `stage_piece`.
        unsafe {
            let words_ptr = self.header.as_mut_ptr();
            let events = &mut *(words_ptr as *mut api::Events);
            events.num_events = num_events as i32;
            events._reserved = 0;
            let base = words_ptr.byte_add(events_offset) as *mut *mut api::Event;
            // One pointer to each list's storage, so no later borrow of a list
            // invalidates an event pointer already in the table.
            let midi_events = self.api_events.as_mut_slice().as_mut_ptr();
            let sysex_events = self.sysex_events.as_mut_slice().as_mut_ptr();
            for (i, staged) in self.order.iter().enumerate() {
                let ptr = match *staged {
                    Staged::Midi(n) => midi_events.add(n) as *mut api::Event,
                    Staged::SysEx(n) => sysex_events.add(n) as *mut api::Event,
                };
                *base.add(i) = ptr;
            }
            Some(words_ptr as *mut api::Events)
        }
    }
}

impl MidiSendBuffer {
    /// Whether the events staged from now on are marked `kVstMidiEventIsRealtime`: true
    /// while the host renders in realtime (the initial state), false for an offline render.
    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    /// Stages the piece (see `stage_piece`) and hands it to the plugin
    /// through `render`, if any event is in it. Returns how many events it
    /// staged: MIDI 1.0 messages and whole SysEx messages, the unit of
    /// [`events_dropped`](Self::events_dropped).
    pub fn send_piece(
        &mut self,
        render: &mut RenderHandle,
        midi_events: &[UmpAt],
        start: usize,
        len: usize,
        total: usize,
    ) -> usize {
        if let Some(events_ptr) = self.stage_piece(midi_events, start, len, total) {
            // SAFETY: `stage_piece` returns a pointer valid until the next
            // `stage` call or `drop`. It is used immediately and never
            // retained. The VST2 spec requires the plugin to copy any event
            // data it needs before `process_events` returns.
            render.process_events(unsafe { &*events_ptr });
            self.order.len()
        } else {
            0
        }
    }

    /// SysEx messages dropped whole since this buffer was made: longer than
    /// [`SYSEX_MESSAGE_CAPACITY`](tutti_plugin_types::SYSEX_MESSAGE_CAPACITY),
    /// or left without their `END` by a later `START`.
    pub fn sysex_dropped(&self) -> &Counter<u64> {
        &self.sysex_dropped.reader
    }

    /// Adds what the assembler has dropped since the last call to the `sysex_dropped` count.
    fn count_sysex_drops(&mut self) {
        let dropped = self.sysex.dropped();
        self.sysex_dropped
            .add(dropped.saturating_sub(self.sysex_seen));
        self.sysex_seen = dropped;
    }

    /// The plugin was reset: drops a SysEx message open across the reset
    /// (counted in [`sysex_dropped`](Self::sysex_dropped)) and resets the MIDI 1.0 translator.
    /// Never allocates.
    pub fn reset_midi(&mut self) {
        self.sysex.reset();
        self.count_sysex_drops();
        self.midi1_translator.reset();
    }

    /// Channel voice messages dropped since this buffer was made because they
    /// were of a Group other than 0: VST2 has one MIDI input, which takes Group
    /// 0's channels only. Each is dropped whole and counted, never heard on
    /// Group 0's channels (M2-104 section 2.1.2).
    pub fn groups_dropped(&self) -> &Counter<u64> {
        &self.groups_dropped.reader
    }

    /// Events refused since this buffer was made because a block staged more
    /// than its room ([`MIDI_STACK_CAPACITY`]); a refused SysEx is a whole
    /// message.
    pub fn events_dropped(&self) -> &Counter<u64> {
        &self.events_dropped.reader
    }

    /// The `SysExEvent` for a reassembled message, at `end`'s frame.
    fn sysex_event(&self, delta_frames: i32, message: SysexRef) -> crate::api::SysExEvent {
        let bytes = self.sysex.message(message).bytes();
        crate::api::SysExEvent {
            event_type: crate::api::EventType::SysEx,
            byte_size: std::mem::size_of::<crate::api::SysExEvent>() as i32,
            // Already clamped to the block and rebased to the piece.
            delta_frames,
            _flags: 0,
            data_size: bytes.len() as i32,
            _reserved1: 0,
            // The plugin reads, never writes, these bytes; VST2 types the
            // pointer `*mut`. The buffer never grows and nothing in it moves
            // until the next `stage`.
            system_data: bytes.as_ptr().cast_mut(),
            _reserved2: 0,
        }
    }
}

/// Number of `u64` words required to hold a flexible `api::Events`
/// struct with `events` trailing pointers.
fn header_words(events: usize) -> usize {
    let events_offset = std::mem::offset_of!(crate::api::Events, events);
    let needed = events_offset + events * std::mem::size_of::<*mut crate::api::Event>();
    let alloc_size = needed.max(std::mem::size_of::<crate::api::Events>());
    alloc_size.div_ceil(8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::Samples;
    use tutti_ump::Ump;

    /// A plugin-side event at a raw frame (`WireMidi` is unchecked, as `from_midi` reads it).
    fn wire(ump: Ump, offset: u32) -> WireMidi {
        WireMidi { offset, ump }
    }

    /// A block event at frame `k` of a block long enough to hold it.
    fn at(k: usize, ump: Ump) -> UmpAt {
        (Offset::of(Samples::new(k), Samples::new(k + 1)), ump)
    }

    /// **Draining a full inbox does not allocate**: the out-list was reserved
    /// for the inbox's whole capacity when it was built, and is cleared, not
    /// rebuilt, between blocks.
    #[test]
    fn draining_a_full_inbox_does_not_allocate() {
        let inbox = Arc::new(ArrayQueue::new(256));
        let mut io = MidiIo::new(Arc::clone(&inbox), &tutti_rt::tally());
        for _ in 0..3 {
            io.out.clear();
            for i in 0..inbox.capacity() as u32 {
                inbox
                    .push(wire(Ump([0x2090_3C40, 0, 0, 0]), i))
                    .expect("room");
            }
            assert_no_alloc::assert_no_alloc(|| io.drain_from(0));
            assert_eq!(io.out.len(), 256);
            assert_eq!(io.out[255].offset, 255, "in order");
        }
    }
    /// **The translator sees every packet, whether or not the list has room for
    /// what it makes**: an RPN run whose data entry arrives while the list is
    /// full is counted as lost, and the selection it made still holds for the
    /// run after (M2-104 Appendix D.3.3, a selected parameter stays selected).
    #[test]
    fn a_run_through_a_full_list_still_assembles() {
        let cc =
            |controller: u32, value: u32| Ump([0x20B0_0000 | controller << 8 | value, 0, 0, 0]);
        let inbox = Arc::new(ArrayQueue::new(MIDI_STACK_CAPACITY));
        let overflow = tutti_rt::tally();
        let mut io = MidiIo::new(Arc::clone(&inbox), &overflow);
        // The list is full of notes, then the run (select, MSB, LSB) arrives.
        for i in 0..MIDI_STACK_CAPACITY as u32 {
            inbox
                .push(wire(Ump([0x2090_3C40, 0, 0, 0]), i))
                .expect("room");
        }
        io.drain_from(0);
        assert_eq!(io.out.len(), MIDI_STACK_CAPACITY);
        for packet in [cc(101, 0), cc(100, 0), cc(6, 12), cc(38, 0)] {
            inbox.push(wire(packet, 0)).expect("room");
        }
        io.drain_from(0);
        assert_eq!(
            io.out.len(),
            MIDI_STACK_CAPACITY,
            "no room for the controller"
        );
        assert_eq!(overflow.get(), 1, "which is counted");
        // The next block: RPN 0/0 is still selected, so a new value is one
        // Registered Controller (7 semitones, zero-extended, M2-115 4.1).
        io.out.clear();
        for packet in [cc(6, 7), cc(38, 0)] {
            inbox.push(wire(packet, 0)).expect("room");
        }
        io.drain_from(0);
        let [event] = io.out[..] else {
            panic!("expected one controller, got {:?}", io.out);
        };
        let Ok(Message::Midi2(_, tutti_ump::Voice2::Registered { value, .. })) = event.ump.decode()
        else {
            panic!(
                "expected a Registered Controller, got {:?}",
                event.ump.decode()
            );
        };
        assert_eq!(value.get(), 7 << 25);
    }

    /// **A split block's pieces cannot grow the MIDI-out list**: each piece
    /// drains a full inbox, the list holds one inbox's worth, and what does not
    /// fit is counted, not pushed. Inside the no-alloc gate.
    #[test]
    fn a_split_block_with_full_inboxes_does_not_grow_the_out_list() {
        let inbox = Arc::new(ArrayQueue::new(256));
        let overflow = tutti_rt::tally();
        let mut io = MidiIo::new(Arc::clone(&inbox), &overflow);
        for _ in 0..2 {
            io.out.clear();
            for piece in 0..2usize {
                for i in 0..inbox.capacity() as u32 {
                    inbox
                        .push(wire(Ump([0x2090_3C40, 0, 0, 0]), i))
                        .expect("room");
                }
                assert_no_alloc::assert_no_alloc(|| io.drain_from(piece * 64));
            }
            assert_eq!(io.out.len(), 256, "capped at the reservation");
        }
        assert_eq!(overflow.get(), 2 * 256, "the second piece's, counted");
    }

    use tutti_fixtures::midi;

    use tutti_ump::{
        Bank, BendValue, ControlValue, NoteVelocity, Rpn, Sysex7Packets, System, U7, U14,
    };
    use tutti_ump::{CCNumber, MidiChannel, MidiGroup};

    /// Spec: vst2.event.kVstMidiType
    #[test]
    fn note_on_off_roundtrip() {
        let event = wire(
            midi::note_on(
                MidiChannel::of(1),
                60,
                NoteVelocity::from(U7::of(127)).get(),
            ),
            10,
        );
        let api = from_midi(&event).expect("NoteOn should convert");
        assert_eq!(api.midi_data[0], 0x91);
        assert_eq!(api.midi_data[1], 60);
        assert_eq!(api.midi_data[2], 127);
        assert_eq!(api.delta_frames, 10);

        let event = wire(
            midi::note_off(MidiChannel::of(9), 48, NoteVelocity::from(U7::of(64)).get()),
            0,
        );
        let api = from_midi(&event).expect("NoteOff should convert");
        assert_eq!(api.midi_data[0], 0x80 | 9);
        assert_eq!(api.midi_data[1], 48);
        assert_eq!(api.midi_data[2], 64);
    }

    /// A frame offset past `i32::MAX` must saturate, not wrap negative.
    ///
    /// `WireMidi::offset` is `u32` and VST2's `delta_frames` is `i32`, so
    /// a bare `as i32` turns a large offset into a negative one. The plugin
    /// offsets into its own buffers with that value, so the out-of-bounds access
    /// happens inside the plugin where nothing here can catch it.
    ///
    /// The inbound mirror (`to_midi`) already clamped with `.max(0)`, which is
    /// what makes the unguarded direction quiet: a wrapped offset that survives
    /// a round trip comes back as frame 0 rather than as anything a caller could
    /// recognise as wrong.
    #[test]
    fn a_huge_frame_offset_saturates_rather_than_going_negative() {
        let event = wire(
            midi::note_on(
                MidiChannel::FIRST,
                60,
                NoteVelocity::from(U7::of(100)).get(),
            ),
            u32::MAX,
        );
        let api = from_midi(&event).expect("NoteOn should convert");
        assert!(
            api.delta_frames >= 0,
            "delta_frames went negative ({}) — the plugin will offset into \
             its buffers out of bounds",
            api.delta_frames
        );
        assert_eq!(
            api.delta_frames,
            i32::MAX,
            "an unrepresentable offset must saturate"
        );

        // A wrap would be laundered by the inbound clamp: `to_midi` maps a
        // negative `delta_frames` to 0, so a round trip would report frame 0
        // for an event scheduled at the end of time. Saturating keeps it there.
        let back = to_midi(&api).expect("the saturated event must parse back");
        assert_eq!(back.offset, i32::MAX as u32);
    }

    #[test]
    fn cc_roundtrip() {
        let event = wire(
            midi::cc(
                MidiChannel::of(1),
                CCNumber::BRIGHTNESS,
                ControlValue::from(U7::of(100)).get(),
            ),
            0,
        );
        let api = from_midi(&event).expect("CC should convert");
        assert_eq!(api.midi_data[0], 0xB1);
        assert_eq!(api.midi_data[1], 74);
        assert_eq!(api.midi_data[2], 100);
    }

    #[test]
    fn pitch_bend_roundtrip() {
        let event = wire(
            midi::pitch_bend(MidiChannel::of(1), BendValue::from(U14::of(8192)).get()),
            0,
        );
        let api = from_midi(&event).expect("PitchBend should convert");
        assert_eq!(api.midi_data[0], 0xE1);
        let bend14 = (api.midi_data[1] as u16) | ((api.midi_data[2] as u16) << 7);
        assert!(
            (bend14 as i32 - 8192).abs() <= 1,
            "center 8192 roundtrip got {}",
            bend14
        );

        let event = wire(
            midi::pitch_bend(MidiChannel::of(1), BendValue::from(U14::of(0)).get()),
            0,
        );
        let api = from_midi(&event).expect("PitchBend min should convert");
        assert_eq!(api.midi_data[1], 0x00);
        assert_eq!(api.midi_data[2], 0x00);

        let event = wire(
            midi::pitch_bend(MidiChannel::of(1), BendValue::from(U14::of(16383)).get()),
            0,
        );
        let api = from_midi(&event).expect("PitchBend max should convert");
        assert_eq!(api.midi_data[1], 0x7F);
        assert_eq!(api.midi_data[2], 0x7F);
    }

    #[test]
    fn program_change_roundtrip() {
        let event = wire(midi::program_change(MidiChannel::of(1), 42, None), 0);
        let api = from_midi(&event).expect("ProgramChange should convert");
        assert_eq!(api.midi_data[0], 0xC1);
        assert_eq!(api.midi_data[1], 42);
        assert_eq!(api.midi_data[2], 0);
    }

    /// `from_midi` is one event: a Program Change with a bank is three, so it
    /// has none (`stage_piece` sends the three), and a MIDI 2.0 Note On at
    /// velocity 0 goes out at velocity 1, not as a MIDI 1.0 Note Off (M2-104).
    #[test]
    fn from_midi_is_one_message_or_none() {
        let banked = wire(
            midi::program_change(
                MidiChannel::of(1),
                9,
                Some(Bank {
                    msb: U7::of(1),
                    lsb: U7::of(5),
                }),
            ),
            0,
        );
        assert!(from_midi(&banked).is_none());
        let silent_on = wire(midi::note_on(MidiChannel::of(1), 60, 0), 0);
        let api = from_midi(&silent_on).expect("a Note On converts");
        assert_eq!(api.midi_data, [0x91, 60, 1]);
    }

    #[test]
    fn channel_pressure_roundtrip() {
        let event = wire(
            midi::channel_pressure(MidiChannel::of(1), ControlValue::from(U7::of(100)).get()),
            0,
        );
        let api = from_midi(&event).expect("ChannelPressure should convert");
        assert_eq!(api.midi_data[0], 0xD1);
        assert_eq!(api.midi_data[1], 100);
        assert_eq!(api.midi_data[2], 0);
    }

    #[test]
    fn poly_pressure_roundtrip() {
        let event = wire(
            midi::poly_pressure(MidiChannel::of(1), 60, ControlValue::from(U7::of(80)).get()),
            0,
        );
        let api = from_midi(&event).expect("PolyPressure should convert");
        assert_eq!(api.midi_data[0], 0xA1);
        assert_eq!(api.midi_data[1], 60);
        assert_eq!(api.midi_data[2], 80);
    }

    /// Builds a raw VST2 `api::MidiEvent` from 3 MIDI-1 status/data bytes.
    fn api_from_bytes(data: [u8; 3]) -> crate::api::MidiEvent {
        use crate::api;
        use std::mem;
        api::MidiEvent {
            event_type: api::EventType::Midi,
            byte_size: mem::size_of::<api::MidiEvent>() as i32,
            delta_frames: 0,
            flags: api::MidiEventFlags::REALTIME_EVENT.bits(),
            note_length: 0,
            note_offset: 0,
            midi_data: data,
            _midi_reserved: 0,
            detune: 0,
            note_off_velocity: 0,
            _reserved1: 0,
            _reserved2: 0,
        }
    }

    /// A plugin-emitted MIDI-1 CC arrives as MIDI-2 Channel Voice 2, not
    /// CV1, once the output's translator has it: the engine sees one
    /// vocabulary regardless of source (as the hardware input path's does).
    #[test]
    fn inbound_cc_promotes_to_cv2() {
        use tutti_ump::{Message, Voice2};

        // 0xB1 = CC on channel 1, controller 74, value 100.
        let midi = to_midi(&api_from_bytes([0xB1, 74, 100])).expect("CC decodes");
        let ump = Midi2Translator::new()
            .translate(midi.ump)
            .expect("a CC outside a run is sent");
        match ump.decode().expect("valid UMP") {
            Message::Midi2(address, Voice2::ControlChange { controller, value }) => {
                assert_eq!(address.channel, MidiChannel::of(1));
                assert_eq!(controller, CCNumber::of(74));
                // Upscaled to 32-bit, not left as 7-bit.
                assert_eq!(value, ControlValue::from(U7::of(100)));
            }
            other => panic!("expected CV2 ControlChange, got {other:?}"),
        }
    }

    /// A System real-time message (Timing Clock 0xF8) has no channel voice
    /// form and must pass through unchanged, not be dropped or widened.
    #[test]
    fn inbound_system_message_passes_through() {
        use tutti_ump::{Message, System};

        let midi = to_midi(&api_from_bytes([0xF8, 0, 0])).expect("clock decodes");
        assert!(
            matches!(
                midi.ump.decode(),
                Ok(Message::System(_, System::TimingClock))
            ),
            "timing clock should stay a System Real-Time message"
        );
    }

    /// **A block past the cap stages the cap and counts the rest**: the lists are fixed at
    /// [`MIDI_STACK_CAPACITY`], so an event past it is refused, never staged by growing.
    #[test]
    fn a_block_past_the_cap_stages_the_cap_and_counts_the_rest() {
        let mut buf = MidiSendBuffer::new();
        let evs = filler(MIDI_STACK_CAPACITY + 5, 0);
        for _ in 0..2 {
            assert_eq!(staged_bytes(&mut buf, &evs).len(), MIDI_STACK_CAPACITY);
        }
        assert_eq!(buf.events_dropped().get(), 10);
    }

    /// What the plugin reads from a staged `api::Events`: per event, its
    /// `delta_frames` and either the MIDI bytes or the SysEx bytes, read
    /// through the pointers it is handed.
    fn note(frame: u32, key: u8) -> UmpAt {
        at(
            frame as usize,
            midi::note_on(MidiChannel::FIRST, key, 0x4000),
        )
    }

    /// The plugin is handed `deltaFrames` inside the block, in order,
    /// whatever the caller gave: an offset past the block is clamped to its
    /// last frame (as JUCE clamps with `jlimit`) and out-of-order events are
    /// delivered sorted, stably.
    #[test]
    fn stage_clamps_and_sorts_delta_frames() {
        let mut buf = MidiSendBuffer::new();
        let midi = [note(30, 62), note(9_000, 64), note(5, 60), note(30, 63)];
        let staged = read_staged(buf.stage_piece(&midi, 0, 64, 64).expect("events staged"));
        let got: Vec<(i32, u8)> = staged.iter().map(|(d, b)| (*d, b[1])).collect();
        assert_eq!(got, [(5, 60), (30, 62), (30, 63), (63, 64)]);
    }

    /// A piece of a split block gets the events that fall in it, rebased to
    /// the piece, and each event is delivered in exactly one piece.
    #[test]
    fn stage_piece_delivers_each_event_once_rebased() {
        let mut buf = MidiSendBuffer::new();
        let midi = [note(10, 60), note(70, 61), note(130, 62)];
        let piece = |buf: &mut MidiSendBuffer, start| {
            buf.stage_piece(&midi, start, 64, 200)
                .map(read_staged)
                .unwrap_or_default()
                .iter()
                .map(|(d, b)| (*d, b[1]))
                .collect::<Vec<_>>()
        };
        assert_eq!(piece(&mut buf, 0), [(10, 60)]);
        assert_eq!(piece(&mut buf, 64), [(6, 61)]);
        assert_eq!(piece(&mut buf, 128), [(2, 62)]);
    }

    /// **A SysEx message open at a block's end is completed by an `END` that
    /// falls in a later piece of the next, split block**: delivered once, at
    /// the rebased frame, with nothing counted dropped.
    #[test]
    fn a_sysex_ending_in_a_later_piece_is_delivered_once() {
        let body: Vec<u8> = (0..20).map(|i| i as u8 + 1).collect();
        let packets = sysex_packets(&body, 0);
        assert!(packets.len() >= 3, "fixture: START, CONTINUE.., END");
        let (open, end) = packets.split_at(packets.len() - 1);
        let mut buf = MidiSendBuffer::new();

        // Block N-1: START and CONTINUE, no END. One piece.
        assert!(buf.stage_piece(open, 0, 64, 64).is_none());

        // Block N, 128 frames in two pieces of 64: the END at frame 100.
        let end = [at(100, end[0].1)];
        assert!(
            buf.stage_piece(&end, 0, 64, 128).is_none(),
            "the END is not in the first piece"
        );
        let staged = read_staged(buf.stage_piece(&end, 64, 64, 128).expect("END's piece"));
        assert_eq!(staged, vec![(36, framed(&body))], "once, rebased: 100 - 64");
        assert_eq!(buf.sysex_dropped().get(), 0);

        // A block that leaves a run open, split in three, counts it at most
        // once (at the next START), not once per piece.
        let (start, _) = packets.split_at(1);
        for piece_start in [0, 64, 128] {
            let _ = buf.stage_piece(start, piece_start, 64, 192);
        }
        let _ = buf.stage_piece(&packets[..1], 0, 64, 64);
        assert_eq!(buf.sysex_dropped().get(), 1, "the one abandoned run");
    }

    /// **A SysEx keeps its input position among events at the same frame**:
    /// one whose `END` is listed before a note at that frame reaches the
    /// plugin first.
    #[test]
    fn a_sysex_keeps_input_order_at_an_equal_frame() {
        let body: Vec<u8> = (0..20).map(|i| i as u8 + 1).collect();
        let mut events = sysex_packets(&body, 3);
        events.push(note(3, 60));
        let mut buf = MidiSendBuffer::new();
        let staged = read_staged(buf.stage_piece(&events, 0, 64, 64).expect("staged"));
        assert_eq!(staged.len(), 2);
        assert_eq!(staged[0], (3, framed(&body)), "the SysEx, listed first");
        assert_eq!(staged[1].0, 3);
        assert_eq!(staged[1].1[1], 60, "then the note");
    }

    /// `n` Control Changes on channel 10, one a frame from frame `from`: they take room in a
    /// block and allocate no note.
    fn filler(n: usize, from: usize) -> Vec<UmpAt> {
        (from..from + n)
            .map(|k| at(k, Ump([0x20B9_0700 | 100, 0, 0, 0])))
            .collect()
    }

    fn read_staged(events: *mut crate::api::Events) -> Vec<(i32, Vec<u8>)> {
        use crate::api;
        let mut out = Vec::new();
        // SAFETY: test helper; `events` comes from `stage_piece`, which points
        // at the buffer's live header and event pointers (valid until the next
        // stage), and the buffer outlives this read.
        unsafe {
            let events = &*events;
            let table = events.events.as_ptr();
            for i in 0..events.num_events as usize {
                let ev = *table.add(i);
                match (*ev).event_type {
                    api::EventType::Midi => {
                        let m = &*(ev as *const api::MidiEvent);
                        out.push((m.delta_frames, m.midi_data.to_vec()));
                    }
                    api::EventType::SysEx => {
                        let x = std::ptr::read_unaligned(ev.cast::<api::SysExEvent>());
                        assert_eq!(x.byte_size as usize, std::mem::size_of::<api::SysExEvent>());
                        let bytes = std::slice::from_raw_parts(x.system_data, x.data_size as usize);
                        out.push((x.delta_frames, bytes.to_vec()));
                    }
                    _ => panic!("unexpected event type"),
                }
            }
        }
        out
    }

    fn sysex_packets(payload: &[u8], frame: u32) -> Vec<UmpAt> {
        Sysex7Packets::from_payload(MidiGroup::FIRST, payload)
            .expect("a payload of 7-bit bytes")
            .collect::<Vec<Ump>>()
            .into_iter()
            .map(|e| at(frame as usize, e))
            .collect()
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        tutti_ump::frame(payload).collect()
    }

    /// A Program Change with a bank reaches the plugin as CC 0, CC 32, PC at one
    /// offset, in that order, each in its own slot. With room for fewer than
    /// three, none is staged and all three are counted.
    #[test]
    fn a_banked_program_change_stages_as_three_events() {
        let pc = at(
            5,
            midi::program_change(
                MidiChannel::of(1),
                9,
                Some(Bank {
                    msb: U7::of(1),
                    lsb: U7::of(5),
                }),
            ),
        );
        let mut buf = MidiSendBuffer::new();
        let staged = read_staged(buf.stage_piece(&[pc], 0, 64, 64).expect("events staged"));
        assert_eq!(
            staged,
            vec![
                (5, vec![0xB1, 0, 1]),
                (5, vec![0xB1, 32, 5]),
                (5, vec![0xC1, 9, 0]),
            ]
        );
        // Room for two: none of the three is staged, and all three are counted.
        let mut small = MidiSendBuffer::new();
        let mut events = filler(MIDI_STACK_CAPACITY - 2, 6);
        events.push(pc);
        assert_eq!(
            staged_bytes(&mut small, &events).len(),
            MIDI_STACK_CAPACITY - 2
        );
        assert_eq!(small.events_dropped().get(), 3);
        // Room for the note and one more: the note stays, the bank does not.
        let note = at(
            0,
            midi::note_on(
                MidiChannel::of(1),
                60,
                NoteVelocity::from(U7::of(100)).get(),
            ),
        );
        let mut tight = MidiSendBuffer::new();
        let mut events = filler(MIDI_STACK_CAPACITY - 3, 6);
        events.extend([note, pc]);
        let staged = staged_bytes(&mut tight, &events);
        assert_eq!(staged.len(), MIDI_STACK_CAPACITY - 2);
        assert_eq!(staged[0], vec![0x91, 60, 100], "the note stays, at frame 0");
        assert_eq!(tight.events_dropped().get(), 3);
        // In a later piece every message of the bank takes that piece's
        // rebased frame (offset 70 of a 128-frame block, piece 64..128: 6).
        let late = at(70, pc.1);
        let mut buf = MidiSendBuffer::new();
        let staged = read_staged(buf.stage_piece(&[late], 64, 64, 128).expect("staged"));
        assert_eq!(
            staged,
            vec![
                (6, vec![0xB1, 0, 1]),
                (6, vec![0xB1, 32, 5]),
                (6, vec![0xC1, 9, 0]),
            ]
        );
    }

    /// **A multi-packet SysEx reaches the plugin as one `SysExEvent`**, in
    /// order beside the block's MIDI, `0xF0 … 0xF7`; one spanning two blocks
    /// arrives in the second.
    /// Spec: vst2.event.kVstSysExType
    #[test]
    fn a_multi_packet_sysex_reaches_the_plugin_whole() {
        let body: Vec<u8> = (0..40).collect();
        let note = midi::note_on(
            MidiChannel::of(0),
            60,
            NoteVelocity::from(U7::of(100)).get(),
        );
        let mut midi = vec![at(1, note)];
        midi.extend(sysex_packets(&body, 3));
        midi.push(at(4, note));
        let mut buf = MidiSendBuffer::new();
        let staged = read_staged(
            buf.stage_piece(&midi, 0, 4096, 4096)
                .expect("events staged"),
        );
        assert_eq!(
            staged,
            vec![
                (1, vec![0x90, 60, 100]),
                (3, framed(&body)),
                (4, vec![0x90, 60, 100]),
            ]
        );

        let long: Vec<u8> = (0..200).map(|i| (i % 0x7F) as u8).collect();
        let packets = sysex_packets(&long, 0);
        let (first, second) = packets.split_at(10);
        assert!(
            buf.stage_piece(first, 0, 4096, 4096).is_none(),
            "nothing before END"
        );
        let staged = read_staged(
            buf.stage_piece(second, 0, 4096, 4096)
                .expect("events staged"),
        );
        assert_eq!(staged, vec![(0, framed(&long))]);
        assert_eq!(buf.sysex_dropped().get(), 0);
    }

    /// **Staging a block's whole MIDI cap does not allocate, and an event
    /// past it is refused and counted.** Notes and complete SysEx messages
    /// fill every list (`api_events`, `sysex_events`, `order`, `header`) to
    /// [`MIDI_STACK_CAPACITY`]; then one more of each.
    #[test]
    fn staging_the_whole_cap_does_not_allocate() {
        let note = at(
            0,
            midi::note_on(
                MidiChannel::of(0),
                60,
                NoteVelocity::from(U7::of(100)).get(),
            ),
        );
        let single = sysex_packets(&[1, 2, 3], 0);
        assert_eq!(single.len(), 1);
        // Half notes, half one-packet SysEx, then one past the cap of each.
        let midi: Vec<UmpAt> = (0..MIDI_STACK_CAPACITY / 2 + 1)
            .flat_map(|_| [note, single[0]])
            .collect();
        let mut buf = MidiSendBuffer::new();
        let mut staged = 0;
        assert_no_alloc::assert_no_alloc(|| {
            let events = buf
                .stage_piece(&midi, 0, 4096, 4096)
                .expect("events staged");
            // SAFETY: `events` was just returned by `stage_piece` on `buf`,
            // which is live and untouched since.
            staged = unsafe { (*events).num_events } as usize;
        });
        assert_eq!(staged, MIDI_STACK_CAPACITY);
        assert_eq!(buf.events_dropped().get(), 2);
        assert_eq!(buf.sysex_dropped().get(), 0);
    }

    /// **Staging a packet of any message type allocates nothing.** Every
    /// message type (the defined ones and the reserved ones) with bodies that
    /// are valid, banked, undefined and malformed goes through `stage_piece`
    /// inside `assert_no_alloc`; only System, MIDI 1.0 and MIDI 2.0 channel
    /// voice packets stage anything.
    #[test]
    fn staging_a_packet_of_any_message_type_allocates_nothing() {
        const BODIES: [u32; 8] = [
            0x0000_0000,
            0x0090_3C40,
            0x00C2_0001,
            0x00F8_0000,
            0x0120_0000,
            0x0010_1234,
            0x0F01_0101,
            0x00E1_0000,
        ];
        let midi: Vec<UmpAt> = (0..16u32)
            .flat_map(|mt| {
                BODIES.map(|body| {
                    (
                        Offset::ZERO,
                        Ump([mt << 28 | body, 0x0900_0102, 0x0102_0304, 0x0506_0708]),
                    )
                })
            })
            .collect();
        let mut buf = MidiSendBuffer::new();
        assert_no_alloc::assert_no_alloc(|| {
            let _ = buf.stage_piece(&midi, 0, 4096, 4096);
        });
        assert_eq!(buf.events_dropped().get(), 0);
    }

    /// **The staged `api::Events` pointer is 8-aligned for every list size**,
    /// and its pointer table starts at the flexible array's offset.
    #[test]
    fn the_staged_events_pointer_is_8_aligned() {
        let note = at(
            0,
            midi::note_on(
                MidiChannel::of(0),
                60,
                NoteVelocity::from(U7::of(100)).get(),
            ),
        );
        let mut buf = MidiSendBuffer::new();
        for n in 1..=MIDI_STACK_CAPACITY {
            let midi = vec![note; n];
            let events = buf.stage_piece(&midi, 0, 64, 64).expect("events staged");
            assert_eq!(
                events as usize % align_of::<crate::api::Events>(),
                0,
                "n = {n}"
            );
            // SAFETY: `events` is the pointer `stage_piece` returned for `buf`,
            // which is live and not restaged since.
            let staged = unsafe { (*events).num_events };
            assert_eq!(staged as usize, n);
        }
    }

    /// **A SysEx left open by a later `START` is dropped whole and counted**,
    /// and the later one arrives.
    #[test]
    fn a_start_left_open_is_dropped_and_counted() {
        let a: Vec<u8> = (0..30).collect();
        let mut midi = sysex_packets(&a, 0);
        midi.truncate(2);
        midi.extend(sysex_packets(&[1, 2, 3, 4, 5, 6, 7], 2));
        let mut buf = MidiSendBuffer::new();
        let staged = read_staged(
            buf.stage_piece(&midi, 0, 4096, 4096)
                .expect("events staged"),
        );
        assert_eq!(staged, vec![(2, framed(&[1, 2, 3, 4, 5, 6, 7]))]);
        assert_eq!(buf.sysex_dropped().get(), 1);
    }
    /// The bytes of what one block of `midi` hands the plugin, through `buf`.
    fn staged_bytes(buf: &mut MidiSendBuffer, midi: &[UmpAt]) -> Vec<Vec<u8>> {
        buf.stage_piece(midi, 0, 4096, 4096)
            .map(read_staged)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, bytes)| bytes)
            .collect()
    }

    /// **VST2 has one MIDI input, which takes Group 0 only**: a channel voice
    /// message of another Group is dropped and counted, never heard on Group 0's
    /// channels (M2-104 section 2.1.2). A message with no Group's channels (a
    /// System Real Time message) is still sent.
    #[test]
    fn only_group_zero_reaches_the_one_midi_input() {
        use tutti_ump::{MidiChannel, MidiGroup};
        let group = |n| MidiGroup::new(n).unwrap();
        let mut buf = MidiSendBuffer::new();
        let midi = [
            at(
                0,
                midi::note_on(
                    tutti_ump::Address::new(group(0), MidiChannel::FIRST),
                    60,
                    0x8000,
                ),
            ),
            at(
                0,
                midi::note_on(
                    tutti_ump::Address::new(group(1), MidiChannel::FIRST),
                    61,
                    0x8000,
                ),
            ),
            at(
                0,
                midi::cc(
                    tutti_ump::Address::new(group(2), MidiChannel::FIRST),
                    tutti_ump::CCNumber::BRIGHTNESS,
                    0,
                ),
            ),
            at(0, Ump::from(Message::System(group(1), System::TimingClock))),
        ];
        assert_eq!(
            staged_bytes(&mut buf, &midi),
            [vec![0x90, 60, 64], vec![0xF8, 0, 0]]
        );
        assert_eq!(buf.groups_dropped().get(), 2);
    }

    /// **An RPN is four Control Changes every time** (M2-104 Appendix D.2.3): CC
    /// 101 and 100 select, CC 6 and 38 carry the top 14 bits (12 << 25 is MSB
    /// 12, LSB 0); the same parameter in the next block is the four again, and
    /// so is one after a reset.
    #[test]
    fn an_rpn_is_four_control_changes_every_time() {
        use tutti_ump::MidiChannel;
        let rpn = |value| {
            at(
                0,
                midi::registered(MidiChannel::of(1), Rpn::new(U7::of(0), U7::of(0)), value),
            )
        };
        let mut buf = MidiSendBuffer::new();
        assert_eq!(
            staged_bytes(&mut buf, &[rpn(12 << 25)]),
            [
                vec![0xB1, 101, 0],
                vec![0xB1, 100, 0],
                vec![0xB1, 6, 12],
                vec![0xB1, 38, 0],
            ]
        );
        assert_eq!(
            staged_bytes(&mut buf, &[rpn(24 << 25)]),
            [
                vec![0xB1, 101, 0],
                vec![0xB1, 100, 0],
                vec![0xB1, 6, 24],
                vec![0xB1, 38, 0],
            ]
        );
        buf.reset_midi();
        assert_eq!(staged_bytes(&mut buf, &[rpn(12 << 25)]).len(), 4);
    }

    /// **A refused message leaves an MPE note's member channel as it was**: the
    /// The MIDI 1.0 translator's note allocation survives a message past the room, so the
    /// note's per-note bend still goes to its channel (M2-104 D.2.8).
    #[test]
    fn a_refused_message_keeps_an_mpe_notes_channel() {
        use tutti_midi_protocol::{
            MpeZoneConfig, PerNoteTranslation, PitchBendSensitivity, TranslationMode,
        };
        use tutti_ump::{
            Address, Attribute, BendValue, ControlValue, ExtremeValues, Message, NoteNumber,
            NoteVelocity, Voice2,
        };
        let zone = MpeZoneConfig::lower(
            2,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap();
        // Room for the note, the bend and one more: the RPN's four do not fit.
        let mut buf = MidiSendBuffer::new();
        buf.midi1_translator = Midi1Translator::with_mode(TranslationMode {
            extremes: ExtremeValues::Scaled,
            per_note: PerNoteTranslation::Mpe(zone),
        });
        let address = Address::new(MidiGroup::FIRST, MidiChannel::FIRST);
        let note = NoteNumber::of(60);
        let ump = |voice| at(0, Message::midi2(address, voice).into());
        let midi = [
            ump(Voice2::NoteOn {
                note,
                velocity: NoteVelocity::new(0x8000),
                attribute: Attribute::None,
            }),
            ump(Voice2::registered(
                tutti_ump::Rpn::PITCH_BEND_SENSITIVITY,
                ControlValue::new(12 << 25),
            )),
            ump(Voice2::PerNotePitchBend {
                note,
                value: BendValue::new(0x8000_0000),
            }),
        ];
        // The block holds fillers first, so three slots are left for the note, the RPN and
        // the bend: the RPN's four are refused.
        let mut events = filler(MIDI_STACK_CAPACITY - 3, 1);
        events.extend(midi);
        let staged = staged_bytes(&mut buf, &events);
        // Note On on member channel 2 (0x91), then its Pitch Bend, centre 0x2000.
        assert_eq!(staged[..2], [vec![0x91, 60, 0x40], vec![0xE1, 0x00, 0x40]]);
        assert_eq!(staged.len(), MIDI_STACK_CAPACITY - 1);
        assert_eq!(buf.events_dropped().get(), 4);
    }

    /// **A banked Program Change is three messages every time** (M2-104
    /// Appendix D.2.4), and a refused message (past the room) is dropped whole,
    /// so the next one is sent in full.
    #[test]
    fn a_banked_program_change_is_three_messages_every_time() {
        use tutti_ump::MidiChannel;
        let pc = |program, bank| at(0, midi::program_change(MidiChannel::FIRST, program, bank));
        let bank = Some(Bank {
            msb: U7::of(5),
            lsb: U7::of(9),
        });
        let mut buf = MidiSendBuffer::new();
        assert_eq!(
            staged_bytes(&mut buf, &[pc(3, bank)]),
            [vec![0xB0, 0, 5], vec![0xB0, 32, 9], vec![0xC0, 3, 0]]
        );
        assert_eq!(
            staged_bytes(&mut buf, &[pc(4, bank)]),
            [vec![0xB0, 0, 5], vec![0xB0, 32, 9], vec![0xC0, 4, 0]]
        );
        assert_eq!(
            staged_bytes(
                &mut buf,
                &[pc(
                    4,
                    Some(Bank {
                        msb: U7::of(6),
                        lsb: U7::of(0)
                    })
                )]
            )
            .len(),
            3
        );

        // A block with room for three past its fillers: a note then the three-message bank
        // for bank B does not fit, so the bank is refused whole and the next Program Change
        // of B is in full.
        let note = at(0, midi::note_on(MidiChannel::FIRST, 60, 0x8000));
        let mut tight = MidiSendBuffer::new();
        let bank_b = Some(Bank {
            msb: U7::of(7),
            lsb: U7::of(0),
        });
        let with_fillers = |events: &[UmpAt]| {
            let mut all = filler(MIDI_STACK_CAPACITY - 3, 1);
            all.extend_from_slice(events);
            all
        };
        assert_eq!(
            staged_bytes(&mut tight, &with_fillers(&[pc(3, bank)])).len(),
            MIDI_STACK_CAPACITY
        );
        assert_eq!(
            staged_bytes(&mut tight, &with_fillers(&[note, pc(4, bank_b)])).len(),
            MIDI_STACK_CAPACITY - 2
        );
        assert_eq!(tight.events_dropped().get(), 3);
        assert_eq!(
            staged_bytes(&mut tight, &with_fillers(&[pc(5, bank_b)])).len(),
            MIDI_STACK_CAPACITY
        );
    }
}
