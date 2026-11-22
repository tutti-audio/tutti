//! The discovering side of a UMP link (M2-100 Table 4, steps 2 to 4; M2-104
//! §7.1.6.1): Endpoint Discovery and the Stream negotiation that completes it.
//!
//! An [`EndpointDiscovery`] is a state machine over received messages and time. It
//! reads no clock and sends nothing itself: every step returns a [`DiscoveryAction`]
//! (a packet to send, or the result), and time comes in as an [`Instant`]
//! value from whoever drives it.
//!
//! 1. **Endpoint Discovery** (Table 4 step 2): ask for everything, fold Endpoint
//!    Info, Device Identity, Endpoint Name, Product Instance Id and the Stream
//!    Configuration Notification into an [`Endpoint`].
//! 2. **Select Protocol** (step 3): if the Notification does not match what is
//!    wanted, narrowed to what the Endpoint supports, send a Stream
//!    Configuration Request and wait for the Notification, which is then the
//!    truth (§7.1.6.1 step 3: the peer may refuse any part).
//! 3. **Function Blocks** (step 4): ask for every block's Info and Name.
//!
//! A step that gets no answer within [`STEP_TIMEOUT`] is sent again, up to
//! [`MAX_ATTEMPTS`] times in all. Then Discovery and Select Protocol give up
//! and the peer is a MIDI 1.0 device ([`DiscoveryEnding::Midi1Fallback`]); Function
//! Blocks are complete with the blocks that arrived. Once every block's Info is in,
//! the Names have [`NAME_GRACE`] to follow, since a block with no name sends
//! none.

use std::time::Duration;

use tutti_platform::Instant;

use bitflags::bitflags;
use tutti_ump::{
    BlockFilter, BlockSelect, DiscoveryFilter, Field, FunctionBlockSet, Message, Protocol, Stream,
    StreamConfig, StreamTextAssembler, Ump,
};

use crate::deadline::Retry;
use crate::endpoint::{Endpoint, MAX_FUNCTION_BLOCKS, UMP_VERSION};
use tutti_ump::{
    BLOCK_NAME_BYTES, BlockName, ENDPOINT_NAME_BYTES, EndpointName, INSTANCE_ID_BYTES,
    ProductInstanceId,
};

/// How long a step waits for its answer before it is sent again.
///
/// M2-104 and M2-100 give no timeout for the UMP Stream steps (the 3 s of
/// M2-101 §5.5.5 is MIDI-CI Discovery's). One second is ours: UMP Stream
/// replies come from the peer's firmware or driver, not a user.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(1);

/// How long Function Block discovery waits for Names once every Info is in. A
/// block with no name never sends one (§7.1.9 makes the name optional), so a Name is not
/// something to wait for without end; when every block's Info and Name are in
/// it ends at once. 100 ms is ours.
pub const NAME_GRACE: Duration = Duration::from_millis(100);

/// How many times a step is sent in all, the first included. Ours: three
/// attempts, so a silent peer ends as MIDI 1.0 after
/// `3 x` [`STEP_TIMEOUT`].
pub const MAX_ATTEMPTS: u8 = 3;

/// How an Endpoint Discovery ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiscoveryEnding {
    /// The steps ran: the Endpoint's active configuration is what its last
    /// Stream Configuration Notification reported, whether or not that is what
    /// was wanted. The Function Blocks are those that were reported.
    Negotiated,
    /// A step went unanswered: the peer is treated as a MIDI 1.0 device,
    /// whatever it had said (MIDI 1.0 Protocol, no JR Timestamps, and one
    /// inferred Function Block if none was reported).
    Midi1Fallback,
}

/// What the driver does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryAction {
    /// Nothing; wait for a message, or for the [`deadline`](EndpointDiscovery::deadline).
    Wait,
    /// Send this packet to the peer.
    Send(Ump),
    /// The discovery is over; read [`EndpointDiscovery::endpoint`].
    Ended(DiscoveryEnding),
    /// After a [`Negotiated`](DiscoveryEnding::Negotiated) ending, the peer told
    /// something that changes what [`EndpointDiscovery::endpoint`] holds: a Stream
    /// Configuration Notification that moves the Protocol or JR Timestamps in
    /// use, or (when the Endpoint did not declare its Function Blocks static)
    /// a Function Block Info or Name that differs from what is held.
    Changed,
}

bitflags! {
    /// What Discovery has heard that it cannot go on without.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Heard: u8 {
        const INFO = 1;
        const CONFIG = 1 << 1;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Discover(Heard),
    Request(StreamConfig),
    /// The blocks whose Info has arrived, and whose Name has. `infos`
    /// is when the last Info came, which starts the [`NAME_GRACE`].
    Blocks {
        info: FunctionBlockSet,
        name: FunctionBlockSet,
        infos: Option<Instant>,
    },
    Done(DiscoveryEnding),
}

