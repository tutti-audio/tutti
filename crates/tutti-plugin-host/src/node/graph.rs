//! The in-process plugin as a graph node, over any [`InProcess`] format: MIDI
//! arrives on an event input, on its frame, and the plugin's MIDI-out leaves
//! on an event output (a plugin that declared `Features::MIDI_OUT`), each
//! event on the frame the plugin gave it.
//!
//! **Parameters** arrive on the same event input: a ramp or a curve segment
//! (a control lane addressed by id, `ForeignLaneConfig`), addressed by
//! the plugin's own parameter id.
//! They go through the out-of-process node's delivery (`host::node`'s
//! `param_delivery` and `param_input`): a slot per parameter declared at
//! load, **changes only** against the value last delivered to the instance,
//! and the same segment points. The block is the chunk: before the plugin
//! processes it, each parameter that moved is set to its latest value in the
//! block is handed to the Processor as [`ProcessContext::params`](tutti_plugin_types::ProcessContext::params), which
//! consumes it as its format does (VST2 sets each queue's last point, as the
//! out-of-process VST2 loader sets a chunk's). The two hosts differ only in
//! that the out-of-process one's chunk is the pipeline's, this one's the
//! graph's block. A block with no Processor in hand (silence, counted in
//! [`BlockLosses::unlent`](crate::BlockLosses::unlent)) or one the Processor
//! failed ([`BlockLosses::failed`](crate::BlockLosses::failed), its cause
//! logged through [`Io::log_plugin`]) delivers nothing, and its values are
//! owed to the next block. Counted in [`NodeControls::param_delivery`].
//!
//! Forkable by state transfer (see its `IntoNode` below, and `fork.rs`). The
//! live node and its fork read the transport the same way: from the block's
//! `Cx`, the transport its input was rendered at (`TransportFeed`).

use std::num::NonZeroUsize;

use arrayvec::ArrayVec;
use tutti_buffers::Float;
use tutti_graph::{
    BusOutput, Cx, Event, EventKind, Harmony, HarmonyKind, InBus, IntoNode, Io, MAX_BUSES, Node,
    NodeParts, Offset, OutBus, Prepare, RtLevel, Shape, SortedEvents, Status,
};
use tutti_units::{Latency, Samples, Tail};

use super::client::{
    InProcessClient, Inputs, PluginNode, Rendered, render_with, take_edits, void_stale_edits,
};
use crate::{Call, NodeControls, Snapshot, param_input, transport_info};
use tutti_plugin_types::{BlockStatus, ProcessOutput, Processing};
use tutti_plugin_types::{
    ChannelTable, ChordChanges, ChordValue, ExpressiveContext, Features, InProcess, Loaded,
    Processor, ScaleChanges, ScaleValue, TransportInfo, WireMidi, silence,
};

/// A side's handles: one per bus and one for the ports past them.
type Handles<H> = ArrayVec<H, { MAX_BUSES + 1 }>;

/// Every input channel, a bus at a time in port order.
fn channels_in<'s, F: Float>(
    inputs: &tutti_graph::Inputs<'s, F>,
    buses: &[InBus],
) -> impl Iterator<Item = &'s [F]> {
    buses.iter().flat_map(|&bus| inputs.bus(bus).channels())
}

