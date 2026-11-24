//! [`IpcProcessor`]: the [`Batcher`] behind [`Processor::process`].
//!
//! The Processor renders in the pipeline's chunks (`chunk()`), which begin in
//! the middle of a graph block and span several calls; the graph node cuts
//! each block at them (`Processor::chunk`'s contract), so every call is one
//! piece inside one chunk. A call hands the piece's audio to the batcher and
//! gathers the chunk's payload from the piece's [`ProcessContext`]: MIDI and
//! harmony at the **chunk's frames** (the piece's own frames plus where it
//! sits in the chunk); the transport and its changes are the node's walk across
//! the chunk, read in the piece that completes it; the parameter changes are the
//! chunk's so far, copied into the buffer the bridge swaps when the chunk
//! ships. What a chunk lost on the way (MIDI not staged or refused, parameter
//! changes not sent) is reported in the call's [`ProcessOutput`], and the
//! plugin's MIDI-out is emitted on the piece's frames.

use std::num::NonZeroUsize;
use std::sync::Arc;

use tutti_buffers::{Float, Planes};
use tutti_plugin_types::{BlockStatus, MIDI_STACK_BOUND, MidiIntake};
use tutti_plugin_types::{
    ChordValue, ParameterChanges, PluginIo, PluginResult, ProcessContext, ProcessOutput, Processor,
    SampleRate, Samples, ScaleValue, TransportInfo, WireMidi,
};

use super::Link;
use crate::{Batcher, BlockPayload, Chunks, Replies, SlabWriter};
#[cfg(test)]
use tutti_plugin_types::fake::{at_frame, unit_value};

/// The realtime half of an activated served plugin. See the module docs.
pub struct IpcProcessor {
    pub(crate) link: Arc<Link>,
    batcher: Batcher,
    /// The payload of the chunk being filled: gathered when the chunk begins,
    /// shipped when it completes (possibly a call later).
    pending: BlockPayload,
    /// Takes every block's MIDI into `pending`: a note-off displaces a note-on
    /// when the chunk is full, and a SysEx that lost a packet loses the rest.
    intake: MidiIntake,
    /// The chunk's parameter changes as the bridge takes them: the node's
    /// list copied in when the chunk ships, then swapped for a free buffer.
    params: ParameterChanges,
}

impl std::fmt::Debug for IpcProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcProcessor")
            .field("chunk", &self.batcher.chunk())
            .finish_non_exhaustive()
    }
}

impl IpcProcessor {
    pub(super) fn new(link: Arc<Link>, batcher: Batcher) -> Self {
        let (queues, points) = link.param_room(batcher.chunk());
        Self {
            pending: BlockPayload::new(link.rate()),
            intake: MidiIntake::default(),
            link,
            batcher,
            params: ParameterChanges::reserved_bridged(queues, points),
        }
    }

    /// The slab's writer and the next chunk's sequence, back to the loaded
    /// plugin on deactivation.
    pub(super) fn into_parts(self) -> (SlabWriter, u64, Replies) {
        self.batcher.into_parts()
    }
}

impl Processor for IpcProcessor {
    fn process<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
        let mut piece = Piece {
            ctx: io.ctx,
            out: &mut *io.out,
            pending: &mut self.pending,
            intake: &mut self.intake,
            rate: self.link.rate(),
            params: &mut self.params,
            over: 0,
            said: BlockStatus::Continue,
        };
        self.batcher
            .process(self.link.bridge.audio(), &mut io.buffer, &mut piece);
        let carries = carries_input(piece.ctx, io.buffer.split().0);
        let over = piece.over;
        let said = piece.said;
        // **A sleep is applied only to a piece that gave the plugin nothing.**
        // The status is the plugin's word on the chunk collected at this
        // piece's start, whose input is a chunk behind; this piece's input and
        // events are in the chunk it begins, which has not been shipped. Idle
        // would park the node and that chunk with it, so a piece that carries
        // anything (an event, a parameter, a note expression, a non-silent
        // input frame) does not get the sleep, and the node stays called.
        if said == BlockStatus::Sleep && !carries {
            io.out.set_status(BlockStatus::Sleep);
        }
        if over > 0 {
            // Taken into a chunk already holding the most it can: counted
            // lost, as the served node counts what its cap drops.
            io.out
                .count_midi_lost(usize::try_from(over).unwrap_or(usize::MAX));
        }
        Ok(())
    }

    /// Nothing to stop: the chunk in flight stays where it is, and the next
    /// call carries on from it.
    fn stop(&mut self) {}

    /// Drops the chunk in flight and tells the server to reset. (What the
    /// chunk being filled held of MIDI is dropped with it, uncounted: the
    /// graph node's own reset counts the SysEx it cuts.)
    fn reset(&mut self) {
        self.batcher.reset();
        // The chunk's MIDI is dropped with it, and a SysEx open in it is cut.
        let _ = self.intake.lose(&mut self.pending.midi);
        self.pending = BlockPayload::new(self.link.rate());
        self.params.clear();
        // A refused reset is counted (`BridgeLosses::commands_refused`).
        let _ = self.link.bridge.audio().reset_rt();
    }

    /// A block's MIDI is as large as a chunk's: [`MIDI_STACK_CAPACITY`](tutti_plugin_types::MIDI_STACK_CAPACITY)
    /// events. A chunk spans several blocks and every one adds to its one
    /// list, so a served plugin has a second, chunk-level cut: a note-on is
    /// displaced for a note-off and every event lost is counted once as MIDI
    /// lost.
    fn event_input(&self) -> std::num::NonZeroU32 {
        MIDI_STACK_BOUND
    }

    /// The pipeline's chunk: the frames every submission carries.
    fn chunk(&self) -> Option<NonZeroUsize> {
        NonZeroUsize::new(self.batcher.chunk())
    }

    /// One chunk: the pipeline submits a chunk and plays the one before.
    fn latency(&self) -> Samples {
        Samples::new(self.batcher.chunk())
    }

    const RETUNE: Option<fn(&mut Self, SampleRate, Samples, Option<Samples>)> =
        Some(Self::retune_live);
}

