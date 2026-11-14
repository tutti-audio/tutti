//! One hardware input port: the producer handle a driver callback pushes
//! through, and the port's entry in the list.
//!
//! [`HardwareMidiInput`] is the entry [`HardwareMidiInputs`] keeps;
//! the [`InputProducerHandle`] is the only way events enter.
//!
//! # Who owns what
//!
//! The handle owns the device's end of its inlet
//! ([`DeviceInput`]), is not `Clone`, and
//! pushes through `&mut self`: the borrow checker keeps an inlet to one
//! pusher. The entry holds a view of the inlet's counts and the active flag
//! the handle reads, so the control side reads and sets them without touching
//! the thread that pushes. A driver thread still pushing after its port was
//! retired pushes into an inlet nothing reads.
//!
//! [`HardwareMidiInputs`]: super::HardwareMidiInputs

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_graph::Losses;
use tutti_midi_device::{ClockHeard, JrReceiver};
use tutti_midi_runtime::{DeviceInput, DeviceSlot, InletCounts};
use tutti_ump::{Message, Ump, Utility};

use crate::InputTiming;

/// An [`InputTiming`] both ends of a port read: the control side sets it as the
/// device's configuration settles, and the driver thread follows it.
#[derive(Debug)]
struct SharedTiming(AtomicU8);

impl SharedTiming {
    fn new() -> Self {
        Self(AtomicU8::new(InputTiming::Arrival.to_raw()))
    }

    fn get(&self) -> InputTiming {
        // Only `set` stores, and it stores `to_raw`: every byte is a timing.
        InputTiming::from_raw(self.0.load(Ordering::Acquire)).unwrap_or(InputTiming::Arrival)
    }

    fn set(&self, timing: InputTiming) {
        self.0.store(timing.to_raw(), Ordering::Release);
    }
}

/// What a port counts of its device's JR Clock messages, shared with the
/// port's entry: how many restarted the sender's clock, and how many were
/// ignored as late.
#[derive(Debug, Default)]
struct JrCounts {
    restarted: AtomicU64,
    late: AtomicU64,
}

/// What the handle holds of a device that sends JR Timestamps: the sender's
/// clock on the host's and the timestamp the next events are under.
#[derive(Debug)]
struct JrInput {
    following: InputTiming,
    clock: JrReceiver,
    counts: Arc<JrCounts>,
    /// The time the last event was placed at: events are rendered in the order
    /// they arrive (M2-104 §7.2.2.2), so none is placed before it, whichever
    /// timing times them.
    placed: Option<Instant>,
}

impl JrInput {
    fn new(counts: Arc<JrCounts>) -> Self {
        Self {
            following: InputTiming::Arrival,
            clock: JrReceiver::new(),
            counts,
            placed: None,
        }
    }

    /// Follows `timing`: a change starts the sender's clock and timestamp
    /// over, since what was heard before it was not meant for this. The time
    /// of the last event is kept, so the events stay in order across it.
    fn follow(&mut self, timing: InputTiming) {
        if timing != self.following {
            self.clock.reset();
            self.following = timing;
        }
    }

    /// Takes a JR message. Whatever else it is, the caller keeps it from the
    /// graph.
    fn take(&mut self, utility: Utility, arrival: Instant) {
        if self.following != InputTiming::JitterReduction {
            return;
        }
        match utility {
            Utility::JrClock(ticks) => match self.clock.clock(ticks, arrival) {
                ClockHeard::Restarted => {
                    self.counts.restarted.fetch_add(1, Ordering::Relaxed);
                }
                ClockHeard::Late => {
                    self.counts.late.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            },
            Utility::JrTimestamp(ticks) => self.clock.timestamp(ticks, arrival),
            _ => {}
        }
    }

    /// Where an event that arrived at `arrival` is placed: the time its JR
    /// Timestamp gives, rendered after the connection's jitter, or its arrival
    /// where it has no timestamp or the clock cannot map it; never before the
    /// event before it.
    fn place(&mut self, arrival: Instant) -> Instant {
        let at = match self.following {
            InputTiming::Arrival => arrival,
            InputTiming::JitterReduction => self.clock.resolve_held(arrival).render_at(arrival),
        };
        match self.placed {
            // Only a port that has been timed by JR keeps the order.
            Some(placed) => {
                let at = at.max(placed);
                self.placed = Some(at);
                at
            }
            None => {
                if self.following == InputTiming::JitterReduction {
                    self.placed = Some(at);
                }
                at
            }
        }
    }
}

/// The producer end of one port: the driver thread's. Owns the inlet's only
/// writer. It is `Send`, so it moves to the thread the OS delivers on, and
/// **not** `Clone`: [`push`](Self::push) takes `&mut self`, so an inlet has
/// exactly one pusher by construction. A driver callback that is an `FnMut`
/// (CoreMIDI's, ALSA's pump loop) owns its handle outright.
///
/// There is no second handle to a port. It cannot be cloned:
///
/// ```compile_fail,E0599
/// fn second(handle: tutti_midi_hardware::InputProducerHandle) {
///     let _ = handle.clone();
/// }
/// ```
///
/// and the list hands one out only from
/// [`open_input_port`](super::HardwareMidiInputs::open_input_port).
pub struct InputProducerHandle {
    input: DeviceInput,
    /// Whether the port delivers: shared with the port's entry.
    active: Arc<AtomicBool>,
    /// Where the device's UMP Stream messages go: not into the graph.
    stream: HeapProd<Ump>,
    /// How many of them the ring had no room for: shared with the port's entry.
    stream_lost: Arc<AtomicU64>,
    /// Whether the control side wants a copy of the SysEx7 packets the device
    /// sends (it is waiting for an Identity Reply): shared with the port's entry.
    hear_sysex: Arc<AtomicBool>,
    /// Where event times come from: shared with the port's entry.
    timing: Arc<SharedTiming>,
    jr: JrInput,
}

/// How many UMP Stream messages a port holds between two looks of the control
/// side. A discovery's replies are a few dozen packets (names are up to 98 and
/// 91 bytes, 14 and 13 to a packet, for up to 32 blocks); the control side
/// reads them at every update, and a reply lost to a full ring is asked for
/// again when its step times out.
pub(crate) const STREAM_CAPACITY: usize = 512;

/// The UMP message type of a 7-bit System Exclusive packet (M2-104 §7.7).
const SYSEX7: u8 = 0x3;

impl core::fmt::Debug for InputProducerHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InputProducerHandle")
            .field("slot", &self.input.slot())
            .finish_non_exhaustive()
    }
}

