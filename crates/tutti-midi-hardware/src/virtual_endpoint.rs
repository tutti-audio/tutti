//! Endpoints this process publishes for other applications to connect to.
//!
//! A [`VirtualSource`] is an endpoint other applications read from: this
//! process sends UMP into it. A [`VirtualDestination`] is one they write to:
//! this process receives what they send. The two are different types, so
//! receiving on a source and sending to a destination do not compile. Dropping
//! either removes the endpoint from the system.
//!
//! An endpoint is described by a [`VirtualSpec`]: its name, the UMP protocol it
//! carries, and the Function Blocks it declares. What a platform cannot do is an
//! error from the call that asks for it: creating on a platform with no backend
//! is [`MidiHardwareError::Unsupported`](crate::MidiHardwareError::Unsupported), and declaring Function
//! Blocks where the platform cannot publish them is
//! [`MidiHardwareError::VirtualBlocksUnsupported`](crate::MidiHardwareError::VirtualBlocksUnsupported). The protocol is
//! the exception, because a platform may carry the other one than asked for:
//! [`VirtualSource::protocol`] and [`VirtualDestination::protocol`] report the
//! one the endpoint was created with.
//!
//! A destination delivers into an [`InputProducerHandle`], exactly as a device
//! input does, so its events are stamped on the same clock and reach the graph
//! through the same inlets.

use std::sync::Mutex;
use tutti_rt::LockExt;

use tutti_midi_device::FunctionBlock;
use tutti_midi_protocol::{Midi1Translator, Midi2Translator};
use tutti_ump::EndpointName;
use tutti_ump::{Message, Ump};

use crate::InputProducerHandle;
use crate::backend;
use crate::endpoints::InputConnection;
use crate::error::Result;
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
#[derive(Clone, Debug)]
#[must_use = "a spec does nothing until an endpoint is opened from it"]
pub struct VirtualSpec {
    name: EndpointName,
    /// The protocol asked for; the backends read it.
    pub(crate) protocol: VirtualProtocol,
    blocks: Vec<FunctionBlock>,
}

