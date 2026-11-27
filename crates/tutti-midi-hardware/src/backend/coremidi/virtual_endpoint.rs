//! A virtual MIDI endpoint on CoreMIDI, in the UMP protocol the spec asks for:
//! one name that other apps send to and read from.
//!
//! A UMP endpoint is created with `MIDISourceCreateWithProtocol` and
//! `MIDIDestinationCreateWithProtocol` and carries UMP words, so a MIDI-2-only
//! message (per-note controllers, JR Timestamps) reaches the wire; the raw CoreMIDI
//! calls for it are `tutti-coremidi-ffi`'s (`UmpSource`, `UmpDestination`), and this
//! module names the endpoint and puts its errors in this crate's.
//!
//! Over a MIDI-1.0 transport every inbound message arrives as legacy bytes. Most of
//! MIDI 2.0 survives that (MIDI-CI is Universal SysEx by design, M2-101), but
//! **UMP Stream** (message type 0xF, M2-104 §7.1) has no MIDI-1.0 encoding at all:
//! it arrives only over a UMP endpoint, which is what this publishes. CoreMIDI
//! delivers a packet as a *packed word stream*: several concatenated UMP messages
//! with no separators. The destination splits it, so the callback gets one message
//! at a time with the packet's time.
//!
//! A spec that declares Function Blocks is created as a macOS 15
//! `MIDIUMPMutableEndpoint` (`UmpEndpoint`), one object with a source and a
//! destination, with its blocks registered and enabled so other apps see them. A
//! spec with none is a source and a destination under the spec's name, which is
//! what CoreMIDI offers for an endpoint with no blocks to declare.
//!
//! [`Opening`] is the creation, polled: a spec with no blocks is ready on the first
//! poll; one with blocks drives `CreateUmpEndpoint`, which retries a registration or
//! an enable that CoreMIDI refuses and says when the next attempt is due.
//! [`UmpVirtualEndpoint`] owns the endpoint, disposed on drop.

use std::sync::{Arc, Weak};
use std::task::Poll;

use tracing::debug;
use tutti_coremidi_ffi::{CreateUmpEndpoint, UmpDestination, UmpEndpoint, UmpSender, UmpSource};
use tutti_platform::{Instant, MainThreadMarker};
use tutti_ump::{FunctionBlockId, Protocol, Ump};

use crate::InputProducerHandle;
use crate::error::{MidiHardwareError, Result};
use crate::start::Started;
use crate::virtual_endpoint::{
    Delivery, EndpointBackend, Live, OpeningBackend, SenderBackend, VirtualProtocol, VirtualSpec,
};

/// CoreMIDI's protocol for the spec's.
fn protocol_of(protocol: VirtualProtocol) -> Protocol {
    match protocol {
        VirtualProtocol::Midi1 => Protocol::Midi1,
        VirtualProtocol::Midi2 => Protocol::Midi2,
    }
}

/// A CoreMIDI virtual endpoint that speaks the UMP protocol it was created with.
///
/// It carries UMP words end to end, so JR Timestamps and other MIDI-2-only
/// messages survive to the wire in both directions. Each received message is
/// passed to the callback supplied at construction, on CoreMIDI's own delivery
/// thread. Owns its endpoint, which is disposed on drop. With Function Blocks the
/// endpoint is a main-thread object, so this type is `!Send`; what sends from
/// another thread is the [`SenderBackend`] it hands out.
#[must_use = "dropping it removes the virtual endpoint"]
pub(crate) struct UmpVirtualEndpoint {
    endpoint: Endpoint,
    name: String,
}

/// The CoreMIDI objects an endpoint is.
enum Endpoint {
    /// A source and a destination of one name: no Function Blocks.
    Pair {
        /// Held by senders only weakly, and only while they hold `live`.
        source: Arc<UmpSource>,
        destination: UmpDestination,
        live: Live,
    },
    /// A UMP Endpoint that declares blocks.
    WithBlocks(UmpEndpoint),
}