impl<L: Loaded + 'static, F: Float> Node<F> for PluginNode<L, F>
where
    L::Processor: 'static,
{
    /// The plugin's channels, one event input (MIDI and parameter events; a
    /// param ramp or segment scheduled to the node arrives here),
    /// and a MIDI event output for a plugin that declared MIDI out; its
    /// latency and tail as the control side last read them. The plugin reports its tail apart from
    /// its latency, so the tail excludes latency as [`Shape::tail`] asks.
    fn shape(&self) -> Shape {
        let this = &self.client;
        let rate = this.sample_rate;
        let midi_out = u16::from(this.features.contains(Features::MIDI_OUT));
        // The plugin's figures as the control side last re-read them, whether
        // or not it holds the instance now: nothing is dispatched to the
        // plugin here.
        let (latency, tail) = match &this.controls {
            // A served plugin's, re-reported by the bridge's thread into the
            // controls' cells, which also hold the pipeline's chunk.
            Some(controls) => (controls.latency(), controls.tail()),
            // An in-process plugin's, published by its instance at every step.
            None => this.figures.get(),
        };
        // The plugin's own latency, and what the Processor adds to it (the
        // served pipeline's chunk).
        let latency = Latency::new(Samples::new(latency.get() + this.pipeline.get()));
        this.declared
            .ports
            .events(1, midi_out)
            .param_events()
            .event_input_capacity(this.event_input)
            .latency(latency)
            // The ceiling: the plugin's claim, raised by what a probe measured
            // (empty until a probe ran with `--measure`), never lowered.
            .tail(Processing::new(tail).ceiling(rate))
    }

    /// The rate (pending for the plugin, dispatched from the main thread; a
    /// fork's dispatched here). Control thread.
    ///
    /// The parameter delivery's point room follows the largest block, and
    /// the segments in flight are dropped (as a re-prepare starts the node
    /// over); what was delivered to the plugin is kept.
    fn prepare(&mut self, p: &Prepare) {
        let this = &mut self.client;
        // What the event input is declared to take in a block: the node's own
        // intake, at most the Processor's input room (`Processor::event_input`;
        // the last one seen while it is away). A served plugin has a second cut
        // at its chunk (`Processor::event_input`).
        this.event_input = this.event_bound();
        // A Processor that follows the graph takes its rate, block and
        // quantum live (no restart, no main thread); one that does not is
        // told the rate by the lifecycle.
        if this.retune(p) {
            // The Processor started its pipeline over: the chunk in flight is
            // dropped (`drop_chunk` below), and the rest of a SysEx open in it
            // is cut, as at a reset. Its far end keeps its reassembler's open
            // message across the retune (a served server does nothing for a
            // rate it already has), so a tail taken now would complete the
            // message with a gap.
            this.intake.lost();
        } else {
            this.set_rate(p.sample_rate());
        }
        // The session's mode, at configure time beside the rate.
        this.set_render_mode_from(p);
        // A chunked Processor's delivery chunk is its own (`Processor::chunk`),
        // and it starts counting from here.
        this.phase = Samples::new(0);
        this.drop_chunk();
        let chunk = this
            .rt
            .held()
            .and_then(|processor| processor.chunk())
            .map(NonZeroUsize::get);
        this.params
            .prepare(chunk.unwrap_or_else(|| p.max_block().get()));
        this.segments.prepare(this.params.room());
    }

    /// A parked node (bypassed) answers its Processor's handoff: a recall the
    /// control side raised (a restart, a commit's relend, a rate change) is
    /// returned, and a Processor on offer is taken. Wait-free; renders nothing.
    fn poll_parked(&mut self) {
        let _ = self.client.rt.poll(Processor::stop);
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let Self {
            client: this,
            tempo,
        } = self;
        let size = io.frames();
        // The MIDI a reset or a re-prepare dropped with the chunk in flight.
        if this.owed_lost > 0 {
            io.count_lost(std::mem::take(&mut this.owed_lost));
        }
        if this.rate_refused {
            // The plugin was not told this node's rate (`set_rate`): silence,
            // not audio at another rate.
            return Status::Silent;
        }
        let ports = this.declared.ports;
        let n_in = ports.audio_in().count() as usize;
        let n_out = ports.audio_out().count() as usize;
        // The plugin's channels are a list in its bus order: the graph's
        // buffers are reached a bus at a time, in that order.
        let (ins_by_bus, outs_by_bus): (Handles<InBus>, Handles<OutBus>) = (
            ports.input_handles().collect(),
            ports.output_handles().collect(),
        );
        let features = this.features;
        // Between blocks: take a Processor on offer, or hand this one back
        // (stopped) when the control side recalled it. Taken once, here, so
        // its chunk (`Processor::chunk`) is read from the one the block runs.
        let mut processor = this.rt.poll(Processor::stop);
        let chunk = processor
            .as_deref()
            .and_then(Processor::chunk)
            .map(NonZeroUsize::get);
        // No clamp and no size check: the Processor splits a block longer than
        // the one the plugin was told.
        let events = if io.event_input_count() > 0 {
            io.events(0)
        } else {
            SortedEvents::EMPTY
        };
        let sends = features.contains(Features::MIDI_OUT) && io.event_output_count() > 0;
        // One lend of the project's tempo map for the block, ended with it.
        let map = tempo.lend();
        let mut view = map.view(cx.env.sample_rate);
        let mut lost = 0u64;

        if let Some(chunk) = chunk {
            // **A chunked Processor** (`Processor::chunk`): the block is cut at
            // the chunk edges, counted from `prepare` and every `reset`, and
            // each piece is one `process`, with the transport at its first
            // frame and the changes inside it. A piece of a chunk begins the
            // delivery's chunk at the chunk's first frame and ends it at its
            // last: the parameter changes in the context are the chunk's so
            // far, on chunk frames.
            let mut phase = this.phase.get();
            if phase >= chunk {
                // A Processor with another chunk took over.
                phase = 0;
            }
            this.out_events.clear();
            let mut block = this.transport.lend(features);
            let (inputs, mut outputs) = io.split();
            // The cause of the last piece the Processor failed, for the log.
            let mut failure = None;
            let mut harmony_dropped = 0u64;
            let mut out_dropped = 0u64;
            // Whether every piece of the block was asleep (`asleep`): one that
            // was not, or did not run, keeps the node called.
            let mut slept = true;
            let mut at = 0;
            while at < size {
                let n = (chunk - phase).min(size - at);
                if phase == 0 {
                    this.params.begin();
                    this.intake.new_chunk();
                    this.segments.begin(chunk);
                }
                // The host's writes since the last piece, on this piece's
                // first frame; a node with no Processor leaves them in their
                // slots.
                if processor.is_some() {
                    take_edits(this.edits.as_ref(), &mut this.params, phase);
                }
                this.midi.clear();
                this.chords.changes.clear();
                this.scales.changes.clear();
                let midi = &mut this.midi;
                let intake = &mut this.intake;
                let (chords, scales) = (&mut this.chords, &mut this.scales);
                let mut midi_dropped = 0u64;
                param_input::take(
                    events.as_slice(),
                    events.params(),
                    at,
                    n,
                    phase,
                    cx,
                    &mut this.segments,
                    &mut this.params,
                    |e, frame| match e.kind {
                        EventKind::Midi(ump) => {
                            // On the piece's frames; `frame` is the chunk's.
                            let event = WireMidi {
                                offset: (frame - phase) as u32,
                                ump,
                            };
                            midi_dropped += intake.push_displacing(midi, event);
                        }
                        EventKind::Harmony(h) => {
                            harmony_dropped +=
                                push_harmony(chords, scales, features, h, frame - phase);
                        }
                        EventKind::Resync => {}
                    },
                );
                this.params.count_midi_dropped(midi_dropped);
                lost += midi_dropped;

                // The transport across the chunk, one walk over its pieces as
                // the served node walked it: read at the chunk's first frame,
                // scanned for changes in each piece (on chunk frames), and
                // finished at the piece that completes the chunk, which writes
                // the slope to the chunk's end.
                let completes = phase + n == chunk;
                if phase == 0 {
                    this.changes.clear();
                }
                if features.contains(Features::TRANSPORT) {
                    let (meter, steady, project, block_ns) = block.parts();
                    if phase == 0 {
                        // `at < size` here: the piece has frames.
                        let offset = Offset::new(Samples::new(at), Samples::new(size))
                            .expect("inside the block");
                        let mut s = Snapshot::at(cx, offset, steady.at(at), project, block_ns);
                        let settled = s.settle(&mut view);
                        this.walk.begin(&s, settled);
                        this.chunk_info = transport_info(&s, meter);
                    }
                    let call = Call {
                        cx,
                        frames: size,
                        steady,
                        block_ns,
                        meter,
                    };
                    let over = this.walk.scan(
                        &call,
                        &mut view,
                        project,
                        &mut this.chunk_info,
                        &mut this.changes,
                        at,
                        n,
                        phase,
                    );
                    if completes {
                        this.walk
                            .finish(&mut view, chunk, &mut this.chunk_info, &mut this.changes);
                    }
                    this.params.count_transport_dropped(over);
                } else if phase == 0 {
                    this.chunk_info = TransportInfo::new(cx.env.sample_rate);
                }

                let piece_midi = this.midi.len();
                let ins: ChannelTable<&[F]> = channels_in(&inputs, &ins_by_bus)
                    .take(n_in)
                    .map(|ch| &ch[at..at + n])
                    .collect();
                let mut outs: ChannelTable<&mut [F]> = outputs
                    .buses(&outs_by_bus)
                    .flat_map(BusOutput::into_channels)
                    .take(n_out)
                    .map(|out| &mut out[at..at + n])
                    .collect();
                // A load since a write was drained voids it before the chunk goes.
                void_stale_edits(this.edits.as_ref(), &mut this.params);
                let rendered = render_with(
                    processor.as_deref_mut(),
                    this.params.losses(),
                    &this.params,
                    &mut this.midi,
                    &mut this.out,
                    Inputs {
                        transport: &this.chunk_info,
                        changes: this.changes.as_slice(),
                        expressive: expressive(features, &this.chords, &this.scales),
                    },
                    &ins,
                    &mut outs,
                    n,
                );
                if rendered != Rendered::Ran {
                    silence(&mut outs, n);
                }
                if let Rendered::Failed(why) = rendered {
                    failure = Some(why);
                }
                slept &= rendered == Rendered::Ran && asleep(&this.out, &outs);
                if rendered == Rendered::Ran {
                    // In the chunk the Processor holds until it ships it.
                    this.chunk_midi += piece_midi as u64;
                    this.params.refused(this.out.params_refused() as u64);
                    let losses = this.params.losses();
                    losses
                        .entries_refused
                        .add(this.out.entries_refused() as u64);
                    losses.inputs_refused.add(this.out.input_refused() as u64);
                    // What the Processor shipped in this call, and lost.
                    let reported = this.out.midi_lost() as u64;
                    if reported > 0 {
                        this.params.count_midi_dropped(reported);
                        this.intake.lost();
                        lost += reported;
                    }
                    if completes {
                        this.params.handed_off(!this.out.params_unsent());
                    }
                    if sends {
                        let out_events = &mut this.out.midi_events;
                        tutti_plugin_types::insertion_sort_by_key(out_events, |e| e.offset);
                        for e in out_events.iter() {
                            let frame = (at + e.offset as usize).min(size - 1);
                            // Past the block's room the event is lost, and counted.
                            let fits = this
                                .out_events
                                .try_push(WireMidi {
                                    offset: frame as u32,
                                    ump: e.ump,
                                })
                                .is_ok();
                            out_dropped += u64::from(!fits);
                        }
                    }
                } else {
                    // The chunk cannot be handed over whole: its values are
                    // owed to the next, its MIDI is lost with the piece, and
                    // the SysEx open in it is not completed.
                    this.params.handed_off(false);
                    this.params.count_midi_dropped(piece_midi as u64);
                    lost += piece_midi as u64;
                    this.intake.lost();
                }
                if completes {
                    this.chunk_midi = 0;
                }
                phase = if completes { 0 } else { phase + n };
                at += n;
            }
            this.phase = Samples::new(phase);
            drop(block);
            this.transport.advance(size);
            io.count_lost(lost);
            let losses = this.params.losses();
            losses.harmony.add(harmony_dropped);
            losses.midi_out.add(out_dropped);
            if let Some(why) = failure {
                io.log_plugin(RtLevel::Warn, why, Offset::ZERO, &[]);
            }
            if sends {
                let writer = io.event_out(0);
                let block = Samples::new(size);
                for e in this.out_events.iter() {
                    let _ = writer.push(Event::midi(Offset::clamped(e.offset, block), e.ump));
                }
            }
            // A block that follows an Idle one reads the answers of chunks
            // shipped before the node parked, so the plugin's word on what
            // this one shipped (a wake's) comes a block later: it is not
            // parked again before then.
            let stale = std::mem::take(&mut this.idled);
            this.idled = slept && this.out_events.is_empty() && !stale;
            return slept_status(&io, this.idled);
        }

        // The event input: its MIDI, in its (sorted) order, past the cap
        // dropped and counted (the list is inline and cannot grow; a SysEx
        // that lost a packet loses the rest of it, `MidiIntake`); its
        // parameter events
        // into the block's changes (`param_input`, the out-of-process node's
        // walk, with the block as the chunk).
        this.midi.clear();
        this.chords.changes.clear();
        this.scales.changes.clear();
        this.params.begin();
        if processor.is_some() {
            take_edits(this.edits.as_ref(), &mut this.params, 0);
        }
        let midi = &mut this.midi;
        let intake = &mut this.intake;
        let (chords, scales) = (&mut this.chords, &mut this.scales);
        intake.new_chunk();
        let mut midi_dropped = 0u64;
        let mut harmony_dropped = 0u64;
        param_input::take(
            events.as_slice(),
            events.params(),
            0,
            size,
            0,
            cx,
            &mut this.segments,
            &mut this.params,
            |e, frame| match e.kind {
                EventKind::Midi(ump) => {
                    let event = WireMidi::at(e.offset, ump);
                    midi_dropped += intake.push_displacing(midi, event);
                }
                // A chord or scale, for a plugin that takes sequencer context
                // (VST 2.4 has no such event, and declares none).
                EventKind::Harmony(h) => {
                    harmony_dropped += push_harmony(chords, scales, features, h, frame);
                }
                // The walk keeps parameter events for itself; a plugin
                // re-sends nothing.
                EventKind::Resync => {}
            },
        );
        this.params.count_midi_dropped(midi_dropped);
        this.params.losses().harmony.add(harmony_dropped);
        io.count_lost(midi_dropped);
        // Built before the plugin is entered: the Processor publishes it to the
        // cell `audioMasterGetTime` reads, then calls `process`.
        let transport = this.transport.block(cx, size, features, &mut view);
        // The graph's buffers go to the plugin as they are: the Processor
        // reads them through the channel tables below. Scoped so the tables
        // release `io` before the event output is taken.
        let block_midi = this.midi.len();
        let rendered = {
            let (inputs, mut outputs) = io.split();
            let ins: ChannelTable<&[F]> = channels_in(&inputs, &ins_by_bus)
                .take(n_in)
                .map(|ch| &ch[..size])
                .collect();
            let mut outs: ChannelTable<&mut [F]> = outputs
                .buses(&outs_by_bus)
                .flat_map(BusOutput::into_channels)
                .take(n_out)
                .map(|out| &mut out[..size])
                .collect();
            // A load since a write was drained or owed voids it before the block goes.
            void_stale_edits(this.edits.as_ref(), &mut this.params);
            let rendered = render_with(
                processor,
                this.params.losses(),
                &this.params,
                &mut this.midi,
                &mut this.out,
                Inputs {
                    transport: &transport,
                    changes: &[],
                    expressive: expressive(features, &this.chords, &this.scales),
                },
                &ins,
                &mut outs,
                size,
            );
            if rendered != Rendered::Ran {
                silence(&mut outs, size);
            }
            // Read with the buffers still lent: a sleep claims silent output.
            let slept = rendered == Rendered::Ran && asleep(&this.out, &outs);
            (rendered, slept)
        };
        let (rendered, slept) = rendered;
        let processed = rendered == Rendered::Ran;
        if let Rendered::Failed(why) = rendered {
            io.log_plugin(RtLevel::Warn, why, Offset::ZERO, &[]);
        }
        if processed {
            // The writes the plugin refused, counted dropped.
            this.params.refused(this.out.params_refused() as u64);
            let losses = this.params.losses();
            losses
                .entries_refused
                .add(this.out.entries_refused() as u64);
            losses.inputs_refused.add(this.out.input_refused() as u64);
            // What a Processor says it lost (none that is not chunked does).
            let reported = this.out.midi_lost() as u64;
            if reported > 0 {
                this.params.count_midi_dropped(reported);
                this.intake.lost();
                io.count_lost(reported);
            }
        }
        // Set on the plugin with the block, or owed to the next one.
        this.params
            .handed_off(processed && !this.out.params_unsent());
        if !processed {
            // The block's MIDI was dropped with it (`render_with`): counted,
            // and the rest of every SysEx open in it is skipped.
            this.params.count_midi_dropped(block_midi as u64);
            io.count_lost(block_midi as u64);
            this.intake.lost();
        }
        if sends && processed {
            let out_events = &mut this.out.midi_events;
            tutti_plugin_types::insertion_sort_by_key(out_events, |e| e.offset);
            let writer = io.event_out(0);
            let block = Samples::new(size);
            for e in out_events.iter() {
                let at = (e.offset as usize).min(size - 1);
                if let Some(at) = Offset::new(Samples::new(at), block) {
                    let _ = writer.push(Event::midi(at, e.ump));
                }
            }
        }
        slept_status(&io, slept && !sends)
    }

    fn reset(&mut self) {
        let this = &mut self.client;
        // The segments in flight stop, as on the out-of-process node; what
        // was delivered to the plugin is kept (the plugin keeps its
        // parameters), and no block's changes are in flight between blocks.
        this.segments.clear();
        this.params.abandon();
        // The Processor's chunk starts over, so a block is cut from here.
        this.phase = Samples::new(0);
        this.drop_chunk();
        // A SysEx open across the reset is not completed past it: its rest
        // is skipped (the Processor drops its own open message, counted).
        this.intake.lost();
        // The Processor clears its own signal state (`Processor::reset`:
        // per format, what the plugin can be asked, and the host's SysEx
        // reassembly). Between blocks on the audio thread, and only if it
        // holds the Processor: the control side holds it while it is recalled,
        // and a rate change that brings the new one starts it clear.
        if let Some(processor) = this.rt.held() {
            processor.reset();
        }
    }

    /// Hands the plugin's host a wake that raises the node's: a plugin that
    /// slept and later asks to be processed again (CLAP's `request_process`)
    /// runs on the next block even though the executor parked the node.
    /// Control thread.
    fn bind_wake(&mut self, wake: tutti_graph::Wake) {
        let mut instance = self.client.inner.lock();
        if let Some(control) = instance.control() {
            control.bind_wake(tutti_plugin_types::ProcessWake::new(move || wake.notify()));
        }
    }
}