/// The discovering side of one UMP Endpoint.
///
/// Named for Endpoint Discovery (M2-104 §7.1.1), the process whose end result is a
/// described [`Endpoint`]: the Stream Configuration exchange of step 3 is part of
/// that discovery, not a process of its own that leaves anything else behind.
#[derive(Clone, Debug)]
pub struct EndpointDiscovery {
    wanted: StreamConfig,
    step: Step,
    retry: Retry,
    endpoint: Endpoint,
    endpoint_name: StreamTextAssembler<ENDPOINT_NAME_BYTES>,
    instance_id: StreamTextAssembler<INSTANCE_ID_BYTES>,
    block_name: StreamTextAssembler<BLOCK_NAME_BYTES>,
    block_names: [Option<BlockName>; MAX_FUNCTION_BLOCKS],
}

impl EndpointDiscovery {
    /// A discovery that asks for the MIDI 2.0 Protocol with JR Timestamps both
    /// ways, where the peer supports them; with the Endpoint Discovery to send.
    #[must_use]
    pub fn new(now: Instant) -> (Self, DiscoveryAction) {
        Self::with_wanted(
            StreamConfig {
                protocol: Protocol::Midi2,
                rx_jr: true,
                tx_jr: true,
            },
            now,
        )
    }

    /// A discovery that asks for `wanted`, as the Stream Configuration Request
    /// would carry it: `rx_jr` is the peer receiving JR Timestamps, `tx_jr`
    /// the peer sending them. Each part the peer does not support is dropped
    /// from the request, and a reserved Protocol asks for MIDI 2.0.
    #[must_use]
    pub fn with_wanted(wanted: StreamConfig, now: Instant) -> (Self, DiscoveryAction) {
        let connection = Self {
            wanted,
            step: Step::Discover(Heard::empty()),
            retry: Retry::first(now),
            endpoint: Endpoint::blank(),
            endpoint_name: StreamTextAssembler::new(),
            instance_id: StreamTextAssembler::new(),
            block_name: StreamTextAssembler::new(),
            block_names: [None; MAX_FUNCTION_BLOCKS],
        };
        let first = DiscoveryAction::Send(connection.packet_for_step());
        (connection, first)
    }

    /// What the connection learned so far. Final once [`ending`](Self::ending).
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// How it ended, or `None` while a step is waiting.
    #[must_use]
    pub const fn ending(&self) -> Option<DiscoveryEnding> {
        match self.step {
            Step::Done(ending) => Some(ending),
            _ => None,
        }
    }

