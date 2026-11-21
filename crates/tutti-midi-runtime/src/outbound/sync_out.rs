//! [`SyncOut`]: the transport sent to other gear (Beat Clock, MIDI Time Code
//! and MIDI Machine Control) as a graph node with one event output.
//!
//! It reads the transport at every piece of each block (`Env::segments`), so a
//! start, a stop or a locate inside a block is sent on its own frame, and what
//! follows it follows the transport from there. What it sends is the one
//! setting [`messages`](SyncOutControls::messages) (a [`SyncMessages`]),
//! read once per block; everything is off until a host sets it.
//!
//! - The Beat Clock and MTC are the clock's: Timing Clock, Start, Stop,
//!   Continue and Song Position, the MTC quarter-frames, and an MTC Full Message
//!   on every locate (a cue while stopped, a jump while playing, a loop wrap).
//! - Machine Control is [`MachineControlSender`]'s: PLAY, STOP and LOCATE (with
//!   DEFERRED PLAY while rolling) each on the frame the transport changes, so a
//!   start and a stop in one block both go. The first block after MMC is turned
//!   on only looks. A loop wrap inside a block is a locate on the next block's
//!   first frame, where the position is first seen to have moved: at most one
//!   block late, unlike the clock's and MTC's, which are on the wrap's own
//!   frame. A look sees only a position, so several wraps in one block can
//!   leave it where rolling would have, and then no Locate goes at all.
//! - Flex Data tempo, meter and metronome are the clock's too
//!   ([`SyncMessages::flex`], [`SyncMessages::metronome`]): the meter is read
//!   from the cell [`meter`](SyncOut::meter) names (4/4 until one is).
//! - **One rate**: MTC and an MMC LOCATE state time codes at
//!   [`SyncMessages::rate`], so the two cannot disagree.
//!
//! Wire its event output to a [`MidiOutNode`](crate::MidiOutNode) with an
//! ordinary edge; nothing about it is special to a tap. A message goes out as
//! packets in one run, whole or not at all: a block with no room for all of a
//! message's packets counts it lost, so an output never carries half a SysEx
//! run. A transport time no time code names (before zero, NaN, a day or more) is
//! counted refused and sends no MMC LOCATE ([`SyncOutControls::losses`]); a beat
//! the Song Position Pointer cannot name (before the start, or past its 14-bit
//! field, 4096 beats) is counted refused and sends no pointer.
//!
//! **A follower and a sender on one machine forward**: a transport that a
//! follower moved is sent on like any other move. That is a relay, not guarded
//! against; a device that echoes MMC back to the follower of the same machine
//! loops.
//!
//! A fork (a bounce) runs: its events are data, and the output node it feeds
//! forks as a node that sends nothing.

use std::sync::Arc;
use tutti_buffers::Float;

use tutti_graph::{
    Cx, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Losses, Node, NodeParts,
    Offset, Prepare, Shape, Status, UmpAt, declare_params,
};
use tutti_midi_protocol::{MtcRate, Recipient};
use tutti_music::MeterMap;
use tutti_rt::{AtomicCell, Counter, CounterWriter, Publisher, Reader, RtVec, counter};
use tutti_ump::{MidiGroup, Sysex7Packets};
use tutti_units::{Beat, SampleRate};

use super::clock_master::ClockMaster;
use super::machine_control_out::MachineControlSender;
use super::sync_messages::SyncMessages;
use crate::live::order::{in_offset_order, offset_of};

/// The most events a [`SyncOut`] sends in one block, its declared event
/// capacity. At 300 BPM and 30 fps an 8 192-frame block at 44.1 kHz holds about
/// 23 ticks and 23 quarter-frames; a block holds at most nine pieces, each at
/// most one Machine Control message of two packets (a LOCATE and a DEFERRED
/// PLAY are 12 bytes with their header).
///
/// A loop wrap in the block adds a Song Position, a tick on the loop's start
/// when it is on one and the pieces its restarted grid sends, so a short loop
/// in a long block adds that per wrap. A loop of a sixteenth of a beat (1 500
/// frames at 120 BPM and 48 kHz, 600 at 300 BPM) wraps about 14 times in an
/// 8 192-frame block, well under the cap; it takes a loop of about sixty frames
/// (a millisecond) to reach it. Past it the events of later passes are refused
/// and counted ([`SyncOutControls::losses`]).
pub const SYNC_OUT_EVENT_CAPACITY: usize = 256;

const SYNC_OUT_PORT_CAPACITY: std::num::NonZeroU32 =
    std::num::NonZeroU32::new(SYNC_OUT_EVENT_CAPACITY as u32).unwrap();

/// The Group's clock before the graph's rate arrives with `prepare`.
const UNPREPARED_RATE: SampleRate = SampleRate::SR_48K;