/// Whether the plugin said it is asleep this block, and its audio and MIDI
/// say so too: CLAP's *sleep* means the output is silent, and the executor
/// writes silence for a node it parks, so a plugin that sleeps over sound is
/// not believed.
fn asleep<F: Float>(out: &ProcessOutput, outs: &[&mut [F]]) -> bool {
    out.status() == BlockStatus::Sleep
        && out.midi_events.is_empty()
        && outs.iter().all(|c| c.iter().all(|&x| x == F::lit(0.0)))
}

/// The status a block ends with. A plugin that slept
/// through the whole block rings out nothing: **no tail, and [`Status::Idle`]**,
/// which is what lets the executor park a node with an event input (every
/// instrument): the executor still calls it the moment a note or any input
/// arrives, at that block, so a sleeping plugin is woken on time. Everything
/// else reports nothing: *continue*, *continue if not quiet* and *tail* all
/// leave the declared ceiling standing (a call that reports nothing restores
/// it, which is what a tail status would report), and a format with no
/// per-block status never says anything.
fn slept_status<F: Float>(io: &Io<'_, F>, slept: bool) -> Status {
    if slept {
        io.report_tail(Tail::None);
        Status::Idle
    } else {
        Status::Modified
    }
}

/// The sequencer context a block carries, for a plugin that declared it.
fn expressive<'a>(
    features: Features,
    chords: &'a ChordChanges,
    scales: &'a ScaleChanges,
) -> Option<ExpressiveContext<'a>> {
    features
        .contains(Features::SEQUENCER_CONTEXT)
        .then_some(ExpressiveContext {
            chords: Some(chords),
            scales: Some(scales),
            ..ExpressiveContext::default()
        })
}