impl IpcProcessor {
    /// The rate by a queued command, the chunk from the block and quantum,
    /// the bridge's parameter buffers for it. Starts the pipeline over.
    fn retune_live(&mut self, rate: SampleRate, max_block: Samples, quantum: Option<Samples>) {
        self.batcher.prepare(max_block, quantum);
        let chunk = self.batcher.chunk();
        self.link.reserve(chunk);
        let (queues, points) = self.link.param_room(chunk);
        self.params = ParameterChanges::reserved_bridged(queues, points);
        self.pending = BlockPayload::new(rate);
        // A refused rate command is counted (`BridgeLosses::commands_refused`).
        let _ = self.link.bridge.audio().set_sample_rate(rate);
        self.link.set_rate(rate);
        // A fork's graph is compiled from the shape read right after this, and
        // a plugin may move its latency when its rate or render mode changes
        // (an HQ offline mode is the usual case). Both changes are queued
        // commands the server has not necessarily handled yet, so wait for
        // them and for any latency they caused (`AudioBridge::settle`), then
        // record the figure the plan will hold: the fork's health reports a
        // later move as a fault rather than a silently misaligned export.
        // Live, the latency poll re-plans instead, and a retune never blocks.
        if let Some(super::Watch { wait, .. }) = self.link.watch() {
            self.link.bridge.audio().settle();
            wait.plan(Samples::new(
                self.link.latency().get() + self.batcher.chunk(),
            ));
        }
    }
}

/// What a piece gives the batcher: the chunk's payload, begun and filled from
/// the piece's context, and where the plugin's MIDI-out and the chunk's
/// losses go.
struct Piece<'a> {
    ctx: &'a ProcessContext<'a>,
    out: &'a mut ProcessOutput,
    pending: &'a mut BlockPayload,
    intake: &'a mut MidiIntake,
    /// The rate the chunk's placeholder transport is at.
    rate: SampleRate,
    params: &'a mut ParameterChanges,
    /// MIDI the chunk had no room for, or gave up for a note-off.
    over: u64,
    /// What the plugin said at the end of the chunk collected in this call.
    said: BlockStatus,
}

/// Whether a piece hands the plugin anything a sleeping plugin would wake
/// for: MIDI, parameter changes, note expression, sequencer context, or a
/// non-silent input frame.
fn carries_input<S: Float>(ctx: &ProcessContext<'_>, input: Planes<'_, S>) -> bool {
    !ctx.midi_events.is_empty()
        || ctx.param_changes.is_some_and(|c| !c.is_empty())
        || ctx.note_expression.is_some_and(|c| !c.changes.is_empty())
        || ctx.expressive.as_ref().is_some_and(|e| {
            e.chords.is_some_and(|c| !c.changes.is_empty())
                || e.scales.is_some_and(|c| !c.changes.is_empty())
                || e.expr_texts.is_some_and(|c| !c.changes.is_empty())
                || e.expr_ints.is_some_and(|c| !c.changes.is_empty())
        })
        || input
            .iter()
            .any(|plane| plane.iter().any(|&x| x != S::SILENCE))
}