/// A [`SyncOut`]'s cells: what the control side sets and reads, what the node
/// counts. Shared as an `Arc`, the node's controls ([`IntoNode`]).
#[derive(Debug)]
pub struct SyncOutControls {
    /// What is sent: read once per block, set through
    /// [`messages`](Self::messages).
    messages: AtomicCell<SyncMessages>,
    losses: Counter<Losses>,
}

declare_params!([] SyncOutControls, settings SyncSettings {
    messages: SyncMessages = SyncMessages::OFF,
});

impl SyncOutControls {
    /// What the node could not send, since it was made. `refused` counts the
    /// events a block produced past [`SYNC_OUT_EVENT_CAPACITY`] and the looks
    /// at a transport that moved to a time no time code names
    /// ([`NoTimeCode`](crate::NoTimeCode)), and the Song Position Pointers
    /// and MTC positions of a beat their fields cannot hold (past 4096 beats
    /// or before the start); `lost` counts the Machine Control
    /// messages the block had no room for. A host polls it: nonzero means a
    /// receiver missed something and should be resynced.
    #[must_use]
    pub fn losses(&self) -> Losses {
        self.losses.get()
    }
}

/// Where a [`SyncOut`] gets a reader of the project meter: a function that
/// registers one (control side), called for the node and again for each fork.
#[derive(Clone)]
struct MeterSource(Arc<dyn Fn() -> Reader<MeterMap> + Send + Sync>);

impl MeterSource {
    /// A source of a constant meter, 4/4.
    fn constant() -> Self {
        Self(Arc::new(|| Publisher::new(MeterMap::default()).reader()))
    }
}

impl std::fmt::Debug for MeterSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MeterSource")
    }
}

/// The transport sent to other gear, as a graph node with one event output.
/// See the module docs.
#[derive(Debug)]
pub struct SyncOut {
    clock: ClockMaster,
    /// The reader of the project meter, lent once per block when Flex is on.
    meter: Reader<MeterMap>,
    /// Registers another reader on the same meter, for a fork. Control side:
    /// `into_parts` moves it into the fork source, so the audio node holds
    /// only its reader.
    meter_source: Option<MeterSource>,
    sender: MachineControlSender,
    group: MidiGroup,
    controls: Arc<SyncOutControls>,
    /// The writer of [`SyncOutControls::losses`].
    losses: CounterWriter<Losses>,
    /// Where a block's events are gathered, to go out in offset order. Capped
    /// at the port's capacity: past it an event is refused and counted, never
    /// grown into.
    pending: RtVec<UmpAt, SYNC_OUT_EVENT_CAPACITY>,
}

impl SyncOut {
    /// A node that sends on `group`, sending nothing until
    /// [`messages`](SyncOutControls::messages) is set, placing events at the
    /// graph's rate once prepared. Control thread.
    ///
    /// ```
    /// use tutti_graph::IntoNode as _;
    /// use tutti_midi_protocol::{MtcRate, Recipient};
    /// use tutti_midi_runtime::{SyncMessages, SyncOut};
    /// use tutti_ump::MidiGroup;
    ///
    /// // An f32 graph's node; any float format takes the same.
    /// let (_node, controls): (Box<dyn tutti_graph::Node<f32>>, _) =
    ///     SyncOut::new(MidiGroup::masked(2))
    ///         .sending(SyncMessages {
    ///             clock: true,
    ///             mmc: Some(Recipient::AllCall),
    ///             rate: MtcRate::Fps30,
    ///             ..SyncMessages::OFF
    ///         })
    ///         .into_node();
    /// assert!(controls.messages().load().clock);
    /// ```
    #[must_use]
    pub fn new(group: MidiGroup) -> Self {
        let (losses, reader) = counter();
        let meter_source = MeterSource::constant();
        Self {
            clock: ClockMaster::new(group, UNPREPARED_RATE),
            meter: (meter_source.0)(),
            meter_source: Some(meter_source),
            sender: MachineControlSender::new(Recipient::AllCall, MtcRate::Fps25),
            group,
            controls: Arc::new(SyncOutControls {
                messages: AtomicCell::new(SyncSettings::new().messages),
                losses: reader,
            }),
            losses,
            pending: RtVec::new(),
        }
    }

    /// The project meter the Flex time signatures state: `register` makes a
    /// reader on its cell (control side), called now and again for each fork.
    /// 4/4 until set.
    #[must_use]
    pub fn meter(
        mut self,
        register: impl Fn() -> Reader<MeterMap> + Send + Sync + 'static,
    ) -> Self {
        let source = MeterSource(Arc::new(register));
        self.meter = (source.0)();
        self.meter_source = Some(source);
        self
    }

    /// What the node sends from its first block.
    #[must_use]
    pub fn sending(self, messages: SyncMessages) -> Self {
        self.controls
            .messages
            .store(SyncSettings::new().messages(messages).messages);
        self
    }
}