/// A chord or scale into the piece's harmony at its frame `frame`, for a plugin
/// that takes sequencer context; nothing for one that does not. Past the cap
/// ([`HARMONY_CAPACITY`](tutti_plugin_types::HARMONY_CAPACITY)) it is dropped:
/// returns how many were (one or none), for the node to count.
/// No display text: the degrees are what a plugin acts on.
fn push_harmony(
    chords: &mut ChordChanges,
    scales: &mut ScaleChanges,
    features: Features,
    h: Harmony,
    frame: usize,
) -> u64 {
    if !features.contains(Features::SEQUENCER_CONTEXT) {
        return 0;
    }
    let sample_offset = i32::try_from(frame).unwrap_or(i32::MAX);
    let (root, bass, mask) = (
        i16::from(h.root()),
        i16::from(h.bass()),
        i16::try_from(h.degrees()).unwrap_or(0),
    );
    let kept = match h.kind() {
        HarmonyKind::Chord => chords.add_change(ChordValue {
            sample_offset,
            root,
            bass_note: bass,
            mask,
            text: String::new(),
        }),
        HarmonyKind::Scale => scales.add_change(ScaleValue {
            sample_offset,
            root,
            mask,
            text: String::new(),
        }),
    };
    u64::from(!kept)
}

/// The plugin, inserted as a graph node. Its controls ([`NodeControls`]) are
/// its meter cell (the same cell as [`NodeControls::set_meter`]), so a host can
/// give it the project meter after it has moved into the graph, and its
/// parameter delivery's counters, as an out-of-process plugin's
/// `PluginControls` are. Its [`ForkSource`] forks it **by state transfer**: a
/// second instance of the plugin from the same library, restored from this
/// one's saved state ([`PluginNode::fork_instance`]). So `Editor::fork` (an
/// offline export) renders a graph holding it.
///
/// [`ForkSource`]: tutti_graph::ForkSource
impl<L: InProcess + 'static, F: Float> IntoNode<F> for PluginNode<L, F>
where
    L::Processor: 'static,
{
    type Controls = NodeControls;

    fn into_parts(self) -> NodeParts<NodeControls, F> {
        // A plugin with no origin to load again has no fork.
        let fork = super::fork::Fork::of(&self).map(|fork| Box::new(fork) as _);
        let controls = self.client.controls();
        NodeParts {
            node: Box::new(self),
            controls,
            fork,
        }
    }
}

