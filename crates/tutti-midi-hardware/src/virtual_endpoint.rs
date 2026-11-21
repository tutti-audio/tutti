//! The endpoint this process publishes for other applications to connect to.
//!
//! A UMP Endpoint is bidirectional (M2-104 §7.1), and a [`VirtualEndpoint`] is
//! one: other applications read what this process sends
//! ([`send`](VirtualEndpoint::send), [`sender`](VirtualEndpoint::sender)) and
//! write what this process receives, which is delivered into an
//! [`InputProducerHandle`] exactly as a device input does, so its events are
//! stamped on the same clock and reach the graph through the same inlets. The
//! Function Blocks of the [`VirtualSpec`] declare which way each Group goes.
//! Dropping the endpoint removes it from the system.
//!
//! An endpoint is described by a [`VirtualSpec`]: its name, its Device Identity and
//! Product Instance Id, the UMP protocol it carries, and the Function Blocks it
//! declares. What a platform cannot do is an
//! error from the call that asks for it: creating on a platform with no backend
//! is [`MidiHardwareError::Unsupported`], and declaring Function
//! Blocks where the platform cannot publish them is
//! [`MidiHardwareError::VirtualBlocksUnsupported`]. The protocol is
//! the exception, because a platform may carry the other one than asked for:
//! [`VirtualEndpoint::protocol`] reports the one the endpoint was created with.
//!
//! # What each platform publishes
//!
//! - **Linux:** one sequencer client with one UMP Endpoint record and one port
//!   that is readable and writable.
//! - **macOS 15 and later, with Function Blocks:** one `MIDIUMPMutableEndpoint`,
//!   which has a source and a destination, and the OS numbers the blocks itself
//!   ([`VirtualEndpoint::blocks`] reads the numbers peers see).
//! - **macOS without Function Blocks:** a source and a destination under one name,
//!   which is what CoreMIDI offers for an endpoint with no blocks to declare (before
//!   macOS 15 it is all CoreMIDI offers).
//!
//! On macOS a peer reads from the endpoint's source and writes to its destination;
//! on Linux it subscribes to the one port in either direction.
//!
//! # Opening is polled
//!
//! [`VirtualEndpoint::open`] takes the [`Started`] a [`Start`](crate::Start)
//! resolved to and returns an [`OpeningVirtualEndpoint`], which is polled with the
//! current time until it yields the endpoint. Most platforms are ready on the first
//! poll. macOS 15 with Function Blocks registers and enables the endpoint, which
//! CoreMIDI has been seen to refuse for a few milliseconds while other processes
//! create endpoints: a poll then reports `Pending`, and
//! [`deadline`](OpeningVirtualEndpoint::deadline) is the instant the next poll can
//! make progress. Nothing waits inside it, and there is no blocking form: the
//! caller polls from its own loop or timer.
//!
//! # Main thread
//!
//! Opening an endpoint takes a [`MainThreadMarker`] at each poll, and a
//! [`VirtualEndpoint`] and its opening are `!Send` and `!Sync` on every platform:
//! macOS 15's UMP endpoint with Function Blocks is a main-thread object, and the
//! traits an endpoint has do not differ by operating system. Other backends ignore
//! the marker. What sends from another thread is a [`VirtualSender`]
//! ([`VirtualEndpoint::sender`]).

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use tutti_platform::Instant;
use tutti_rt::LockExt;

use tutti_midi_device::FunctionBlock;
use tutti_midi_protocol::{Midi1Translator, Midi2Translator};
use tutti_platform::{Executor, MainThreadMarker};
use tutti_ump::{DeviceIdentity, EndpointName, FunctionBlockId, Message, ProductInstanceId, Ump};

use crate::InputProducerHandle;
use crate::backend;
use crate::error::{MidiHardwareError, Result};
use crate::start::Started;
use crate::wire::translate_voice;

/// The UMP protocol an endpoint carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VirtualProtocol {
    /// MIDI 1.0 channel voice messages in UMP packets (message type 2).
    Midi1,
    /// MIDI 2.0 Protocol messages (message type 4).
    Midi2,
}