    /// When [`on_timeout`](Self::on_timeout) next has something to do, or
    /// `None` once ended.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        match self.step {
            Step::Done(_) => None,
            Step::Blocks {
                infos: Some(at), ..
            } => Some(at + NAME_GRACE),
            _ => Some(self.retry.deadline(STEP_TIMEOUT)),
        }
    }

    /// Takes `active` as the Protocol and JR Timestamps in use, for a
    /// configuration changed by the peer's own Stream Configuration Request
    /// that the owner granted: the peer's Notification that follows reads as
    /// no change.
    pub fn adopt(&mut self, active: StreamConfig) {
        self.endpoint.active = active;
    }

    /// Folds one message from the peer. One that is not a UMP Stream message,
    /// and one the step does not wait for, are ignored.
    ///
    /// After a [`Negotiated`](DiscoveryEnding::Negotiated) ending the peer may still
    /// change what it said, and the connection follows it, answering
    /// [`DiscoveryAction::Changed`]: a Stream Configuration Notification is the
    /// Protocol and JR Timestamps in use whenever it comes (§7.1.6.1 step 3),
    /// but a Function Block Info or Name is read only when the Endpoint did
    /// not declare its Function Blocks static (§6.1.1: a static Endpoint's
    /// blocks never change after discovery, and §7.1.8 and §7.1.9 send
    /// change Notifications only from an Endpoint that has not declared that). A Function Block outside the count Endpoint Info declared is
    /// never read. After a [`Midi1Fallback`](DiscoveryEnding::Midi1Fallback) ending
    /// everything is ignored: the peer is a MIDI 1.0 device.
    pub fn on_message(&mut self, message: Message, now: Instant) -> DiscoveryAction {
        let Message::Stream(stream) = message else {
            return DiscoveryAction::Wait;
        };
        match (self.step, stream) {
            (Step::Done(DiscoveryEnding::Negotiated), _) => self.follow(stream),
            (Step::Done(DiscoveryEnding::Midi1Fallback), _) => DiscoveryAction::Wait,
            (Step::Discover(heard), _) => self.discover(heard, stream, now),
            (Step::Request(_), Stream::StreamConfigurationNotification(config)) => {
                self.endpoint.active = settle_protocol(config);
                self.start_blocks(now)
            }
            (Step::Blocks { info, name, infos }, _) => self.blocks(info, name, infos, stream, now),
            (Step::Request(_), _) => DiscoveryAction::Wait,
        }
    }

    /// Handles time having passed: when the current step has waited
    /// [`STEP_TIMEOUT`] since it was sent, sends it again, or, after
    /// [`MAX_ATTEMPTS`], gives up as the step says (see [`DiscoveryEnding`]). Calling it
    /// early is harmless.
    pub fn on_timeout(&mut self, now: Instant) -> DiscoveryAction {
        if let Step::Blocks {
            infos: Some(at), ..
        } = self.step
        {
            return if now.saturating_duration_since(at) >= NAME_GRACE {
                self.end(DiscoveryEnding::Negotiated)
            } else {
                DiscoveryAction::Wait
            };
        }
        if matches!(self.step, Step::Done(_)) || !self.retry.due(now, STEP_TIMEOUT) {
            return DiscoveryAction::Wait;
        }
        if !self.retry.spent(MAX_ATTEMPTS) {
            self.retry.resend(now);
            return DiscoveryAction::Send(self.packet_for_step());
        }
        match self.step {
            Step::Blocks { .. } => self.end(DiscoveryEnding::Negotiated),
            _ => {
                self.endpoint.fall_back_to_midi1();
                self.end(DiscoveryEnding::Midi1Fallback)
            }
        }
    }

    fn discover(&mut self, mut heard: Heard, stream: Stream, now: Instant) -> DiscoveryAction {
        match stream {
            Stream::EndpointInfo(info) => {
                // A count above 32 is not an Endpoint (§7.1.7: the numbers are
                // 0 to 31): the Info is refused, so it is never heard and the
                // step ends as a silent peer's does.
                if self.endpoint.apply_info(&info) {
                    heard |= Heard::INFO;
                }
            }
            Stream::DeviceIdentity(identity) => self.endpoint.identity = Some(identity),
            Stream::EndpointName(form, bytes) => {
                if let Some(text) = self.endpoint_name.feed(0, form, &bytes) {
                    self.endpoint.name = EndpointName::new(text.as_str());
                }
            }
            Stream::ProductInstanceId(form, bytes) => {
                if let Some(text) = self.instance_id.feed(0, form, &bytes) {
                    self.endpoint.instance_id = ProductInstanceId::new(text.as_str());
                }
            }
            Stream::StreamConfigurationNotification(config) => {
                self.endpoint.active = settle_protocol(config);
                heard |= Heard::CONFIG;
            }
            _ => {}
        }
        self.step = Step::Discover(heard);
        if heard != Heard::all() {
            return DiscoveryAction::Wait;
        }
        let wanted = narrow(self.wanted, &self.endpoint);
        if wanted == self.endpoint.active {
            return self.start_blocks(now);
        }
        self.step = Step::Request(wanted);
        self.send_step(now)
    }

    fn start_blocks(&mut self, now: Instant) -> DiscoveryAction {
        if self.endpoint.declared_blocks.get() == 0 {
            return self.end(DiscoveryEnding::Negotiated);
        }
        self.step = Step::Blocks {
            info: FunctionBlockSet::EMPTY,
            name: FunctionBlockSet::EMPTY,
            infos: None,
        };
        self.send_step(now)
    }

    fn blocks(
        &mut self,
        mut info: FunctionBlockSet,
        mut name: FunctionBlockSet,
        mut infos: Option<Instant>,
        stream: Stream,
        now: Instant,
    ) -> DiscoveryAction {
        match stream {
            Stream::FunctionBlockInfo(wire) => {
                if let Some(block) = self.endpoint.read_block(&wire) {
                    info.insert(block.id());
                    self.endpoint.insert_block(block);
                }
            }
            Stream::FunctionBlockName {
                block: Field::Value(block),
                form,
                bytes,
            } => {
                // One name is open at a time: another block's Start drops it.
                if let Some(text) = self.block_name.feed(block.get(), form, &bytes)
                    && let (Some(name_text), Some(slot)) = (
                        BlockName::new(text.as_str()),
                        self.block_names.get_mut(usize::from(block.get())),
                    )
                {
                    *slot = Some(name_text);
                    name.insert(block);
                }
            }
            Stream::StreamConfigurationNotification(config) => {
                self.endpoint.active = settle_protocol(config);
            }
            _ => {}
        }
        let declared = self.endpoint.declared_blocks;
        if info.covers(declared) && infos.is_none() {
            infos = Some(now);
        }
        self.step = Step::Blocks { info, name, infos };
        if info.covers(declared) && name.covers(declared) {
            self.end(DiscoveryEnding::Negotiated)
        } else {
            DiscoveryAction::Wait
        }
    }

    /// A message after a negotiated ending (see [`on_message`](Self::on_message)).
    fn follow(&mut self, stream: Stream) -> DiscoveryAction {
        match stream {
            Stream::StreamConfigurationNotification(config) => {
                let active = settle_protocol(config);
                if active == self.endpoint.active {
                    return DiscoveryAction::Wait;
                }
                self.endpoint.active = active;
                DiscoveryAction::Changed
            }
            _ if self.endpoint.static_function_blocks => DiscoveryAction::Wait,
            Stream::FunctionBlockInfo(wire) => {
                let Some(mut block) = self.endpoint.read_block(&wire) else {
                    return DiscoveryAction::Wait;
                };
                // The Info carries no name: the one held stays.
                block.name = self
                    .endpoint
                    .function_block(block.id())
                    .and_then(|b| b.name);
                if self.endpoint.function_block(block.id()) == Some(&block) {
                    return DiscoveryAction::Wait;
                }
                self.endpoint.insert_block(block);
                DiscoveryAction::Changed
            }
            Stream::FunctionBlockName {
                block: Field::Value(block),
                form,
                bytes,
            } => {
                let Some(text) = self.block_name.feed(block.get(), form, &bytes) else {
                    return DiscoveryAction::Wait;
                };
                let (Some(name), Some(Some(held))) = (
                    BlockName::new(text.as_str()),
                    self.endpoint.blocks.get_mut(usize::from(block.get())),
                ) else {
                    return DiscoveryAction::Wait;
                };
                if held.name == Some(name) {
                    return DiscoveryAction::Wait;
                }
                held.name = Some(name);
                DiscoveryAction::Changed
            }
            _ => DiscoveryAction::Wait,
        }
    }

    fn end(&mut self, ending: DiscoveryEnding) -> DiscoveryAction {
        for (i, name) in self.block_names.iter().enumerate() {
            if let (Some(name), Some(Some(block))) = (name, self.endpoint.blocks.get_mut(i)) {
                block.name = Some(*name);
            }
        }
        self.step = Step::Done(ending);
        DiscoveryAction::Ended(ending)
    }

    fn send_step(&mut self, now: Instant) -> DiscoveryAction {
        self.retry = Retry::first(now);
        DiscoveryAction::Send(self.packet_for_step())
    }

    fn packet_for_step(&self) -> Ump {
        let stream = match self.step {
            Step::Request(wanted) => Stream::StreamConfigurationRequest(wanted),
            Step::Blocks { .. } => Stream::FunctionBlockDiscovery {
                block: BlockSelect::All,
                filter: BlockFilter {
                    info: true,
                    name: true,
                },
            },
            Step::Discover(_) | Step::Done(_) => Stream::EndpointDiscovery {
                version: UMP_VERSION,
                filter: DiscoveryFilter {
                    endpoint_info: true,
                    device_identity: true,
                    endpoint_name: true,
                    product_instance_id: true,
                    stream_configuration: true,
                },
            },
        };
        Ump::from(Message::Stream(stream))
    }
}