impl core::fmt::Debug for UmpVirtualEndpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Owns opaque CoreMIDI handles; report the endpoint name only.
        f.debug_struct("UmpVirtualEndpoint")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Drop for UmpVirtualEndpoint {
    fn drop(&mut self) {
        if let Endpoint::Pair { live, .. } = &self.endpoint {
            live.end();
        }
    }
}

/// A CoreMIDI endpoint being made. A pair of source and destination is made when it
/// is created; one with Function Blocks is a [`CreateUmpEndpoint`] the polls drive.
pub(crate) struct Opening {
    made: Making,
    name: String,
    protocol: VirtualProtocol,
    created_at: Instant,
}

enum Making {
    /// The endpoint, ready on the first poll.
    Pair(Option<UmpVirtualEndpoint>),
    /// The endpoint with blocks, being registered and enabled.
    Blocks(CreateUmpEndpoint),
    /// Taken by the poll that finished it.
    Done,
}

impl UmpVirtualEndpoint {
    /// Starts publishing the endpoint `spec` describes, pushing every received
    /// message into `producer` exactly as a hardware input port does: the events land
    /// in the same [`HardwareMidiInputs`](crate::HardwareMidiInputs) rings a physical
    /// endpoint feeds. A spec that declares Function Blocks publishes them on a UMP
    /// Endpoint. `started` is the proof the hub's client is made, and `main` the proof
    /// the endpoint is made on the main thread.
    ///
    /// # Errors
    ///
    ///
    /// [`MidiHardwareError::VirtualBlocksUnsupported`](crate::MidiHardwareError::VirtualBlocksUnsupported)
    /// when `spec` declares blocks and the OS is before macOS 15, and
    /// [`MidiHardwareError::Os`](crate::MidiHardwareError::Os) if CoreMIDI refuses
    /// the client or the endpoint, or cannot carry a block (a MIDI-CI version, a
    /// block number that is not its position).
    pub(crate) fn open(
        spec: &VirtualSpec,
        mut producer: InputProducerHandle,
        started: Started,
        main: MainThreadMarker,
    ) -> Result<Opening> {
        Self::create(spec, started, main, move |event, arrived| {
            // A full inlet counts the drop itself; a log line would run on
            // CoreMIDI's thread.
            let _ = producer.push(event, arrived);
        })
    }

    /// Starts creating the endpoint `spec` describes, invoking `on_event` for every
    /// UMP message received with the `Instant` its packet reached CoreMIDI (the
    /// packet's host time). `on_event` runs on CoreMIDI's delivery thread, so it
    /// must not block.
    pub(super) fn create<F>(
        spec: &VirtualSpec,
        started: Started,
        main: MainThreadMarker,
        mut on_event: F,
    ) -> Result<Opening>
    where
        F: FnMut(Ump, tutti_platform::Instant) + Send + 'static,
    {
        let name = spec.name().as_str();
        let protocol = protocol_of(spec.protocol);
        // The device watch's client is made before either endpoint is (`started` is its
        // proof), a UMP Endpoint included: CoreMIDI sends notifications to the run loop of
        // the process's first client, and an endpoint's creation is what would
        // otherwise make that client.
        let client = super::hub_client(started);
        let on_ump = move |time, event| {
            on_event(
                event,
                super::host_time::Pairing::now().instant_of_stamp(time),
            );
        };
        let now = Instant::now();
        let made = if spec.function_blocks().is_empty() {
            Making::Pair(Some(Self {
                endpoint: Endpoint::Pair {
                    source: Arc::new(UmpSource::new(client, name, protocol)?),
                    destination: UmpDestination::new(client, name, protocol, on_ump)?,
                    live: Live::new(),
                },
                name: name.to_string(),
            }))
        } else {
            Making::Blocks(
                CreateUmpEndpoint::new(
                    main,
                    client,
                    name,
                    protocol,
                    spec.device(),
                    spec.instance(),
                    &super::block_infos(spec),
                    on_ump,
                    now,
                )
                .map_err(super::open_error)?,
            )
        };
        Ok(Opening {
            made,
            name: name.to_string(),
            protocol: spec.protocol,
            created_at: now,
        })
    }
}