impl InputProducerHandle {
    /// Pushes one event the OS delivered at `timestamp`: it is placed on the
    /// frame the engine's clock puts that time on, plus the inlets' lead
    /// ([`DeviceInlets`](tutti_midi_runtime::DeviceInlets)), so the order and
    /// spacing of the device's events are kept to the clock map's accuracy.
    ///
    /// A **UMP Stream message** (type `0xF`, M2-104 §7.1) is the device's talk
    /// about itself, not music: it never reaches the graph. It goes to the
    /// port's stream ring, which the control side reads
    /// (`HardwareMidiInputs::drain_stream`),
    /// and one the ring has no room for is counted
    /// ([`PortInfo::stream_lost`](super::PortInfo::stream_lost)) and is
    /// `Diverted` all the same: it was not lost from the inlet.
    ///
    /// A **SysEx7 packet** (type `0x3`) is music's too, and goes to the graph
    /// whatever else happens to it; but while the control side is waiting for a
    /// device to say what it is (`HardwareMidiInputs::hear_sysex`,
    /// an Identity Request is out, or MIDI-CI runs), a copy goes to the same stream ring. It is a
    /// copy, not a diversion, because the packets of a message are not
    /// recognisable one by one: whether a SysEx is an Identity Reply is known
    /// only once it is whole, and a patch dump that arrives meanwhile must still
    /// reach the graph.
    ///
    /// A **JR Clock or JR Timestamp** (type `0x0`, M2-104 §7.2.2) never reaches
    /// the graph either, and is `Diverted`. While the port is timed by the
    /// device's JR Timestamps (its configuration says it sends them, and the
    /// input was opened with [`InputTiming::JitterReduction`](crate::InputTiming)), a JR Clock feeds the sender's clock
    /// ([`JrReceiver`]) and a JR Timestamp is the time of the events after it,
    /// until the next: an event is then placed at the time the clock maps its
    /// timestamp to, plus the connection's jitter, instead of at `timestamp`.
    /// An event the clock cannot map (none heard yet, or lost) is placed at
    /// `timestamp`. Otherwise the JR messages are ignored and every event is
    /// placed at `timestamp`.
    ///
    /// Returns what became of it: [`Pushed::Sent`] into the graph's inlet,
    /// [`Pushed::Diverted`] (not music, or the port is paused, so nothing was
    /// lost for want of room), or [`Pushed::Full`] when the device's inlet is
    /// full: the event is then dropped, not queued, and counted
    /// ([`HardwareMidiInputs::losses`](super::HardwareMidiInputs::losses)).
    /// An **inactive** port ([`set_port_active`](super::HardwareMidiInputs::set_port_active))
    /// discards the event and is `Diverted`. Lock-free and allocation-free, so
    /// it is safe to call from a driver callback.
    #[inline]
    pub fn push(&mut self, event: impl Into<Ump>, timestamp: Instant) -> Pushed {
        let event = event.into();
        if !self.active.load(Ordering::Acquire) {
            return Pushed::Diverted;
        }
        if event.is_stream() {
            if self.stream.try_push(event).is_err() {
                self.stream_lost.fetch_add(1, Ordering::Relaxed);
            }
            return Pushed::Diverted;
        }
        self.jr.follow(self.timing.get());
        if event.is_jr() {
            if let Ok(Message::Utility(utility)) = event.decode() {
                self.jr.take(utility, timestamp);
            }
            return Pushed::Diverted;
        }
        if event.message_type() == SYSEX7
            && self.hear_sysex.load(Ordering::Relaxed)
            && self.stream.try_push(event).is_err()
        {
            self.stream_lost.fetch_add(1, Ordering::Relaxed);
        }
        if self.input.push(event, self.jr.place(timestamp)) {
            Pushed::Sent
        } else {
            Pushed::Full
        }
    }