/// What a virtual endpoint is: built with [`new`](Self::new), which starts a
/// MIDI 2.0 endpoint with no Function Blocks.
///
/// The endpoint is declared with the Device Identity (M2-104 §7.1.3) and the
/// optional Product Instance Id (§7.1.5) it is built with: an app gives the one it
/// presents to the session ([`MidiSession::present`](crate::MidiSession::present))
/// and the Id the session reports ([`MidiSession::instance_id`](crate::MidiSession::instance_id)),
/// so other apps recognise the endpoint again. Both backends publish both: macOS
/// with the endpoint's Function Blocks, ALSA in the endpoint's record.
#[derive(Clone, Debug)]
#[must_use = "a spec does nothing until an endpoint is opened from it"]
pub struct VirtualSpec {
    name: EndpointName,
    device: DeviceIdentity,
    instance: Option<ProductInstanceId>,
    /// The protocol asked for; the backends read it.
    pub(crate) protocol: VirtualProtocol,
    blocks: Vec<FunctionBlock>,
}

impl VirtualSpec {
    /// An endpoint named `name`, identified as `device` with Product Instance Id
    /// `instance`, carrying MIDI 2.0.
    pub fn new(
        name: EndpointName,
        device: DeviceIdentity,
        instance: Option<ProductInstanceId>,
    ) -> Self {
        Self {
            name,
            device,
            instance,
            protocol: VirtualProtocol::Midi2,
            blocks: Vec::new(),
        }
    }

    /// The endpoint carrying `protocol`. Where the platform has no such
    /// protocol the endpoint reports the one it got.
    pub fn protocol(mut self, protocol: VirtualProtocol) -> Self {
        self.protocol = protocol;
        self
    }

    /// The endpoint declaring `block`, replacing a block of the same number. The
    /// number another application reads for the block is the platform's, which
    /// [`VirtualEndpoint::blocks`] reports.
    pub fn block(mut self, block: FunctionBlock) -> Self {
        self.blocks.retain(|held| held.id() != block.id());
        self.blocks.push(block);
        self
    }

    /// The endpoint's name.
    #[must_use]
    pub fn name(&self) -> &EndpointName {
        &self.name
    }

    /// The Device Identity the endpoint is declared with.
    #[must_use]
    #[allow(
        dead_code,
        reason = "the ALSA and macOS backends read it; Windows has no virtual endpoint"
    )]
    pub(crate) fn device(&self) -> &DeviceIdentity {
        &self.device
    }

    /// The Product Instance Id the endpoint is declared with, when it has one.
    #[must_use]
    #[allow(
        dead_code,
        reason = "the ALSA and macOS backends read it; Windows has no virtual endpoint"
    )]
    pub(crate) fn instance(&self) -> Option<&ProductInstanceId> {
        self.instance.as_ref()
    }

    /// The Function Blocks declared, in the order they were added.
    #[must_use]
    pub fn function_blocks(&self) -> &[FunctionBlock] {
        &self.blocks
    }
}

/// The platform's side of a [`VirtualEndpoint`]: the endpoint itself, which stays
/// on the thread that opened it.
pub(crate) trait EndpointBackend {
    /// What sends on the endpoint from any thread. It does not keep the endpoint
    /// alive: once the endpoint is dropped a send is
    /// [`MidiHardwareError::EndpointGone`].
    /// The drop waits for a send in flight, and no send reaches the OS after it.
    fn sender(&self) -> Arc<dyn SenderBackend>;

    /// Whether the endpoint is still delivering what peers write to it.
    fn is_alive(&self) -> bool;

    /// The numbers the OS gave the blocks, indexed by the number the spec gave each,
    /// when it numbers them itself; `None` when it publishes the spec's own.
    fn block_numbers(&self) -> Option<&[FunctionBlockId]> {
        None
    }
}

/// The platform's send on a [`VirtualEndpoint`], from any thread.
pub(crate) trait SenderBackend: Send + Sync {
    /// Publishes one UMP message to the endpoint's subscribers. Never called
    /// from the endpoint's own receive callback: a send holds the endpoint's
    /// live lock, which its drop waits on.
    fn send(&self, ump: &Ump) -> Result<()>;
}

/// The platform's creation of an endpoint, polled until it is made.
pub(crate) trait OpeningBackend {
    /// One poll at `now`: the endpoint and the protocol it carries, or `Pending`
    /// until [`deadline`](Self::deadline).
    fn poll(
        &mut self,
        main: MainThreadMarker,
        now: Instant,
    ) -> Poll<Result<(Box<dyn EndpointBackend>, VirtualProtocol)>>;