impl Opening {
    /// One poll at `now`: the endpoint once it is made.
    pub(super) fn poll_endpoint(
        &mut self,
        main: MainThreadMarker,
        now: Instant,
    ) -> Poll<Result<UmpVirtualEndpoint>> {
        let endpoint = match &mut self.made {
            Making::Pair(pair) => pair.take(),
            Making::Blocks(create) => match create.poll(main, now) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => {
                    self.made = Making::Done;
                    return Poll::Ready(Err(super::open_error(error)));
                }
                Poll::Ready(Ok(endpoint)) => Some(UmpVirtualEndpoint {
                    endpoint: Endpoint::WithBlocks(endpoint),
                    name: self.name.clone(),
                }),
            },
            Making::Done => None,
        };
        self.made = Making::Done;
        Poll::Ready(endpoint.ok_or(MidiHardwareError::EndpointGone).inspect(|_| {
            debug!(name = %self.name, protocol = ?self.protocol, "Created virtual MIDI endpoint");
        }))
    }
}

impl OpeningBackend for Opening {
    fn poll(
        &mut self,
        main: MainThreadMarker,
        now: Instant,
    ) -> Poll<Result<(Box<dyn EndpointBackend>, VirtualProtocol)>> {
        self.poll_endpoint(main, now).map(|made| {
            made.map(|endpoint| {
                (
                    Box::new(endpoint) as Box<dyn EndpointBackend>,
                    self.protocol,
                )
            })
        })
    }

    fn deadline(&self) -> Instant {
        match &self.made {
            Making::Blocks(create) => create.deadline(),
            Making::Pair(_) | Making::Done => self.created_at,
        }
    }
}

/// A send on a pair's source, which it does not keep alive.
struct PairSender {
    source: Weak<UmpSource>,
    live: Live,
}

impl SenderBackend for PairSender {
    /// # Errors
    ///
    /// [`MidiHardwareError::EndpointGone`] once the endpoint is dropped, and
    /// [`MidiHardwareError::Os`] when `MIDIReceivedEventList` returns a non-zero
    /// `OSStatus`.
    fn send(&self, ump: &Ump) -> Result<Delivery> {
        let _live = self.live.hold()?;
        let source = self.source.upgrade();
        source.ok_or(MidiHardwareError::EndpointGone)?.send(ump)?;
        Ok(Delivery::Sent)
    }
}

/// A send on a UMP Endpoint with Function Blocks.
struct EndpointSender(UmpSender);

impl SenderBackend for EndpointSender {
    /// # Errors
    ///
    /// [`MidiHardwareError::EndpointGone`] once the endpoint is dropped, and
    /// [`MidiHardwareError::Os`] when `MIDIReceivedEventList` returns a non-zero
    /// `OSStatus`.
    fn send(&self, ump: &Ump) -> Result<Delivery> {
        self.0.send(ump).map_err(|error| match error {
            tutti_coremidi_ffi::Error::EndpointGone => MidiHardwareError::EndpointGone,
            other => MidiHardwareError::Os(other),
        })?;
        Ok(Delivery::Sent)
    }
}

/// The destination catches a panic in its receive callback and counts it
/// ([`UmpDestination::faults`]); after one the callback delivers nothing, so the
/// endpoint reads as gone.
impl EndpointBackend for UmpVirtualEndpoint {
    fn sender(&self) -> Arc<dyn SenderBackend> {
        match &self.endpoint {
            Endpoint::Pair { source, live, .. } => Arc::new(PairSender {
                source: Arc::downgrade(source),
                live: live.clone(),
            }),
            Endpoint::WithBlocks(endpoint) => Arc::new(EndpointSender(endpoint.sender())),
        }
    }

    fn is_alive(&self) -> bool {
        let faults = match &self.endpoint {
            Endpoint::Pair { destination, .. } => destination.faults(),
            Endpoint::WithBlocks(endpoint) => endpoint.faults(),
        };
        faults == 0
    }

    fn block_numbers(&self) -> Option<&[FunctionBlockId]> {
        match &self.endpoint {
            Endpoint::Pair { .. } => None,
            Endpoint::WithBlocks(endpoint) => Some(endpoint.blocks()),
        }
    }
}

#[cfg(test)]
mod tests;