impl<L: InProcess, F: Float> InProcessClient<L, F> {
    /// The file and plugin the load opened: its id is the plugin's, whatever the
    /// load chose. `None` for a client built over a plugin that has no origin.
    #[must_use]
    pub fn origin(&self) -> Option<&tutti_plugin_types::PluginRef> {
        self.reload.as_ref().map(|reload| &reload.origin)
    }

    /// The node's controls: its meter cell and its parameter delivery's
    /// counters, the same [`NodeControls`] inserting it hands back. Its block
    /// is the out-of-process node's chunk: a block's points all count as sent,
    /// though a format may take only each parameter's last one, and a block
    /// the plugin did not run (no Processor in hand, or one that failed it;
    /// see [`BlockLosses`](crate::BlockLosses)) loses its MIDI, counted.
    pub fn controls(&self) -> NodeControls {
        NodeControls::new(self.transport.meter(), self.params.counters())
    }
}

/// The owed path needs a block with no Processor in hand, which only code
/// inside the crate can bring about on cue (a rate change that recalls the
/// Processor, through `InProcessClient::inner`); the rest of the
/// node's parameter delivery is pinned against the public API in
/// `tests/vst2_in_process_params.rs`.
#[cfg(test)]
mod owed_tests {
    use tutti_fixtures::main_thread;
    use tutti_plugin_types::MainThreadMarker;
    use tutti_vst2_host::Vst2Loaded;