    /// When the next poll can make progress.
    fn deadline(&self) -> Instant;
}

/// A creation that is ready on the first poll: the endpoint, or why there is none.
pub(crate) struct Immediate {
    made: Option<Result<(Box<dyn EndpointBackend>, VirtualProtocol)>>,
    created_at: Instant,
}

impl Immediate {
    pub(crate) fn new(made: Result<(Box<dyn EndpointBackend>, VirtualProtocol)>) -> Self {
        Self {
            made: Some(made),
            created_at: Instant::now(),
        }
    }
}

impl OpeningBackend for Immediate {
    fn poll(
        &mut self,
        _main: MainThreadMarker,
        _now: Instant,
    ) -> Poll<Result<(Box<dyn EndpointBackend>, VirtualProtocol)>> {
        Poll::Ready(
            self.made
                .take()
                .unwrap_or(Err(MidiHardwareError::EndpointGone)),
        )
    }

    fn deadline(&self) -> Instant {
        self.created_at
    }
}

/// The numbers other applications read for `spec`'s blocks, in the order the spec
/// declares them: the OS's, indexed by the number the spec gave, or the spec's own.
fn published_numbers(spec: &VirtualSpec, os: Option<&[FunctionBlockId]>) -> Vec<FunctionBlockId> {
    spec.function_blocks()
        .iter()
        .map(|block| {
            os.and_then(|os| os.get(usize::from(block.id().get())).copied())
                .unwrap_or_else(|| block.id())
        })
        .collect()
}

/// How an endpoint brings a message to its protocol.
enum Conversion {
    /// A MIDI 1.0 endpoint: MIDI 2.0 voice messages translate through the one
    /// [`Midi1Translator`] of the endpoint's life (M2-104 Appendix D.2).
    ToMidi1(Box<Mutex<Midi1Translator>>),
    /// A MIDI 2.0 endpoint: MIDI 1.0 voice messages translate, holding the
    /// Registered and Assignable Controller runs a MIDI 1.0 stream spreads over
    /// several messages.
    ToMidi2(Box<Mutex<Midi2Translator>>),
}

/// What an endpoint and its senders share: the platform's send and the narrowing
/// state of the endpoint's life.
struct Outbound {
    sender: Arc<dyn SenderBackend>,
    conversion: Conversion,
}

impl Outbound {
    fn send(&self, ump: Ump) -> Result<()> {
        let ump = match &self.conversion {
            Conversion::ToMidi1(midi1_translator) => {
                // A packet no message is defined for, and any that is not MIDI 2.0
                // voice, is forwarded as it is.
                let Ok(message @ Message::Midi2(..)) = ump.decode() else {
                    return self.sender.send(&ump);
                };
                // The packets go in order under the lock: the MIDI 1.0 translator assumes
                // that what it emitted was sent before the next message.
                let mut midi1_translator = midi1_translator.lock_recover();
                let mut sent = Ok(());
                let count = translate_voice(&mut midi1_translator, message, |packet| {
                    if sent.is_ok() {
                        sent = self.sender.send(&packet);
                    }
                });
                if count == 0 {
                    // A MIDI 2.0 voice message with a MIDI 1.0 form translates to at
                    // least one packet, so zero packets is a message with none:
                    // `Message::try_to_midi1` returns its `NoMidi1Form` here and its
                    // `Ok` is not reached.
                    message.try_to_midi1()?;
                }
                return sent;
            }
            Conversion::ToMidi2(translator) => {
                let translated = translator.lock_recover().translate(ump);
                match translated {
                    Some(ump) => ump,
                    None => return Ok(()),
                }
            }
        };
        self.sender.send(&ump)
    }
}

/// The endpoint this process publishes: other applications read what it sends and
/// write what it receives.
///
/// `!Send` and `!Sync` on every platform (see the module docs): it stays on the
/// thread that opened it, and [`sender`](Self::sender) is what sends from another.
///
/// ```compile_fail,E0277
/// fn crosses_threads<T: Send>() {}
/// crosses_threads::<tutti_midi_hardware::VirtualEndpoint>();
/// ```
#[must_use = "dropping it removes the endpoint"]
pub struct VirtualEndpoint {
    backend: Box<dyn EndpointBackend>,
    outbound: Arc<Outbound>,
    name: EndpointName,
    protocol: VirtualProtocol,
    blocks: Vec<FunctionBlockId>,
    _main: MainThreadMarker,
}