    /// The identity of the port this handle feeds, and the slot it holds
    /// until it drops: the key to pass to
    /// [`HardwareMidiInputs`](super::HardwareMidiInputs)' `set_port_active` or
    /// `remove_input_port`.
    #[must_use]
    pub fn slot(&self) -> DeviceSlot {
        self.input.slot()
    }
}

/// What [`InputProducerHandle::push`] did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[must_use = "`Full` is a lost event: a caller counts or logs it"]
pub enum Pushed {
    /// The event went into the device's inlet, for the graph.
    Sent,
    /// The event was kept out of the graph on purpose (a UMP Stream message,
    /// a JR message, or any event of a paused port): nothing was lost.
    Diverted,
    /// The device's inlet had no room: the event is dropped, and counted as
    /// lost.
    Full,
}

/// The UMP Stream messages of one port, drained as they are read
/// (`HardwareMidiInputs::drain_stream`):
/// one ring lock per step, none held across the caller's work.
#[derive(Debug)]
pub struct StreamDrain {
    port: Option<Arc<HardwareMidiInput>>,
}

impl StreamDrain {
    pub(super) fn new(port: Option<Arc<HardwareMidiInput>>) -> Self {
        Self { port }
    }
}

impl Iterator for StreamDrain {
    type Item = Ump;

    fn next(&mut self) -> Option<Ump> {
        self.port.as_ref()?.pop_stream()
    }
}

/// A hardware **input** port in the list: its name, whether it delivers, and
/// the counts of its inlet. There is no output counterpart here: outbound MIDI
/// rides an event tap to a [`MidiOut`](tutti_midi_runtime::MidiOut) sink.
pub(crate) struct HardwareMidiInput {
    name: String,
    active: Arc<AtomicBool>,
    counts: InletCounts,
    /// The reading end of the stream ring; control side, one reader at a time.
    stream: Mutex<HeapCons<Ump>>,
    stream_lost: Arc<AtomicU64>,
    hear_sysex: Arc<AtomicBool>,
    timing: Arc<SharedTiming>,
    jr_counts: Arc<JrCounts>,
}

impl HardwareMidiInput {
    /// A port named `name` over `input`, and the producer handle for it.
    pub(crate) fn new(name: impl Into<String>, input: DeviceInput) -> (Self, InputProducerHandle) {
        let active = Arc::new(AtomicBool::new(true));
        let stream_lost = Arc::new(AtomicU64::new(0));
        let hear_sysex = Arc::new(AtomicBool::new(false));
        let timing = Arc::new(SharedTiming::new());
        let jr_counts = Arc::new(JrCounts::default());
        let (stream_in, stream_out) = HeapRb::<Ump>::new(STREAM_CAPACITY).split();
        (
            Self {
                name: name.into(),
                active: Arc::clone(&active),
                counts: input.counts(),
                stream: Mutex::new(stream_out),
                stream_lost: Arc::clone(&stream_lost),
                hear_sysex: Arc::clone(&hear_sysex),
                timing: Arc::clone(&timing),
                jr_counts: Arc::clone(&jr_counts),
            },
            InputProducerHandle {
                input,
                active,
                stream: stream_in,
                stream_lost,
                hear_sysex,
                timing,
                jr: JrInput::new(jr_counts),
            },
        )
    }

    /// What the inlet has lost.
    pub(crate) fn losses(&self) -> Losses {
        self.counts.losses()
    }

    /// How many Stream messages the ring had no room for.
    pub(crate) fn stream_lost(&self) -> u64 {
        self.stream_lost.load(Ordering::Relaxed)
    }

    /// Whether the stream ring also takes a copy of the SysEx7 packets.
    pub(crate) fn set_hear_sysex(&self, hear: bool) {
        self.hear_sysex.store(hear, Ordering::Relaxed);
    }

    /// How many JR Clock messages restarted the sender's clock.
    pub(crate) fn jr_restarted(&self) -> u64 {
        self.jr_counts.restarted.load(Ordering::Relaxed)
    }

    /// How many JR Clock messages were ignored as late.
    pub(crate) fn jr_late(&self) -> u64 {
        self.jr_counts.late.load(Ordering::Relaxed)
    }

    /// Where the port's event times come from from now on.
    pub(crate) fn set_timing(&self, timing: InputTiming) {
        self.timing.set(timing);
    }

    /// The oldest Stream message held, if any.
    pub(crate) fn pop_stream(&self) -> Option<Ump> {
        self.stream
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_pop()
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }
}

impl core::fmt::Debug for HardwareMidiInput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HardwareMidiInput")
            .field("name", &self.name)
            .finish()
    }
}