    use tutti_graph::contract::Direct;
    use tutti_graph::{Event, Offset, ParamEvent, ParamRamp};
    use tutti_units::SampleRate;

    use crate::InProcessClient;
    use crate::PluginHandle;
    use tutti_units::ParamId;

    /// The reference VST2 plugin through the generic loader.
    fn load_client(
        main: MainThreadMarker,
        path: &std::path::Path,
        rate: SampleRate,
    ) -> tutti_plugin_types::IpcResult<(InProcessClient<Vst2Loaded>, PluginHandle)> {
        crate::load_client::<Vst2Loaded, f32>(
            main,
            &tutti_fixtures::pumped_work(),
            &tutti_plugin_types::PluginRef::file(path),
            rate,
        )
        .map_err(|e| e.into_bridge())
    }

    const BLOCK: usize = 64;

    fn read(handle: &PluginHandle, i: i32) -> Option<f64> {
        futures_lite::future::block_on(handle.any_thread().parameter_value(ParamId::new(i as u32)))
            .flatten()
            .map(|v| v.get())
    }

    /// **A fork told its rate off the main thread is refused, not left at the
    /// wrong rate.** A fork's `prepare` dispatches the rate under the main
    /// thread's proof; off that thread it cannot, so the node records the
    /// refusal, renders silence, and only a `prepare` on the main thread clears it.
    ///
    /// The silence `process` renders while refused is not covered here.
    #[test]
    fn a_fork_prepared_off_the_main_thread_is_refused() {
        let main = main_thread();
        let (client, _handle) = load_client(main, tutti_fixtures::vst2_probe(), SampleRate::SR_48K)
            .unwrap_or_else(|e| panic!("in-process load of the probe: {e:?}"));
        let fork = client
            .bind(tutti_fixtures::tempo())
            .fork_instance(main, tutti_graph::ForkMode::Live)
            .expect("the fork loads on the main thread");
        let (fork, refused) = std::thread::spawn(move || {
            let mut fork = fork;
            fork.set_rate(SampleRate::SR_96K);
            let refused = fork.rate_refused;
            (fork, refused)
        })
        .join()
        .expect("the thread returns");
        assert!(refused, "off the main thread the rate cannot be told");
        // The harness thread is not the process's main thread either, so a second
        // `prepare` is refused too: only the main thread's clears the refusal.
        let mut fork = fork;
        fork.set_rate(SampleRate::SR_96K);
        assert!(
            fork.rate_refused,
            "a prepare off the main thread cannot clear it"
        );
    }

    /// **A render mode a prepare off the main thread could not apply is
    /// reported through the handle's invalidations**, as the rate's refusal is,
    /// and is told by the next prepare on the main thread. The mode change is a
    /// main-thread call on every format.
    #[test]
    fn a_render_mode_prepared_off_the_main_thread_is_reported() {
        let main = main_thread();
        let (client, _handle) = load_client(main, tutti_fixtures::vst2_probe(), SampleRate::SR_48K)
            .unwrap_or_else(|e| panic!("in-process load of the probe: {e:?}"));
        let fork = client
            .bind(tutti_fixtures::tempo())
            .fork_instance(main, tutti_graph::ForkMode::Live)
            .expect("the fork loads on the main thread");
        let prepare = tutti_graph::Prepare::new(
            SampleRate::SR_48K,
            tutti_buffers::MaxBlock::new(tutti_units::Samples::new(BLOCK)).expect("a block"),
        )
        .with_render_mode(tutti_graph::RenderMode::Offline);
        let fork = std::thread::spawn(move || {
            let mut fork = fork;
            fork.set_render_mode_from(&prepare);
            fork
        })
        .join()
        .expect("the thread returns");
        let reports = fork.inner.lock().pump(main).reports;
        assert!(
            matches!(
                reports.as_slice(),
                [tutti_plugin_types::PluginInvalidation::RenderModeNotApplied { mode }]
                    if *mode == tutti_graph::RenderMode::Offline
            ),
            "the refused mode is reported once: {reports:?}"
        );
        assert!(fork.inner.lock().pump(main).reports.is_empty(), "told once");
    }