impl core::fmt::Debug for VirtualEndpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtualEndpoint")
            .field("name", &self.name.as_str())
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl VirtualEndpoint {
    /// Starts publishing the endpoint `spec` describes, delivering what peers write
    /// to it into `producer`; a backend that reads on a thread of its own starts it
    /// on `executor`. `started` is the proof the backend is ready
    /// ([`Start`](crate::Start)). The returned [`OpeningVirtualEndpoint`] is polled to
    /// the endpoint; it takes the `main` that proves the caller is on the main
    /// thread, where macOS requires an endpoint with Function Blocks to be made and
    /// dropped, at each poll.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::BackendStarting`]
    /// when the backend is not ready after all. A platform with no backend, a
    /// `spec` that declares Function Blocks the platform cannot publish and an OS
    /// that refuses the endpoint are the polls' errors
    /// ([`OpeningVirtualEndpoint::poll`]).
    pub fn open(
        executor: &Executor,
        spec: &VirtualSpec,
        producer: InputProducerHandle,
        started: Started,
        main: MainThreadMarker,
    ) -> Result<OpeningVirtualEndpoint> {
        let opening = backend::open_virtual_endpoint(executor, spec, producer, started, main)?;
        Ok(OpeningVirtualEndpoint {
            opening: Some(opening),
            spec: spec.clone(),
            _main: main,
            _thread: PhantomData,
        })
    }

    /// Sends `ump` to everything connected to the endpoint, in the endpoint's
    /// [`protocol`](Self::protocol).
    ///
    /// A MIDI 2.0 voice message translates to its MIDI 1.0 messages on a MIDI 1.0
    /// endpoint (M2-115, M2-104 Appendix D.2): a Program Change with a bank is
    /// the Bank Select controllers and the Program Change, and a Registered or
    /// Assignable Controller is its four Control Changes, the parameter select
    /// sent every time. A MIDI 1.0 voice message translates on
    /// a MIDI 2.0 one; a MIDI 1.0 Registered or Assignable Controller is several
    /// messages, so the translated controller is sent with the message that
    /// completes it and the earlier ones send nothing. Any other message is
    /// sent as it is.
    ///
    /// On a MIDI 1.0 endpoint a MIDI 1.0 packet is sent as it is and bypasses
    /// the narrowing state. A send the OS refuses leaves that state as it was.
    ///
    /// Not real-time safe: it holds a lock across the OS send. Call it from a
    /// control thread.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::NoMidi1Form`] for a MIDI 2.0 message
    /// on a MIDI 1.0 endpoint that has no MIDI 1.0 form even in sequence (a
    /// per-note controller, per-note management); nothing is sent. Otherwise
    /// the platform's error when the OS refuses the message.
    pub fn send(&self, ump: Ump) -> Result<()> {
        self.outbound.send(ump)
    }

    /// A handle that sends as [`send`](Self::send) does from any thread. It does
    /// not keep the endpoint alive: after the endpoint is dropped its sends are
    /// [`MidiHardwareError::EndpointGone`].
    pub fn sender(&self) -> VirtualSender {
        VirtualSender {
            outbound: Arc::clone(&self.outbound),
        }
    }

    /// Whether the endpoint is still delivering: `false` once its reader ended
    /// on an error, while the endpoint is still published.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.backend.is_alive()
    }

    /// The numbers other applications read for the spec's Function Blocks, in the
    /// order the spec declares them: the platform's own when it numbers the blocks
    /// (macOS does, in order of the spec's numbers), the spec's numbers where it
    /// publishes those (ALSA).
    #[must_use]
    pub fn blocks(&self) -> &[FunctionBlockId] {
        &self.blocks
    }

    /// The endpoint's name.
    #[must_use]
    pub fn name(&self) -> &EndpointName {
        &self.name
    }

    /// The protocol the endpoint was created with.
    #[must_use]
    pub fn protocol(&self) -> VirtualProtocol {
        self.protocol
    }
}

