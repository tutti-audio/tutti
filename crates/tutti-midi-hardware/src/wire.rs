//! The wire form of a device's output: what a packet becomes before it is
//! queued for the device ([`Wire`]).
//!
//! The graph's MIDI is one thing (MIDI 2.0 Channel Voice, type `0x4`, after the
//! input edge's `Midi2Translator`), and a device is in a Protocol of its own. The
//! rule is the spec's, applied at one place, [`OpenOutput::queue`](crate::OpenOutput):
//!
//! - **A MIDI 2.0 Protocol endpoint** gets the packets as they are.
//!   (M2-104 §3.3.2: MIDI 2.0 Protocol Devices use no message of
//!   Message Type 0x2, and the graph carries none; a MIDI 1.0 Channel Voice
//!   packet that does arrive is not changed here.) §7.1.6.2 allows MIDI 1.0
//!   Protocol optionally for a Function Block that declares a MIDI 1.0
//!   connection; this does not take the option, because §6.2.2 puts the
//!   translation for such a block on the device, and
//!   a per-Group narrowing would send type `0x2` into an endpoint whose Protocol
//!   is MIDI 2.0, which §3.3.2 forbids for the rest of it.
//! - **A MIDI 1.0 Protocol endpoint**, and a MIDI 1.0 port, gets MIDI 1.0 (§3.2:
//!   UMP MIDI 1.0 Devices use no message of Message Type 0x4, MIDI 2.0 Channel
//!   Voice Messages): each type `0x4` packet through one
//!   [`Midi1Translator`] for the output (M2-104 Appendix D.2, per Group and Channel); type
//!   `0x2`, System and SysEx7 and every other type pass as they are. A
//!   packet the MIDI 1.0 translator has no MIDI 1.0 form for (a per-note controller,
//!   D.2.8) is dropped.
//! - **JR Timestamps** are sent only when the device's configuration says it
//!   receives them (§7.1.6.2, RXJR: the Sender sends none until a Stream
//!   Configuration Notification message has been received): a JR Clock or JR Timestamp is dropped when `rx_jr` is off.
//!   Delta Clockstamp and Ticks Per Quarter Note are a clip's, not a wire's, and
//!   are dropped always.
//!
//! The configuration is the device's [`StreamConfig`]: the OS's description
//! when the device is opened, and what the UMP Stream negotiation settled
//! (`MidiSession::poll`) after it.

use tutti_midi_protocol::Midi1Translator;
use tutti_midi_runtime::{Outgoing, Taken};
use tutti_ump::{Join, Message, Protocol, StreamConfig, Ump, Utility, parse_exact};

/// One device output's wire form: its configuration, the [`Midi1Translator`] that
/// serves it for as long as it is in MIDI 1.0, and the buffers one batch is
/// converted in.
#[derive(Debug)]
pub(crate) struct Wire {
    config: StreamConfig,
    midi1_translator: Midi1Translator,
    /// The batch as it goes to the device.
    out: Vec<Outgoing>,
    /// For each input event, how many of `out` it had produced once it was
    /// done: what turns the device's count back into the caller's.
    ends: Vec<usize>,
}

impl Wire {
    pub(crate) fn new(config: StreamConfig) -> Self {
        Self {
            config,
            midi1_translator: Midi1Translator::new(),
            out: Vec::new(),
            ends: Vec::new(),
        }
    }

    pub(crate) const fn config(&self) -> StreamConfig {
        self.config
    }

    /// The device now has `config`. A change of Protocol resets the MIDI 1.0 translator.
    pub(crate) fn set_config(&mut self, config: StreamConfig) {
        if config.protocol != self.config.protocol {
            self.midi1_translator.reset();
        }
        self.config = config;
    }

    /// Whether the device is in a Protocol that has no MIDI 2.0 Channel Voice:
    /// MIDI 1.0, and the reserved values, which nobody can speak.
    fn speaks_midi1(&self) -> bool {
        self.config.protocol != Protocol::Midi2
    }