    /// Starts a rate change the way one pending for the main thread's pump
    /// does: the Processor is recalled, so the node's next block hands it
    /// back and renders without it.
    fn recall(inner: &tutti_rt::Mutex<crate::node::instance::Instance<Vst2Loaded>>) {
        inner
            .lock()
            .set_sample_rate(main_thread(), SampleRate::SR_44K1, None)
            .expect("the change starts");
    }

    /// Finishes it: the pump collects the Processor and lends the new one.
    fn pump(inner: &tutti_rt::Mutex<crate::node::instance::Instance<Vst2Loaded>>) {
        let _ = inner.lock().pump(main_thread());
    }

    /// **A block with no Processor in hand owes its values to the next.** A
    /// rate change recalls the Processor across one block that ramps
    /// parameter 1 to 0.75: the block renders silent (one unlent block), the
    /// plugin keeps its default (`1 / 16`, the probe's), nothing is counted
    /// sent and the block is counted deferred. Once the pump has lent the new
    /// Processor, the next block carries no event, and the owed value reaches
    /// the plugin on it.
    #[test]
    fn an_unlent_block_owes_its_values_to_the_next() {
        let (client, handle) = load_client(
            main_thread(),
            tutti_fixtures::vst2_probe(),
            SampleRate::SR_48K,
        )
        .unwrap_or_else(|e| {
            panic!(
                "in-process load of the probe at {:?}: {e:?}",
                tutti_fixtures::vst2_probe()
            )
        });
        let inner = std::sync::Arc::clone(&client.inner);
        let controls = client.controls();
        let mut unit = Direct::new(
            client.bind(tutti_fixtures::tempo()),
            SampleRate::SR_48K,
            BLOCK,
        );
        assert_eq!(read(&handle, 1), Some(1.0 / 16.0), "the probe's default");
        // The realtime end takes the Processor it was lent.
        unit.block();

        recall(&inner);
        unit.param_events(
            0,
            &[ParamEvent::ramp(
                Offset::of(
                    tutti_units::Samples::new(0),
                    tutti_units::Samples::new(BLOCK),
                ),
                ParamRamp::by_id(ParamId::new(1), 0.75, tutti_units::Samples::new(0)),
            )],
        );
        unit.block();
        pump(&inner);
        assert_eq!(
            controls.block_losses().unlent,
            1,
            "the block had no Processor"
        );
        assert_eq!(read(&handle, 1), Some(1.0 / 16.0), "not yet delivered");
        let stats = controls.param_delivery();
        assert_eq!(stats.chunks_deferred, 1);
        assert_eq!(stats.points_sent, 0);

        unit.block();
        assert_eq!(read(&handle, 1), Some(0.75), "owed to the next block");
        let stats = controls.param_delivery();
        assert_eq!(stats.points_sent, 1);
        assert_eq!(stats.chunks_deferred, 1);
        assert_eq!(stats.points_dropped, 0);
    }

    /// A 60-byte SysEx's ten packets, then a 12-byte one's two, all on
    /// group 1, as graph events at frame 0.
    fn dumps() -> (Vec<Event>, Vec<Event>) {
        use tutti_ump::MidiGroup;
        let packets = |len: usize| {
            let payload: Vec<u8> = (0..len).map(|i| (i % 0x7F) as u8).collect();
            let mut out = Vec::new();
            out.extend(
                tutti_ump::Sysex7Packets::from_payload(MidiGroup::FIRST, &payload)
                    .expect("a 7-bit payload"),
            );
            let at = Offset::of(
                tutti_units::Samples::new(0),
                tutti_units::Samples::new(BLOCK),
            );
            out.iter().map(|e| Event::midi(at, *e)).collect::<Vec<_>>()
        };
        (packets(60), packets(12))
    }

    /// **A block with no Processor in hand loses its MIDI, counted, and the
    /// SysEx open in it is not completed by its tail.** The dump's first three
    /// packets reach the plugin; the next three go with a block whose
    /// Processor was recalled; the tail and a second dump follow on the new
    /// Processor. The node skips the cut dump's tail; the plugin's host never
    /// completes the cut dump: it drops it at the second dump's `START`.
    #[test]
    fn an_unlent_block_cuts_the_sysex_open_in_it() {
        let (client, _handle) = load_client(
            main_thread(),
            tutti_fixtures::vst2_probe(),
            SampleRate::SR_48K,
        )
        .unwrap_or_else(|e| {
            panic!(
                "in-process load of the probe at {:?}: {e:?}",
                tutti_fixtures::vst2_probe()
            )
        });
        let inner = std::sync::Arc::clone(&client.inner);
        let controls = client.controls();
        let mut unit = Direct::new(
            client.bind(tutti_fixtures::tempo()),
            SampleRate::SR_48K,
            BLOCK,
        );
        let (cut, next) = dumps();

        unit.events(0, &cut[..3]);
        unit.block();
        recall(&inner);
        unit.events(0, &cut[3..6]);
        unit.block();
        pump(&inner);
        let tail: Vec<Event> = cut[6..].iter().chain(&next).copied().collect();
        unit.events(0, &tail);
        unit.block();

        // The host keeps one SysEx reassembler across activations, so the
        // cut dump is still open on it, and its second `START` drops it.
        let fresh = unit.node.rt.held().expect("the new Processor");
        assert_eq!(fresh.dropped_sysex().get(), 1, "the cut dump, whole");
        assert_eq!(
            controls.dropped_midi_in(),
            (3 + cut.len() - 6) as u64,
            "the unlent block's, and the cut dump's tail"
        );
    }