/// A [`VirtualEndpoint`] being made: polled until the platform has published it.
///
/// [`VirtualEndpoint::open`] makes one. Most platforms are ready on the first
/// poll; macOS 15 with Function Blocks retries a refused registration or enable
/// ([`deadline`](Self::deadline) is the instant of the next attempt). It waits for
/// nothing itself. Dropping it before it finished removes what it had published.
///
/// ```compile_fail,E0277
/// fn crosses_threads<T: Send>() {}
/// crosses_threads::<tutti_midi_hardware::OpeningVirtualEndpoint>();
/// ```
#[must_use = "an opening that is not polled publishes nothing"]
pub struct OpeningVirtualEndpoint {
    // `None` once it has finished.
    opening: Option<Box<dyn OpeningBackend>>,
    spec: VirtualSpec,
    _main: MainThreadMarker,
    // Makes the type `!Send` and `!Sync` whatever its fields are.
    _thread: PhantomData<*const ()>,
}

impl core::fmt::Debug for OpeningVirtualEndpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpeningVirtualEndpoint")
            .field("name", &self.spec.name().as_str())
            .finish_non_exhaustive()
    }
}

impl OpeningVirtualEndpoint {
    /// When the next poll can make progress: the next attempt's instant.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.opening
            .as_ref()
            .map_or_else(Instant::now, |opening| opening.deadline())
    }

    /// One poll at `now`: the endpoint once the platform has published it, `Pending`
    /// until [`deadline`](Self::deadline) while it retries. `main` proves the caller
    /// is on the main thread.
    ///
    /// A poll after it finished, with the endpoint or an error, is
    /// [`MidiHardwareError::EndpointGone`].
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::Unsupported`] on a
    /// platform with no backend,
    /// [`MidiHardwareError::VirtualBlocksUnsupported`]
    /// when the spec declares Function Blocks the platform cannot publish, and the
    /// platform's own error when the OS refuses the endpoint (on macOS a refusal
    /// that outlasts the retries is the platform's timeout).
    pub fn poll(&mut self, main: MainThreadMarker, now: Instant) -> Poll<Result<VirtualEndpoint>> {
        let Some(opening) = self.opening.as_mut() else {
            return Poll::Ready(Err(MidiHardwareError::EndpointGone));
        };
        let made = match opening.poll(main, now) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(made) => made,
        };
        self.opening = None;
        Poll::Ready(made.map(|(backend, protocol)| self.published(backend, protocol, main)))
    }

    fn published(
        &self,
        backend: Box<dyn EndpointBackend>,
        protocol: VirtualProtocol,
        main: MainThreadMarker,
    ) -> VirtualEndpoint {
        let conversion = match protocol {
            VirtualProtocol::Midi1 => {
                Conversion::ToMidi1(Box::new(Mutex::new(Midi1Translator::new())))
            }
            VirtualProtocol::Midi2 => {
                Conversion::ToMidi2(Box::new(Mutex::new(Midi2Translator::new())))
            }
        };
        let outbound = Arc::new(Outbound {
            sender: backend.sender(),
            conversion,
        });
        let blocks = published_numbers(&self.spec, backend.block_numbers());
        VirtualEndpoint {
            backend,
            outbound,
            name: self.spec.name,
            protocol,
            blocks,
            _main: main,
        }
    }
}

/// A [`VirtualEndpoint`]'s send, from any thread.
///
/// ```
/// fn crosses_threads<T: Clone + Send + Sync>() {}
/// crosses_threads::<tutti_midi_hardware::VirtualSender>();
/// ```
#[derive(Clone)]
pub struct VirtualSender {
    outbound: Arc<Outbound>,
}

impl core::fmt::Debug for VirtualSender {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtualSender").finish_non_exhaustive()
    }
}

impl VirtualSender {
    /// As [`VirtualEndpoint::send`].
    ///
    /// # Errors
    ///
    /// As [`VirtualEndpoint::send`], and
    /// [`MidiHardwareError::EndpointGone`]
    /// once the endpoint is dropped.
    pub fn send(&self, ump: Ump) -> Result<()> {
        self.outbound.send(ump)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tutti_ump::{ControlValue, MidiChannel, MidiGroup, Rpn, Voice1, Voice2};

    use super::*;
    use crate::error::MidiHardwareError;

    /// A backend that records what it sent and refuses its `fail_at`th send.
    struct Recorder {
        sent: Arc<Mutex<Vec<Ump>>>,
        calls: AtomicUsize,
        fail_at: usize,
    }

    impl SenderBackend for Recorder {
        fn send(&self, ump: &Ump) -> Result<()> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == self.fail_at {
                return Err(MidiHardwareError::Unsupported("recorder"));
            }
            self.sent.lock().unwrap().push(*ump);
            Ok(())
        }
    }