    /// Converts `events` into the batch for the device, which
    /// [`batch`](Self::batch) reads.
    pub(crate) fn convert(&mut self, events: &[Outgoing]) {
        self.out.clear();
        self.ends.clear();
        let speaks_midi1 = self.speaks_midi1();
        let jr = self.config.rx_jr;
        // Whether the event before made a packet: a packet joined to the one
        // before it joins nothing when that event made none.
        let mut follows_packet = false;
        for event in events {
            let (at, send, ump) = (event.at, event.send, event.ump);
            let first = if follows_packet {
                event.join()
            } else {
                Join::Alone
            };
            let made = self.out.len();
            let packet = |ump: Ump, join: Join| {
                let packet = Outgoing::new(at, send, ump);
                match join {
                    Join::Alone => packet,
                    Join::ToPrevious => packet.joined_to_previous(),
                }
            };
            match ump.message_type() {
                0 => {
                    if let Some(kept) = utility(ump, jr) {
                        self.out.push(packet(kept, first));
                    }
                }
                4 if speaks_midi1 => {
                    if let Ok(message) = ump.decode() {
                        let out = &mut self.out;
                        let mut next = first;
                        translate_voice(&mut self.midi1_translator, message, |translated| {
                            out.push(packet(translated, next));
                            // The packets of one event are one message: a cut
                            // between a controller's select and its value
                            // leaves the receiver selected with no value.
                            next = Join::ToPrevious;
                        });
                    }
                }
                _ => self.out.push(packet(ump, first)),
            }
            follows_packet = self.out.len() > made;
            self.ends.push(self.out.len());
        }
    }

    /// The batch [`convert`](Self::convert) made.
    pub(crate) fn batch(&self) -> &[Outgoing] {
        &self.out
    }

    /// How much of `events`, the batch [`convert`](Self::convert) was given,
    /// the device has taken, when it took `taken` of the batch it was sent: the
    /// events whose packets all went, so an event that became nothing counts as
    /// taken. A message of several packets is one event per packet and goes
    /// through the conversion as it is, so the count still ends between
    /// messages.
    pub(crate) fn accepted(&self, taken: Taken, events: &[Outgoing]) -> Taken {
        let events_taken = self.ends.partition_point(|&end| end <= taken.packets());
        Taken::within(events, events_taken)
    }

    /// How many of the first `taken` events of the batch became no packet at
    /// all: they have no form on the wire this device is in. Counted once, at
    /// the offer that takes them.
    pub(crate) fn unrepresentable_in(&self, taken: usize) -> u64 {
        let mut before = 0;
        let mut none = 0;
        for &end in &self.ends[..taken] {
            none += u64::from(end == before);
            before = end;
        }
        none
    }
}

/// Runs a MIDI 2.0 Channel Voice `message` through `midi1_translator` and calls `out`
/// with each MIDI 1.0 packet it becomes, in the order to send them; returns how
/// many that was. Any other message, and one with no MIDI 1.0 form (a per-note
/// controller, D.2.8), is none.
pub(crate) fn translate_voice(
    midi1_translator: &mut Midi1Translator,
    message: Message,
    mut out: impl FnMut(Ump),
) -> usize {
    let Message::Midi2(address, _) = message else {
        return 0;
    };
    let mut count = 0;
    midi1_translator.feed(message, |bytes| {
        if let Some(packet) = parse_exact(address.group, bytes.as_slice()) {
            out(Ump::from(packet));
            count += 1;
        }
    });
    count
}

/// A Utility packet as the wire carries it: the JR messages when the device
/// takes them, a NOOP, and nothing else.
fn utility(ump: Ump, jr: bool) -> Option<Ump> {
    match ump.decode() {
        Ok(Message::Utility(Utility::JrClock(_) | Utility::JrTimestamp(_))) => jr.then_some(ump),
        Ok(Message::Utility(Utility::Noop)) | Err(_) => Some(ump),
        Ok(_) => None,
    }
}

#[cfg(test)]
mod tests;