    /// **A SysEx open across the node's reset is not completed past it**:
    /// its tail is skipped, counted, and the plugin's host drops it at the
    /// next `START`, as the CLAP and VST3 hosts do on a plugin reset.
    #[test]
    fn a_sysex_open_across_a_reset_is_not_completed() {
        use tutti_graph::Node as _;
        let (client, _handle) = load_client(
            main_thread(),
            tutti_fixtures::vst2_probe(),
            SampleRate::SR_48K,
        )
        .unwrap_or_else(|e| {
            panic!(
                "in-process load of the probe at {:?}: {e:?}",
                tutti_fixtures::vst2_probe()
            )
        });
        let controls = client.controls();
        let mut unit = Direct::new(
            client.bind(tutti_fixtures::tempo()),
            SampleRate::SR_48K,
            BLOCK,
        );
        let (cut, next) = dumps();

        unit.events(0, &cut[..3]);
        unit.block();
        unit.node.reset();
        let tail: Vec<Event> = cut[3..].iter().chain(&next).copied().collect();
        unit.events(0, &tail);
        unit.block();

        let processor = unit.node.rt.held().expect("lent");
        assert_eq!(processor.dropped_sysex().get(), 1, "the cut dump, whole");
        assert_eq!(controls.dropped_midi_in(), (cut.len() - 3) as u64);
        assert_eq!(
            unit.node.controls().block_losses().inputs_refused,
            1,
            "the dump the plugin's host dropped whole, counted in its own unit"
        );
    }

    /// **The plugin's host dropping a cut dump does not cut the next one.** A dump cut by a
    /// reset is dropped whole by the plugin's host at the next dump's `START`; that drop is
    /// the host's, counted in its own unit, and the node's open SysEx (the new dump, here
    /// spanning two blocks) is not skipped for it: its `END` reaches the plugin, and no event
    /// beyond the reset's tail counts as lost.
    #[test]
    fn a_host_drop_does_not_cut_the_next_dump() {
        use tutti_graph::Node as _;
        let (client, _handle) = load_client(
            main_thread(),
            tutti_fixtures::vst2_probe(),
            SampleRate::SR_48K,
        )
        .unwrap_or_else(|e| panic!("in-process load of the probe: {e:?}"));
        let controls = client.controls();
        let mut unit = Direct::new(
            client.bind(tutti_fixtures::tempo()),
            SampleRate::SR_48K,
            BLOCK,
        );
        let (cut, next) = dumps();
        assert!(next.len() >= 2, "fixture: a START and an END");

        unit.events(0, &cut[..3]);
        unit.block();
        unit.node.reset();
        unit.events(0, &cut[3..]);
        unit.block();
        let lost = controls.dropped_midi_in();
        assert_eq!(lost, (cut.len() - 3) as u64, "the reset's tail only");

        unit.events(0, &next[..1]);
        unit.block();
        unit.events(0, &next[1..]);
        unit.block();

        assert_eq!(
            controls.dropped_midi_in(),
            lost,
            "the new dump's END was not skipped for the host's drop"
        );
        assert_eq!(
            unit.node.controls().block_losses().inputs_refused,
            1,
            "the cut dump, whole"
        );
        assert_eq!(
            unit.node.rt.held().expect("lent").dropped_sysex().get(),
            1,
            "the host's own count"
        );
    }
}

#[cfg(test)]
mod harmony_tests {
    use super::*;
    use tutti_plugin_types::HARMONY_CAPACITY;

    /// **A chord and a scale become the plugin's chord and scale changes**,
    /// at their frames, for a plugin that takes sequencer context; a plugin
    /// that does not gets none. Past the inline capacity the rest are dropped
    /// (no allocation).
    #[test]
    fn harmony_becomes_chord_and_scale_changes_for_a_plugin_that_takes_it() {
        let (mut chords, mut scales) = (ChordChanges::default(), ScaleChanges::default());
        let takes = Features::SEQUENCER_CONTEXT;
        let chord = Harmony::chord(65, 69, 0b1001_0001);
        push_harmony(&mut chords, &mut scales, takes, chord, 12);
        let scale = Harmony::scale(69, 0b0101_1010_1101);
        push_harmony(&mut chords, &mut scales, takes, scale, 30);
        let c = &chords.changes[0];
        assert_eq!(
            (c.sample_offset, c.root, c.bass_note, c.mask),
            (12, 65, 69, 0b1001_0001)
        );
        let s = &scales.changes[0];
        assert_eq!(
            (s.sample_offset, s.root, s.mask),
            (30, 69, 0b0101_1010_1101)
        );

        let (mut none_chords, mut none_scales) = (ChordChanges::default(), ScaleChanges::default());
        let chord = Harmony::chord(60, 60, 1);
        push_harmony(
            &mut none_chords,
            &mut none_scales,
            Features::empty(),
            chord,
            0,
        );
        assert!(none_chords.changes.is_empty());

        for i in 0..100 {
            push_harmony(
                &mut chords,
                &mut scales,
                takes,
                Harmony::chord(60, 60, 1),
                i,
            );
        }
        assert_eq!(chords.changes.len(), HARMONY_CAPACITY, "kept to the cap");
    }
}