    fn midi1_source(fail_at: usize) -> (Outbound, Arc<Mutex<Vec<Ump>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let outbound = Outbound {
            sender: Arc::new(Recorder {
                sent: sent.clone(),
                calls: AtomicUsize::new(0),
                fail_at,
            }),
            conversion: Conversion::ToMidi1(Box::new(Mutex::new(Midi1Translator::new()))),
        };
        (outbound, sent)
    }

    fn bend_range(semitones: u32) -> Ump {
        let address = tutti_ump::Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::FIRST,
        };
        Message::midi2(
            address,
            Voice2::registered(
                Rpn::PITCH_BEND_SENSITIVITY,
                ControlValue::new(semitones << 25),
            ),
        )
        .into()
    }

    fn controllers(sent: &Mutex<Vec<Ump>>) -> Vec<u8> {
        sent.lock()
            .unwrap()
            .iter()
            .filter_map(|ump| match ump.decode() {
                Ok(Message::Midi1(_, Voice1::ControlChange { controller, .. })) => {
                    Some(controller.get())
                }
                _ => None,
            })
            .collect()
    }

    // M2-104 Appendix D.2.3: every controller is the whole sequence of four, so a
    // send that fails mid-sequence leaves the next controller whole.
    #[test]
    fn a_refused_send_makes_the_next_registered_controller_send_its_select_again() {
        // The first controller's second packet (CC 100) is refused.
        let (outbound, sent) = midi1_source(1);
        assert!(matches!(
            outbound.send(bend_range(12)),
            Err(MidiHardwareError::Unsupported(_))
        ));
        outbound.send(bend_range(7)).unwrap();
        // CC 101 of the first; then the whole sequence of the second.
        assert_eq!(controllers(&sent), [101, 101, 100, 6, 38]);
    }

    /// **A failed send leaves an MPE note's member channel as it was**: the
    /// The MIDI 1.0 translator's note allocation is not dropped by a refused packet, so the
    /// note's per-note bend still goes to its channel (M2-104 D.2.8, MPE mode).
    #[test]
    fn a_failed_send_keeps_an_mpe_notes_channel() {
        use tutti_midi_protocol::{
            MpeZoneConfig, PerNoteTranslation, PitchBendSensitivity, TranslationMode,
        };
        use tutti_ump::{Address, Attribute, BendValue, ExtremeValues, NoteNumber, NoteVelocity};
        let zone = MpeZoneConfig::lower(
            2,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap();
        let midi1_translator = Midi1Translator::with_mode(TranslationMode {
            extremes: ExtremeValues::Scaled,
            per_note: PerNoteTranslation::Mpe(zone),
        });
        // Send 0 (the Note On) succeeds and send 1 (the RPN's CC 101) is refused.
        let (mut outbound, sent) = midi1_source(1);
        outbound.conversion = Conversion::ToMidi1(Box::new(Mutex::new(midi1_translator)));
        let address = Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::FIRST,
        };
        let note = NoteNumber::of(60);
        outbound
            .send(
                Message::midi2(
                    address,
                    Voice2::NoteOn {
                        note,
                        velocity: NoteVelocity::new(0x8000),
                        attribute: Attribute::None,
                    },
                )
                .into(),
            )
            .unwrap();
        assert!(outbound.send(bend_range(12)).is_err());
        outbound
            .send(
                Message::midi2(
                    address,
                    Voice2::PerNotePitchBend {
                        note,
                        value: BendValue::new(0x8000_0000),
                    },
                )
                .into(),
            )
            .unwrap();
        let words: Vec<u32> = sent.lock().unwrap().iter().map(|u| u.0[0]).collect();
        // Note On on member channel 2 (0x91), then its Pitch Bend, centre 0x2000.
        assert_eq!(words, [0x2091_3C40, 0x20E1_0040]);
    }

    // M2-104 Appendix D.2.3: the same controller twice is the four messages twice.
    #[test]
    fn a_second_value_of_the_same_parameter_sends_the_select_again() {
        let (outbound, sent) = midi1_source(usize::MAX);
        outbound.send(bend_range(12)).unwrap();
        outbound.send(bend_range(7)).unwrap();
        assert_eq!(controllers(&sent), [101, 100, 6, 38, 101, 100, 6, 38]);
    }
}