impl Chunks for Piece<'_> {
    /// The chunk begins empty: its transport and changes are the node's, read
    /// when it completes ([`payload`](Chunks::payload)).
    fn begin(&mut self, _at: usize, _chunk: usize) {
        *self.pending = BlockPayload::new(self.rate);
        self.intake.new_chunk();
    }

    /// The piece's MIDI and harmony join the chunk at `at`, the piece's first
    /// frame in it.
    fn take(&mut self, _from: usize, _n: usize, at: usize) {
        let at32 = u32::try_from(at).unwrap_or(u32::MAX);
        for (offset, ump) in self.ctx.midi_events {
            let event = WireMidi {
                offset: offset.get().saturating_add(at32),
                ump: *ump,
            };
            self.over += self.intake.push_displacing(&mut self.pending.midi, event);
        }
        // The chunk's harmony carries the degrees a plugin acts on, shifted to
        // the chunk's frames. Chord and scale text is not carried yet: cloning
        // a `String` here would allocate, and a refused event would free it.
        let shift = i32::try_from(at).unwrap_or(i32::MAX);
        if let Some(expressive) = &self.ctx.expressive {
            if let Some(chords) = expressive.chords {
                for chord in &chords.changes {
                    let _ = self.pending.harmony.chords.add_change(ChordValue {
                        sample_offset: chord.sample_offset.saturating_add(shift),
                        root: chord.root,
                        bass_note: chord.bass_note,
                        mask: chord.mask,
                        text: String::new(),
                    });
                }
            }
            if let Some(scales) = expressive.scales {
                for scale in &scales.changes {
                    let _ = self.pending.harmony.scales.add_change(ScaleValue {
                        sample_offset: scale.sample_offset.saturating_add(shift),
                        root: scale.root,
                        mask: scale.mask,
                        text: String::new(),
                    });
                }
            }
        }
    }

    /// The chunk's MIDI, sorted by frame: stable, in place (a short list,
    /// sorted already unless the pieces came in out of order).
    fn payload(&mut self, _frames: usize) -> BlockPayload {
        // The node walked the transport across the chunk's pieces: in the piece
        // that completes it (this call) the transport is the chunk's first
        // frame's, slope included, and the changes are the chunk's, on its
        // frames (`Processor::chunk`'s contract).
        self.pending.transport = self
            .ctx
            .transport
            .cloned()
            .unwrap_or_else(|| TransportInfo::new(self.rate));
        for change in self.ctx.transport_changes {
            // The node's list and this one hold the same most: none is lost.
            let _ = self.pending.changes.try_push(*change);
        }
        tutti_plugin_types::insertion_sort_by_key(&mut self.pending.midi, |e| e.offset);
        std::mem::replace(self.pending, BlockPayload::new(self.rate))
    }

    /// The node's chunk so far, copied into the buffer the bridge swaps.
    fn params(&mut self) -> Option<&mut ParameterChanges> {
        self.params.clear();
        if let Some(chunk) = self.ctx.param_changes {
            for queue in &chunk.queues {
                if !self.params.open_queue(queue.param_id) {
                    continue;
                }
                let index = self.params.queues.len() - 1;
                for point in &queue.points {
                    let _ = self
                        .params
                        .push_point(index, point.sample_offset, point.value);
                }
            }
        }
        Some(&mut *self.params)
    }

    fn handed_off(&mut self, delivered: bool) {
        if !delivered {
            self.out.set_params_unsent();
        }
        // Swapped for a free buffer on a delivery; still holding the chunk
        // otherwise. The node owes its values to the next chunk either way.
        self.params.clear();
    }

    /// The chunk's MIDI is lost, counted.
    fn unsent(&mut self) {
        self.out.count_midi_lost(self.pending.midi.len());
        self.pending.midi.clear();
    }

    fn refused(&mut self, midi: usize) {
        self.out.count_midi_lost(midi);
    }

    fn status(&mut self, status: BlockStatus) {
        self.said = status;
    }

    /// Each event goes out where it plays.
    fn emit(&mut self, frame: usize, mut event: WireMidi) {
        event.offset = u32::try_from(frame).unwrap_or(u32::MAX);
        let _ = self.out.emit_midi([event]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::MIDI_STACK_CAPACITY;
    use tutti_units::ParamId;

    /// **A piece carries input when it holds an event, a parameter change or a non-silent frame,
    /// and only then**: it is what keeps a collected *sleep* from idling the
    /// node over a chunk that has a note in it (the chunk it begins is not
    /// shipped yet, so parking the node would park the note).
    #[test]
    fn a_piece_carries_input_only_if_it_holds_something() {
        let silence = [0.0f32; 8];
        let mut noisy = silence;
        noisy[5] = 0.25;
        let quiet = ProcessContext::new();
        let (a, b) = (&silence[..], &noisy[..]);
        let both_quiet = [a, a];
        assert!(!carries_input(
            &quiet,
            Planes::new(&both_quiet).expect("equal")
        ));
        let one_noisy = [a, b];
        assert!(
            carries_input(&quiet, Planes::new(&one_noisy).expect("equal")),
            "a sample"
        );
        let past = [&noisy[..5]];
        assert!(
            !carries_input(&quiet, Planes::new(&past).expect("one")),
            "a sample past the piece is not the piece's"
        );
        let mut changes = ParameterChanges::reserved(1, 4);
        assert!(changes.add_change(ParamId::new(1), at_frame(3), unit_value(0.5)));
        let params = ProcessContext::new().params(&changes);
        let one = [&silence[..]];
        assert!(
            carries_input(&params, Planes::new(&one).expect("one")),
            "a parameter"
        );
    }

    /// **A chunk full of note-ons keeps the note-off that arrives after it**,
    /// whichever block of the chunk brings it: the note-off takes the place of
    /// a note-on (the one of its own key), the chunk stays at its cap, and the
    /// one event lost is counted once.
    #[test]
    fn a_full_chunk_keeps_a_later_note_off_and_counts_one_loss() {
        use tutti_fixtures::midi;
        use tutti_graph::{Offset, UmpAt};
        use tutti_ump::MidiChannel;

        let at = Offset::of(Samples::new(0), Samples::new(64));
        let ons: Vec<UmpAt> = (0..MIDI_STACK_CAPACITY)
            .map(|k| {
                (
                    at,
                    midi::note_on(MidiChannel::FIRST, (k % 128) as u8, 0x8000),
                )
            })
            .collect();
        let off: [UmpAt; 1] = [(at, midi::note_off(MidiChannel::FIRST, 5, 0))];
        let mut out = ProcessOutput::default();
        let mut pending = BlockPayload::new(SampleRate::SR_48K);
        let mut intake = MidiIntake::default();
        let mut params = ParameterChanges::reserved(1, 4);
        let mut over = 0;
        for events in [&ons[..], &off[..]] {
            let ctx = ProcessContext::new().midi(events);
            let mut piece = Piece {
                ctx: &ctx,
                out: &mut out,
                pending: &mut pending,
                intake: &mut intake,
                rate: SampleRate::SR_48K,
                params: &mut params,
                over: 0,
                said: BlockStatus::Continue,
            };
            piece.take(0, 64, 0);
            over += piece.over;
        }
        assert_eq!(pending.midi.len(), MIDI_STACK_CAPACITY, "at its cap");
        assert!(
            pending.midi.iter().any(|e| matches!(
                e.ump.note_key_event(),
                Some(tutti_ump::NoteKeyEvent::Off(_))
            )),
            "the note-off is in the chunk"
        );
        assert_eq!(over, 1, "one event given up for it, counted once");
    }

    /// **A piece's chords and scales join the chunk shifted to the chunk's
    /// frames, without allocating, whatever text they carry.** The context
    /// holds a named chord and a named scale; the chunk gets their degrees at
    /// `at` frames later and no text (a refused event past the cap frees
    /// nothing either).
    #[test]
    fn a_pieces_harmony_joins_the_chunk_shifted_and_allocation_free() {
        use tutti_plugin_types::{ChordChanges, ExpressiveContext, ScaleChanges};
        let mut chords = ChordChanges::new();
        let mut scales = ScaleChanges::new();
        for i in 0..=tutti_plugin_types::HARMONY_CAPACITY as i32 {
            let _ = chords.add_change(ChordValue {
                sample_offset: i,
                root: 60,
                bass_note: 48,
                mask: 0b1001_0001,
                text: "Cmaj7 over a long enough name to need the heap".into(),
            });
            let _ = scales.add_change(ScaleValue {
                sample_offset: 8 + i,
                root: 2,
                mask: 0x0AB5,
                text: "D Dorian, spelled out at length for the heap".into(),
            });
        }
        let ctx = ProcessContext::new().expressive(ExpressiveContext {
            chords: Some(&chords),
            scales: Some(&scales),
            ..ExpressiveContext::default()
        });
        let mut out = ProcessOutput::default();
        let mut pending = BlockPayload::new(SampleRate::SR_48K);
        let mut params = ParameterChanges::reserved(1, 4);
        let mut piece = Piece {
            ctx: &ctx,
            out: &mut out,
            pending: &mut pending,
            intake: &mut MidiIntake::default(),
            rate: SampleRate::SR_48K,
            params: &mut params,
            over: 0,
            said: BlockStatus::Continue,
        };
        assert_no_alloc::assert_no_alloc(|| piece.take(0, 64, 64));
        let at: Vec<i32> = pending
            .harmony
            .chords
            .changes
            .iter()
            .map(|c| c.sample_offset)
            .collect();
        assert_eq!(at, [64, 65, 66, 67], "the first four, 64 frames in");
        assert_eq!(pending.harmony.chords.changes[0].mask, 0b1001_0001);
        assert_eq!(pending.harmony.chords.changes[0].bass_note, 48);
        assert_eq!(pending.harmony.scales.changes[3].sample_offset, 8 + 3 + 64);
        assert_eq!(pending.harmony.scales.changes[0].mask, 0x0AB5);
    }
}