/// A Notification's configuration as the connection keeps it: a reserved
/// Protocol is one it cannot use, so it is MIDI 1.0 (ours; the spec
/// only calls it reserved).
const fn settle_protocol(config: StreamConfig) -> StreamConfig {
    match config.protocol {
        Protocol::Reserved(_) => StreamConfig {
            protocol: Protocol::Midi1,
            ..config
        },
        _ => config,
    }
}

/// `wanted` reduced to what `endpoint` supports (§7.1.2): MIDI 2.0 only when
/// it supports it, JR only in the directions it supports.
const fn narrow(wanted: StreamConfig, endpoint: &Endpoint) -> StreamConfig {
    let caps = endpoint.capabilities;
    let protocol = match (wanted.protocol, caps.midi1, caps.midi2) {
        (Protocol::Midi1, true, _) | (_, true, false) => Protocol::Midi1,
        (Protocol::Midi1, false, true) | (_, _, true) => Protocol::Midi2,
        // It supports no Protocol at all: MIDI 1.0 is the one it is in.
        (_, false, false) => Protocol::Midi1,
    };
    StreamConfig {
        protocol,
        rx_jr: wanted.rx_jr && caps.rx_jr,
        tx_jr: wanted.tx_jr && caps.tx_jr,
    }
}

#[cfg(test)]
mod tests;