/// Adds one Machine Control message's packets to `pending`, at `at`, whole or
/// not at all: a message with no room is counted lost, one that cannot be
/// packed refused.
fn push_message(
    pending: &mut RtVec<UmpAt, SYNC_OUT_EVENT_CAPACITY>,
    losses: &mut CounterWriter<Losses>,
    group: MidiGroup,
    at: Offset,
    payload: &[u8],
) {
    // A payload the sender built is seven-bit and short.
    let Ok(packets) = Sysex7Packets::from_payload(group, payload) else {
        losses.add(Losses::refused(1));
        return;
    };
    if packets.len() > SYNC_OUT_EVENT_CAPACITY.saturating_sub(pending.len()) {
        losses.add(Losses::lost(1));
        return;
    }
    for packet in packets {
        let _ = pending.push((at, packet));
    }
}

impl<F: Float> Node<F> for SyncOut {
    /// No audio; one event output of [`SYNC_OUT_EVENT_CAPACITY`].
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_event_capacity(SYNC_OUT_PORT_CAPACITY)
            // Event-only: a few pulses a block, nothing per frame (measured
            // about 0.3 µs a block at any length; a gain's 256 frames, 0.2).
            .with_cost(tutti_graph::Cost::Light)
            .with_no_note_events()
    }

    fn prepare(&mut self, p: &Prepare) {
        self.clock.set_sample_rate(p.sample_rate());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let block_len = cx.env.block_len;
        let messages = self.controls.messages.load();
        self.pending.clear();
        let mut dropped = 0u64;
        // Lent once, held across every piece of the block.
        let loan = messages.any_flex().then(|| self.meter.lend());
        for (start, seg) in cx.env.segments() {
            let base = start.get();
            let pending = &mut self.pending;
            self.clock.tick(
                messages,
                loan.as_deref(),
                &seg.transport,
                seg.block_len,
                &mut |(at, ump): UmpAt| {
                    // Never grows on the audio thread: past the capacity the
                    // rest are refused, as the port would refuse them, and
                    // counted for the host. A segment's events lie inside
                    // it, so inside the block.
                    let at = Offset::clamped(base.saturating_add(at.get()), block_len);
                    if !pending.push((at, ump)) {
                        dropped += 1;
                    }
                },
            );
            let refused = self.clock.take_refused();
            if refused > 0 {
                self.losses.add(Losses::refused(refused));
            }
            let at = Offset::clamped(base, block_len);
            match messages.mmc {
                None => self.sender.reset(),
                Some(to) => {
                    self.sender.retarget(to, messages.rate);
                    // The transport's time is its beat at its tempo from beat
                    // 0, the timeline the clock's MTC runs on.
                    let seconds =
                        (seg.transport.beat() - Beat::new(0.0)).to_seconds(seg.transport.tempo);
                    match self.sender.observe(
                        seg.transport.playing,
                        seconds,
                        seg.frame,
                        cx.env.sample_rate,
                    ) {
                        Ok(Some(payload)) => push_message(
                            &mut self.pending,
                            &mut self.losses,
                            self.group,
                            at,
                            &payload,
                        ),
                        Ok(None) => {}
                        Err(_) => self.losses.add(Losses::refused(1)),
                    }
                }
            }
        }
        if dropped > 0 {
            self.losses.add(Losses::refused(dropped));
        }
        in_offset_order(self.pending.as_mut_slice());
        let out = io.event_out(0);
        for e in &self.pending {
            let _ = out.push(Event::midi(offset_of(e, block_len), e.1));
        }
        Status::Modified
    }

    /// Forgets what it saw: the previous tick, the MTC grid, the Flex
    /// statements and the last Machine Control look, so the next block states
    /// where the transport stands afresh. The messages, the rate and the Group
    /// are kept. Allocates nothing.
    fn reset(&mut self) {
        self.clock.reset();
        self.sender.reset();
        self.pending.clear();
    }
}

/// A [`SyncOut`]'s fork: a node of its own over the group and the messages
/// the live one sent at the fork, sharing no cell with it.
struct SyncFork {
    group: MidiGroup,
    controls: Arc<SyncOutControls>,
    meter: MeterSource,
}

impl<F: Float> ForkSource<F> for SyncFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked<F>, ForkCause> {
        let meter = self.meter.clone();
        let node = SyncOut::new(self.group)
            .meter(move || (meter.0)())
            .sending(self.controls.messages.load());
        Ok(Forked::new(Box::new(node)))
    }
}

impl<F: Float> IntoNode<F> for SyncOut {
    /// What it sends, and what it could not.
    type Controls = Arc<SyncOutControls>;

    fn into_parts(mut self) -> NodeParts<Arc<SyncOutControls>, F> {
        let controls = Arc::clone(&self.controls);
        let fork = SyncFork {
            group: self.group,
            controls: Arc::clone(&controls),
            meter: self
                .meter_source
                .take()
                .unwrap_or_else(MeterSource::constant),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}