impl VirtualSpec {
    /// An endpoint named `name`, carrying MIDI 2.0.
    pub fn new(name: EndpointName) -> Self {
        Self {
            name,
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

    /// The endpoint declaring `block`, replacing a block of the same number.
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

    /// The Function Blocks declared, in the order they were added.
    #[must_use]
    pub fn function_blocks(&self) -> &[FunctionBlock] {
        &self.blocks
    }
}

/// The platform's side of a [`VirtualSource`].
pub(crate) trait SourceBackend: Send + Sync {
    /// Publishes one UMP message to the endpoint's subscribers.
    fn send(&self, ump: &Ump) -> Result<()>;
}

/// How a source brings a message to its protocol.
enum Conversion {
    /// A MIDI 1.0 endpoint: MIDI 2.0 voice messages translate through the one
    /// [`Midi1Translator`] of the endpoint's life (M2-104 Appendix D.2).
    ToMidi1(Box<Mutex<Midi1Translator>>),
    /// A MIDI 2.0 endpoint: MIDI 1.0 voice messages translate, holding the
    /// Registered and Assignable Controller runs a MIDI 1.0 stream spreads over
    /// several messages.
    ToMidi2(Box<Mutex<Midi2Translator>>),
}

/// An endpoint other applications read from.
#[must_use = "dropping it removes the endpoint"]
pub struct VirtualSource {
    backend: Box<dyn SourceBackend>,
    conversion: Conversion,
    name: EndpointName,
    protocol: VirtualProtocol,
}

impl core::fmt::Debug for VirtualSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtualSource")
            .field("name", &self.name.as_str())
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl VirtualSource {
    /// Publishes the endpoint `spec` describes.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::Unsupported`](crate::MidiHardwareError::Unsupported) on a platform with no
    /// backend, [`MidiHardwareError::VirtualBlocksUnsupported`](crate::MidiHardwareError::VirtualBlocksUnsupported)
    /// when `spec` declares Function Blocks the platform cannot publish, and the
    /// platform's own error when the OS refuses the endpoint.
    pub fn open(spec: &VirtualSpec) -> Result<Self> {
        let (backend, protocol) = backend::open_virtual_source(spec)?;
        let conversion = match protocol {
            VirtualProtocol::Midi1 => {
                Conversion::ToMidi1(Box::new(Mutex::new(Midi1Translator::new())))
            }
            VirtualProtocol::Midi2 => {
                Conversion::ToMidi2(Box::new(Mutex::new(Midi2Translator::new())))
            }
        };
        Ok(Self {
            backend,
            conversion,
            name: spec.name,
            protocol,
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
    /// [`MidiHardwareError::NoMidi1Form`](crate::MidiHardwareError::NoMidi1Form) for a MIDI 2.0 message
    /// on a MIDI 1.0 endpoint that has no MIDI 1.0 form even in sequence (a
    /// per-note controller, per-note management); nothing is sent. Otherwise
    /// the platform's error when the OS refuses the message.
    pub fn send(&self, ump: Ump) -> Result<()> {
        let ump = match &self.conversion {
            Conversion::ToMidi1(midi1_translator) => {
                // A packet no message is defined for, and any that is not MIDI 2.0
                // voice, is forwarded as it is.
                let Ok(message @ Message::Midi2(..)) = ump.decode() else {
                    return self.backend.send(&ump);
                };
                // The packets go in order under the lock: the MIDI 1.0 translator assumes
                // that what it emitted was sent before the next message.
                let mut midi1_translator = midi1_translator.lock_recover();
                let mut sent = Ok(());
                let count = translate_voice(&mut midi1_translator, message, |packet| {
                    if sent.is_ok() {
                        sent = self.backend.send(&packet);
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
        self.backend.send(&ump)
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

/// An endpoint other applications write to; what they send is delivered to the
/// [`InputProducerHandle`] it was opened with.
#[must_use = "dropping it removes the endpoint"]
pub struct VirtualDestination {
    connection: Box<dyn InputConnection>,
    name: EndpointName,
    protocol: VirtualProtocol,
}

impl core::fmt::Debug for VirtualDestination {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtualDestination")
            .field("name", &self.name.as_str())
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl VirtualDestination {
    /// Publishes the endpoint `spec` describes, delivering into `producer`.
    ///
    /// # Errors
    ///
    /// As [`VirtualSource::open`].
    pub fn open(spec: &VirtualSpec, producer: InputProducerHandle) -> Result<Self> {
        let (connection, protocol) = backend::open_virtual_destination(spec, producer)?;
        Ok(Self {
            connection,
            name: spec.name,
            protocol,
        })
    }

    /// Whether the endpoint is still delivering: `false` once its reader ended
    /// on an error, while the endpoint is still published.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.connection.is_alive()
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

    impl SourceBackend for Recorder {
        fn send(&self, ump: &Ump) -> Result<()> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == self.fail_at {
                return Err(MidiHardwareError::Unsupported("recorder"));
            }
            self.sent.lock().unwrap().push(*ump);
            Ok(())
        }
    }

    fn midi1_source(fail_at: usize) -> (VirtualSource, Arc<Mutex<Vec<Ump>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let source = VirtualSource {
            backend: Box::new(Recorder {
                sent: sent.clone(),
                calls: AtomicUsize::new(0),
                fail_at,
            }),
            conversion: Conversion::ToMidi1(Box::new(Mutex::new(Midi1Translator::new()))),
            name: EndpointName::new("recorder").unwrap(),
            protocol: VirtualProtocol::Midi1,
        };
        (source, sent)
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
        let (source, sent) = midi1_source(1);
        assert!(matches!(
            source.send(bend_range(12)),
            Err(MidiHardwareError::Unsupported(_))
        ));
        source.send(bend_range(7)).unwrap();
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
        let (mut source, sent) = midi1_source(1);
        source.conversion = Conversion::ToMidi1(Box::new(Mutex::new(midi1_translator)));
        let address = Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::FIRST,
        };
        let note = NoteNumber::of(60);
        source
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
        assert!(source.send(bend_range(12)).is_err());
        source
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
        let (source, sent) = midi1_source(usize::MAX);
        source.send(bend_range(12)).unwrap();
        source.send(bend_range(7)).unwrap();
        assert_eq!(controllers(&sent), [101, 100, 6, 38, 101, 100, 6, 38]);
    }
}
