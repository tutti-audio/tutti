//! [`MidiSession`] — what is connected, and the sink to reach it.
//!
//! Three properties this type is built around:
//!
//! 1. **It owns no driver code.** Everything OS-specific is behind
//!    [`MidiEndpoints`], so this file has no `#[cfg]` at all.
//! 2. **It holds no output.** [`connect_output`](MidiSession::connect_output)
//!    hands the caller the [`OpenOutput`] it opened, which the caller owns and
//!    queues through with `&mut self`: nothing here sends, so the session
//!    shares no output, and an unconnected output is a value the caller does
//!    not have rather than a sink that silently swallows.
//! 3. **No background threads of its own.** A native-UMP send is a
//!    non-blocking syscall and connections are held right here. The threads
//!    that exist belong to the values handed out: the ALSA *input* pump is the
//!    backend's, and an [`OpenOutput`] asked to
//!    [`active_sensing`](OpenOutput::active_sensing) runs one thread that
//!    shares that output's sink with its owner until it drops.
//!
//! # Threading
//!
//! One owner, on the control thread: the session is a plain value, every call
//! that changes it takes `&mut self`, and it is `Send` but is not shared.
//! Inbound **events** never pass through this type — a backend pushes them straight into the
//! [`InputProducerHandle`] of a [`HardwareMidiInputs`] port, which places each
//! arrival on a frame and pushes it into the device's event inlet, whose node
//! [`connect_input`](MidiSession::connect_input) hands the caller to add to the
//! graph. What [`poll`](MidiSession::poll) steps is only the
//! devices' *talk about themselves*: the UMP Stream negotiation of each open
//! input, read from the port's stream ring.
//!
//!
//! [`HardwareMidiInputs`]: crate::HardwareMidiInputs
//! [`InputProducerHandle`]: crate::InputProducerHandle

use std::collections::HashMap;
use std::sync::Arc;
use tutti_platform::Instant;

use tutti_midi_ci::ResourceStore;

use crate::answer::Answer;
use crate::backend;
use crate::capability::legacy;
use crate::ci::{CiChange, CiPeerInfo, Heard, PeerId};
use crate::devices::{Devices, OpenInput, OpenOutput};
use crate::endpoints::MidiEndpoints;
use crate::error::{MidiHardwareError, Result};
use crate::peer::{PeerError, PeerRef};
use crate::process::{ProcessChange, ReportAsk, Reporter};
use crate::profiles::{PeerProfiles, ProfileError, ProfileSwitch, ProfilesChange};
use crate::properties::{PropertyAsk, PropertyChange, PropertyPeer};
use crate::stream::{DeviceStream, InputTiming, Negotiation};
use crate::{HardwareMidiInputs, InputFault};
use tutti_graph::EventInletNode;
use tutti_midi_ci::{CiDeviceId, ProfileChange, ProfileId};
use tutti_midi_device::{DeviceId, DeviceInfo, Learned, MidiIdentity, SavedDevice};
use tutti_midi_runtime::{InputInlets, InputWriters};
use tutti_ump::ProductInstanceId;
use tutti_ump::{GroupSpan, MidiGroup};

/// The set of open MIDI inputs, and the endpoints available to open.
///
/// One owner holds it (the host's resource, or the session's) and changes it
/// through `&mut self`; it is not `Clone`. The output is not part of it:
/// [`connect_output`](Self::connect_output) returns the [`OpenOutput`], which
/// its caller owns and sends through.
///
/// # Drop order
///
/// Fields drop in declaration order: the open inputs first (each
/// [`OpenInput`] closes its connection, then retires its port), then the ring
/// manager handle, then the devices and the backend whose client every
/// connection was opened on. Closing a connection stops its driver: the ALSA
/// backend joins its pump thread, which drops the producer handle before the
/// port is retired. On macOS the handle lives in the input callback, which
/// CoreMIDI retains and may release (and so drop the handle) after the port is
/// disposed; a push in that window lands in a ring nobody drains, which is
/// harmless. The order is about not leaving ports behind, not about soundness:
/// each handle owns its half of the ring and cannot outlive the memory it
/// pushes into.
pub struct MidiSession {
    /// Live input connections, keyed by device.
    inputs: HashMap<DeviceId, Held>,
    /// Ring manager the backend pushes inbound events into: each open input
    /// takes a port from it and retires the port on close.
    ports: Arc<HardwareMidiInputs>,
    /// The devices present, and the backend that opens them.
    devices: Devices,
    /// What the session was opened with.
    options: MidiHardwareOptions,
    /// Who the session presents itself as on every input with an output; `None`
    /// presents nothing ([`present`](MidiSession::present)).
    presented: Option<Presented>,
    /// The Product Instance Id the session declares for a [`presented`](Self::presented)
    /// identity that carries none, made once and kept so every rebuilt Endpoint
    /// declares the same one ([`instance_id`](MidiSession::instance_id)).
    generated_instance: Option<ProductInstanceId>,
    /// What the MIDI-CI devices' Property Exchange Responders serve
    /// ([`MidiSession::serve_properties`]); empty by default.
    properties: ResourceStore,
}

/// An identity and the Groups its one Function Block spans: what
/// [`MidiSession::present`] presents on every input with an output.
///
/// A [`MidiIdentity`] under
/// [`ManufacturerId::NON_COMMERCIAL`](tutti_ump::ManufacturerId::NON_COMMERCIAL)
/// is for development and research only: a Discovery carries a System
/// Exclusive manufacturer id, tutti has none, and the one MIDI 1.0 sets aside
/// for non-commercial use is not for a released product, so an app that ships
/// says who it is with the id it registered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Presented {
    /// Who the session is.
    pub identity: MidiIdentity,
    /// The Groups of the Endpoint's one Function Block, from Group 1.
    pub groups: GroupSpan,
}

/// What an OS MIDI layer is opened with: [`MidiSession::open`]'s second
/// argument. A session runs with these for its whole life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct MidiHardwareOptions {
    /// Where the event times of the input ports come from.
    pub input_timing: InputTiming,
}

impl MidiHardwareOptions {
    /// The options of a session opened with none: inputs timed by arrival.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            input_timing: InputTiming::Arrival,
        }
    }

    /// These options with the event times of the input ports from `timing`:
    /// [`Arrival`](InputTiming::Arrival), the OS's arrival stamp (the default), or
    /// [`JitterReduction`](InputTiming::JitterReduction), the JR Timestamps of a
    /// device that agrees to send them.
    #[must_use]
    pub const fn input_timing(mut self, timing: InputTiming) -> Self {
        self.input_timing = timing;
        self
    }
}

/// One open input, and the UMP Stream negotiation of its device.
///
/// The negotiation drops first: it holds the output it asks on, and the input
/// it reads its answers from goes after.
struct Held {
    stream: Negotiation,
    /// Where the input's event times come from.
    timing: InputTiming,
    /// Whether the port's input edge is copying SysEx7 packets to the stream
    /// ring, for the Identity Request or MIDI-CI.
    hearing: bool,
    /// How many stream-ring packets the port had lost when it was last read.
    lost_seen: u64,
    input: OpenInput,
}

impl core::fmt::Debug for MidiSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MidiSession")
            .field("inputs", &self.inputs.len())
            .finish_non_exhaustive()
    }
}

/// Which device a name or an id picks: what
/// [`connect_input`](MidiSession::connect_input), its output and disconnect
/// forms take, from a `&`[`DeviceId`] or a `&str`.
///
/// # Matching a name
///
/// **Case-insensitive substring, first hit wins.** The name is lowercased and
/// tested with `contains` against each device's lowercased name, in the order
/// [`inputs`](MidiSession::inputs) or [`outputs`](MidiSession::outputs)
/// returns them: by id. So `"iac"` matches `"IAC Driver Bus 1"`, and a name
/// matching two devices picks the one with the lowest id. Connect by
/// [`DeviceId`] when that matters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceQuery(Query);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Query {
    Id(DeviceId),
    Name(String),
}

impl From<&DeviceId> for DeviceQuery {
    fn from(id: &DeviceId) -> Self {
        Self(Query::Id(id.clone()))
    }
}

impl From<&str> for DeviceQuery {
    fn from(name: &str) -> Self {
        Self(Query::Name(name.to_owned()))
    }
}

impl DeviceQuery {
    /// Whether this names `device`: its id, or a name that is a
    /// case-insensitive substring of the device's.
    #[must_use]
    pub fn matches(&self, device: &DeviceInfo) -> bool {
        match &self.0 {
            Query::Id(id) => *id == device.id,
            Query::Name(name) => device.name.to_lowercase().contains(&name.to_lowercase()),
        }
    }

    /// The device this picks among `list`, or the error naming what was
    /// searched. An id is taken as it is (the open reports a stale one).
    fn pick(&self, list: Vec<DeviceInfo>) -> Result<DeviceId> {
        match &self.0 {
            Query::Id(id) => Ok(id.clone()),
            Query::Name(name) => {
                let needle = name.to_lowercase();
                match list
                    .iter()
                    .find(|d| d.name.to_lowercase().contains(&needle))
                {
                    Some(found) => Ok(found.id.clone()),
                    None => Err(MidiHardwareError::NoDeviceNamed {
                        name: name.clone(),
                        searched: list.into_iter().map(|d| d.name).collect(),
                    }),
                }
            }
        }
    }
}

/// What [`MidiSession::connect_input`] did.
#[derive(Debug)]
#[must_use = "an opened input's inlet is to be added to the graph"]
pub enum Connected {
    /// The input was opened: the inlet node its port pushes into, for the caller
    /// to add to the graph.
    Opened(EventInletNode),
    /// The input was open and delivering already: nothing opened, so there is no
    /// new inlet.
    AlreadyOpen,
}

/// What one [`MidiSession::poll`] found, for the device it names.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Polled {
    /// The device's UMP Stream negotiation settled or its settled state
    /// changed.
    /// (Boxed: an Endpoint is large.)
    Stream(DeviceId, Box<DeviceStream>),
    /// A MIDI-CI peer of the device (named by its [`PeerId`]) was found, changed
    /// or lost.
    Ci(DeviceId, PeerId, CiChange),
    /// A MIDI-CI peer's Profiles became known or changed, or the peer's
    /// Profiles were dropped.
    Profile(DeviceId, PeerId, ProfilesChange),
    /// A MIDI-CI peer answered Process Inquiry, or the peer's Process Inquiry
    /// was dropped.
    Process(DeviceId, PeerId, ProcessChange),
    /// A request made of a MIDI-CI peer ended: the peer's Report or NAK, the
    /// wait running out, or the peer lost first.
    Answer(DeviceId, PeerId, Answer),
    /// A MIDI-CI peer answered Property Exchange, a call ended, the peer
    /// changed what the device serves, or the peer's Property Exchange was
    /// dropped.
    Property(DeviceId, PeerId, PropertyChange),
    /// MIDI-CI could not start a device on a Group of the device: the operating
    /// system gave no random bytes for its MUID. It is reported once until the
    /// device starts, and again if the Group leaves the device's Groups and
    /// returns; the Group is tried again at every poll.
    CiNotStarted(DeviceId, MidiGroup, getrandom::Error),
}

impl MidiSession {
    /// Opens a session over this platform's backend, its input ports inlets made
    /// by `inlets` (the session builds and owns the [`HardwareMidiInputs`] over
    /// them: [`ports`](Self::ports)).
    ///
    /// `options` are what the session runs with ([`MidiHardwareOptions`]).
    ///
    /// # Errors
    ///
    /// As [`Devices::open`].
    pub fn open(inlets: Arc<InputInlets>, options: MidiHardwareOptions) -> Result<Self> {
        Self::open_over(backend::active(), inlets, options)
    }

    /// A session over a supplied backend.
    ///
    /// This is what makes the session testable: a fake [`MidiEndpoints`] drives
    /// every connect/disconnect path deterministically, on any platform,
    /// including one with no MIDI hardware. Without it a test can assert only
    /// "does not panic".
    ///
    /// # Errors
    ///
    /// As [`Devices::open_over`].
    pub fn open_over(
        backend: Box<dyn MidiEndpoints>,
        inlets: Arc<InputInlets>,
        options: MidiHardwareOptions,
    ) -> Result<Self> {
        Ok(Self {
            inputs: HashMap::new(),
            ports: Arc::new(HardwareMidiInputs::new(inlets)),
            devices: Devices::open_over(backend)?,
            options,
            presented: None,
            generated_instance: None,
            properties: ResourceStore::default(),
        })
    }

    /// The devices present. A host that wants hot-plug subscribes
    /// ([`Devices::subscribe`]); the session's `Phase::Poll` has a
    /// subscription of its own.
    pub fn devices(&self) -> &Devices {
        &self.devices
    }

    /// The input ports the session opened, and the inlets they are made by
    /// ([`HardwareMidiInputs::inlets`]).
    pub fn ports(&self) -> &Arc<HardwareMidiInputs> {
        &self.ports
    }

    // --- Enumeration ---

    /// Devices that can send MIDI to this engine, as of now, ordered by id.
    pub fn inputs(&self) -> Vec<DeviceInfo> {
        self.devices_where(|d| d.direction.is_input())
    }

    /// Devices this engine can send MIDI to, as of now, ordered by id.
    pub fn outputs(&self) -> Vec<DeviceInfo> {
        self.devices_where(|d| d.direction.is_output())
    }

    fn devices_where(&self, keep: impl Fn(&DeviceInfo) -> bool) -> Vec<DeviceInfo> {
        let mut list = self.devices.list();
        list.retain(keep);
        list
    }

    // --- Input ---

    /// Opens an input endpoint, by [`DeviceId`] or by name (see
    /// [`DeviceQuery`]). Idempotent — connecting an already-open endpoint
    /// succeeds without reopening it ([`Connected::AlreadyOpen`]), because a
    /// second driver connection to one device duplicates every inbound event.
    ///
    /// Each newly opened input gets its own port in [`ports`](Self::ports); the
    /// backend pushes into it from the driver's thread and the audio thread
    /// drains it. Events never pass through this type. The inlet node the port
    /// pushes into comes back as [`Connected::Opened`]: the caller adds it to
    /// the graph, and takes it out when the input closes.
    ///
    /// The device's output, if it has one, is opened too, for discovery alone
    /// (the UMP Stream negotiation, or a MIDI 1.0 port's Identity Request, then
    /// MIDI-CI when the session has an identity, all stepped by [`poll`](Self::poll)): it is not an output a caller holds, which
    /// [`connect_output`](Self::connect_output) opens, and it closes with the
    /// input. An output that will not open
    /// leaves the input connected and the device as the OS describes it.
    ///
    /// The source of the input's event times is the session's
    /// [`MidiHardwareOptions::input_timing`]:
    /// [`InputTiming::JitterReduction`] asks the device for JR Timestamps and
    /// places its events by them once it agrees, while [`InputTiming::Arrival`]
    /// is the OS's arrival stamp. An input already open is kept as it is.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::NoDeviceNamed`] when a name matches no input (listing the
    /// inputs it searched; an absent device is an error, never a silent
    /// no-op). [`MidiHardwareError::NotFound`] when an id names no present device — the
    /// shape a stale id takes after a hot-plug — and [`MidiHardwareError::WrongDirection`]
    /// when it is not an input. Otherwise whatever the backend raises while
    /// opening the port.
    pub fn connect_input(&mut self, device: impl Into<DeviceQuery>) -> Result<Connected> {
        self.connect_input_with(device, InputWriters::new().0)
    }

    /// [`connect_input`](Self::connect_input), the input reporting to `writers`:
    /// its clock map's timing, its inlet's losses and the MIDI-CI messages the
    /// device's output refused (a full or gone output; a refused message is not
    /// sent again, so the count is how many a peer may never have heard). The
    /// readers stay with whoever made them ([`InputWriters::new`]) and outlive
    /// the connection. The writers drop with the input, whether it is refused
    /// here (already open, or the OS failed) or closed later, and their cells
    /// close until reopened.
    ///
    /// # Errors
    ///
    /// As [`connect_input`](Self::connect_input).
    pub fn connect_input_with(
        &mut self,
        device: impl Into<DeviceQuery>,
        writers: InputWriters,
    ) -> Result<Connected> {
        let (writers, ci_refused) = writers.split();
        let timing = self.options.input_timing;
        let id = device.into().pick(self.inputs())?;
        // An input whose reader died is replaced, not kept: it is silent.
        if self
            .inputs
            .get(&id)
            .is_some_and(|held| held.input.is_alive())
        {
            return Ok(Connected::AlreadyOpen);
        }
        if let Some(mut held) = self.inputs.remove(&id) {
            held.stream.close();
        }

        // One ring per connection, retired with it.
        let (input, inlet) = self.devices.open_input(&id, &self.ports, writers)?;
        // A UMP Endpoint with an output is asked what it is by the next
        // `poll`. Its output is the negotiation's own, not the session's one
        // destination: asking must not wait for, or disturb, that choice.
        let os = self.devices.capability(&id).unwrap_or_else(legacy);
        // A MIDI 1.0 port with an output is asked for its identity instead.
        let out = self.devices.open_output(&id).ok();
        let stream = Negotiation::new(id.clone(), os, out, timing, ci_refused);
        let held = Held {
            stream,
            timing,
            hearing: false,
            lost_seen: 0,
            input,
        };
        follow_timing(&self.ports, &held);
        self.inputs.insert(id, held);
        Ok(Connected::Opened(inlet))
    }

    /// Closes an open input and retires the port its events landed in, by
    /// [`DeviceId`] or by name. Returns whether one was open; closing one that
    /// is not is a no-op.
    ///
    /// A name is matched against the **open** inputs' names, case-insensitively
    /// as a substring, and the lowest [`DeviceId`] among those matching is the
    /// one closed (at most one: use [`disconnect_all_inputs`](Self::disconnect_all_inputs)
    /// to close every one).
    ///
    /// Events still queued in that port are discarded with it. MIDI-CI devices
    /// running on it send an Invalidate MUID for their own MUID first (best
    /// effort), and no [`CiChange::Lost`] is reported: the caller closed it.
    /// Dropping the session sends nothing.
    pub fn disconnect_input(&mut self, device: impl Into<DeviceQuery>) -> bool {
        let id = match device.into().0 {
            Query::Id(id) => id,
            Query::Name(name) => {
                let needle = name.to_lowercase();
                let Some(id) = self
                    .inputs
                    .iter()
                    .filter(|(_, held)| held.input.name().to_lowercase().contains(&needle))
                    .map(|(id, _)| id)
                    .min()
                    .cloned()
                else {
                    return false;
                };
                id
            }
        };
        match self.inputs.remove(&id) {
            Some(mut held) => {
                held.stream.close();
                true
            }
            None => false,
        }
    }

    /// Closes every open input, retiring each one's port.
    pub fn disconnect_all_inputs(&mut self) {
        for (_, mut held) in self.inputs.drain() {
            held.stream.close();
        }
    }

    /// Whether any input is open.
    #[must_use]
    pub fn is_any_input_connected(&self) -> bool {
        !self.inputs.is_empty()
    }

    /// The names of every open input.
    ///
    /// The order is the map's, not the ids'.
    pub fn connected_input_names(&self) -> Vec<String> {
        self.inputs
            .values()
            .map(|held| held.input.name().to_owned())
            .collect()
    }

    // --- Output ---

    /// Opens an output endpoint, by [`DeviceId`] or by name (see
    /// [`DeviceQuery`]), and returns it: the caller owns the [`OpenOutput`],
    /// queues through it, and closes it by dropping it. The session keeps no
    /// handle to it, so a destination is one value with one owner, and
    /// connecting another is the caller's replacing it.
    ///
    /// It sends in what its device's input has settled when that input is open
    /// and has settled; otherwise in the OS's description of the device. A
    /// later settle is the caller's to apply, from what [`poll`](Self::poll)
    /// reports, with [`OpenOutput::follow`].
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::NoDeviceNamed`] when a name matches no output,
    /// [`MidiHardwareError::NotFound`] when an id names no present device,
    /// [`MidiHardwareError::WrongDirection`] when it is not an output, plus whatever the
    /// backend raises while opening.
    pub fn connect_output(&self, device: impl Into<DeviceQuery>) -> Result<OpenOutput> {
        let id = device.into().pick(self.outputs())?;
        let mut output = self.devices.open_output(&id)?;
        if let Some(stream) = self.inputs.get(&id).and_then(|held| held.stream.settled()) {
            output.set_config(stream.config());
        }
        Ok(output)
    }

    /// Whether `id` is an open input that is still delivering: one whose
    /// reader ended on an error is not (see [`OpenInput::is_alive`]), and
    /// [`connect_input`](Self::connect_input) reopens it.
    #[must_use]
    pub fn is_input_connected(&self, id: &DeviceId) -> bool {
        self.inputs
            .get(id)
            .is_some_and(|held| held.input.is_alive())
    }

    /// Why input `id` stopped delivering: `None` while it delivers and when it
    /// is not open. An input with a fault is one
    /// [`is_input_connected`](Self::is_input_connected) reads `false` for.
    #[must_use]
    pub fn input_fault(&self, id: &DeviceId) -> Option<InputFault> {
        self.inputs.get(id).and_then(|held| held.input.fault())
    }

    /// Steps the UMP Stream negotiation and the MIDI-CI of every open input at
    /// `now`: starts the ones connected since, hands each the Stream messages
    /// and SysEx its device sent, and sends what they answer or ask again.
    /// Returns, once each, the devices whose negotiation settled in this call
    /// and those whose settled state changed ([`Polled::Stream`]: a Stream
    /// Configuration Notification, or a Function Block of an Endpoint that did
    /// not declare them static), and the MIDI-CI peers that were found,
    /// changed or lost ([`Polled::Ci`]). An output a caller holds follows its
    /// device's Protocol and JR Timestamps once the caller hands it each report
    /// ([`OpenOutput::follow`]).
    ///
    /// The stepping is done when this returns; the iterator only hands out what
    /// it found, so ignoring it loses nothing but the report.
    ///
    /// `now` is the caller's time: nothing here reads a clock or waits, so a
    /// device that never answers costs a few packets over a few seconds of
    /// polls and then reads as [`Unanswered`](crate::Basis::Unanswered). Call
    /// it from the control tick (the session's `Phase::Poll`); it only reads
    /// rings and sends.
    pub fn poll(&mut self, now: Instant) -> std::vec::IntoIter<Polled> {
        let mut polled = Vec::new();
        let generated = self.generated_instance;
        let presented = self.presented.map(|mut who| {
            who.identity.instance = who.identity.instance.or(generated);
            who
        });
        let ci = presented.is_some();
        let ports = &*self.ports;
        for (id, held) in &mut self.inputs {
            let slot = held.input.port();
            // The copy of the SysEx7 packets is on before a request goes out
            // and before MIDI-CI starts, which a step decides a poll ahead
            // (`Ci::Idle`), and off once nothing listens.
            sync_hearing(ports, held, ci);
            // The ring's loss is read either side of the drain (`Heard`).
            let lost = || ports.port_info(slot).map_or(0, |p| p.losses.lost);
            let before = lost();
            let said: Vec<_> = ports.drain_stream(slot).collect();
            let after = lost();
            let heard = Heard {
                packets: &said,
                lost_before: before > held.lost_seen,
                lost_during: after > before,
            };
            held.lost_seen = after;
            let stepped = held
                .stream
                .poll(now, &heard, presented, &mut self.properties);
            // Off the moment nothing listens, so no packet is kept for no one.
            sync_hearing(ports, held, ci);
            if let Some(stream) = stepped.stream {
                follow_timing(ports, held);
                polled.push(Polled::Stream(id.clone(), Box::new(stream)));
            }
            polled.extend(
                stepped
                    .ci
                    .into_iter()
                    .map(|(peer, change)| Polled::Ci(id.clone(), peer, change)),
            );
            let refusals = mpe_zone_refusals(&stepped.profiles, &held.stream.profiles());
            polled.extend(
                stepped
                    .profiles
                    .into_iter()
                    .chain(refusals)
                    .map(|(peer, change)| Polled::Profile(id.clone(), peer, change)),
            );
            polled.extend(
                stepped
                    .process
                    .into_iter()
                    .map(|(peer, change)| Polled::Process(id.clone(), peer, change)),
            );
            polled.extend(
                stepped
                    .answers
                    .into_iter()
                    .map(|(peer, answer)| Polled::Answer(id.clone(), peer, answer)),
            );
            polled.extend(
                stepped
                    .properties
                    .into_iter()
                    .map(|(peer, change)| Polled::Property(id.clone(), peer, change)),
            );
            polled.extend(
                stepped
                    .unstarted
                    .into_iter()
                    .map(|(group, error)| Polled::CiNotStarted(id.clone(), group, error)),
            );
        }
        polled.into_iter()
    }

    /// Presents the session as `who` on every open input with an output and on
    /// every one opened later, from the next [`poll`](Self::poll): MIDI-CI runs
    /// as its identity (a Discovery from each device), and a peer's UMP
    /// Endpoint Discovery, Function Block Discovery and Stream Configuration
    /// Request are answered as the Endpoint it describes (M2-104 §7.1), one
    /// bidirectional Function Block over its Groups, UMP 1.1, the MIDI 1.0 and
    /// MIDI 2.0 Protocols, JR Timestamps sent (the output stamps for a device
    /// that asks) and, when the session was opened with
    /// [`InputTiming::JitterReduction`], received. `None` presents nothing.
    ///
    /// A session built by `tutti-session` has this set on every update from its
    /// `MidiPolicy`, which overwrites a call made here.
    ///
    /// An identity with no Product Instance Id is declared with one the session
    /// makes the first time it is presented so and keeps
    /// ([`instance_id`](Self::instance_id)), so the Id (M2-104 §7.1.5) is the
    /// same through every change of name or Groups; one the identity carries is
    /// declared as it is.
    ///
    /// A change of identity ends the devices that ran (each sends Invalidate
    /// MUID for its own MUID, its peers are lost) and the new identity starts
    /// them again under fresh MUIDs, all in the next poll; a change of name or
    /// Product Instance Id alone re-announces the Endpoint.
    pub fn present(&mut self, who: Option<Presented>) {
        if self.generated_instance.is_none()
            && who.is_some_and(|who| who.identity.instance.is_none())
        {
            self.generated_instance = {
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes)
                    .ok()
                    .map(|()| ProductInstanceId::from_random(bytes))
            };
        }
        self.presented = who;
    }

    /// The Product Instance Id in force: the presented identity's own when it
    /// has one, else the one the session made for it
    /// ([`present`](Self::present)), else `None` (before an identity without
    /// one was presented, or when the operating system had no random bytes).
    /// An app keeps it (it is serde with the `serde` feature) and puts it in
    /// the [`MidiIdentity`] it presents next time, so the Id re-identifies the
    /// device after a power cycle.
    #[must_use]
    pub fn instance_id(&self) -> Option<ProductInstanceId> {
        self.presented
            .and_then(|who| who.identity.instance)
            .or(self.generated_instance)
    }

    /// Who the session presents itself as ([`present`](Self::present)).
    #[must_use]
    pub const fn presented(&self) -> Option<Presented> {
        self.presented
    }

    /// What the session was opened with.
    #[must_use]
    pub fn options(&self) -> MidiHardwareOptions {
        self.options
    }

    /// The MIDI-CI peers found on the open input `id`, by Group then MUID:
    /// empty when it is not open or nothing was found (the first
    /// [`poll`](Self::poll) after its negotiation settled starts Discovery,
    /// and a peer is found when its Reply arrives). The list is shared, not
    /// copied: it is rebuilt when a [`poll`](Self::poll) reports a change, and a
    /// read clones the pointer, so holding one keeps the peers as they were.
    #[must_use]
    pub fn ci_peers(&self, id: &DeviceId) -> Arc<[CiPeerInfo]> {
        self.inputs
            .get(id)
            .map_or_else(Arc::default, |held| held.stream.ci_peers())
    }

    /// Makes the MIDI-CI devices' Property Exchange Responders serve `store`
    /// (M2-103): on every open input and every input opened later, at the next
    /// [`poll`](Self::poll). It replaces what was served. The session serves an
    /// empty store until this is called.
    ///
    /// Every device on every input serves this one store, so a peer's Set
    /// through one Group is seen by the others and by the app
    /// ([`served_properties`](Self::served_properties)), and is reported as
    /// [`PropertyChange::Served`].
    /// The session has no way to tell a subscriber of a change the app makes,
    /// so every entry is served as not subscribable: the ResourceList says so
    /// and a Subscription Start is refused with the status for a Resource that
    /// cannot be subscribed to (M2-103 §11.1). Publishing to subscribers is not
    /// offered.
    pub fn serve_properties(&mut self, store: ResourceStore) {
        self.properties = store.without_subscriptions();
    }

    /// The store the devices serve: what [`serve_properties`](Self::serve_properties)
    /// gave, as peers' Sets have changed it. Only
    /// [`poll`](Self::poll) changes it between calls.
    #[must_use]
    pub const fn served_properties(&self) -> &ResourceStore {
        &self.properties
    }

    /// The MIDI-CI peers of the open input `id` that declared Property Exchange
    /// and answered its Capabilities (M2-103 §3.2), by Group then MUID: empty
    /// when it is not open or no peer did. The session asks each such peer for
    /// its Capabilities when it is found. Shared like
    /// [`ci_peers`](Self::ci_peers): read again after a [`Polled::Property`].
    #[must_use]
    pub fn property_peers(&self, id: &DeviceId) -> Arc<[PropertyPeer]> {
        self.inputs
            .get(id)
            .map_or_else(Arc::default, |held| held.stream.property_peers())
    }

    /// Asks a peer for a Property Exchange call, as the Initiator (M2-103): the
    /// request is made at the next [`poll`](Self::poll), and an [`Answer::Got`] or
    /// [`Answer::Set`] ends it. Make the [`PropertyAsk`]
    /// from a handle in [`property_peers`](Self::property_peers).
    ///
    /// # Errors
    ///
    /// [`PeerError::NotOpen`] when that input has since closed, and
    /// [`PeerError::NoSuchPeer`] when the peer is gone.
    pub fn ask_property(&mut self, ask: PropertyAsk) -> std::result::Result<(), PeerError> {
        self.input_of(ask.peer.at())?.stream.ask_property(ask)
    }

    /// The Profiles of the MIDI-CI peers found on the open input `id` that
    /// declared Profile Configuration, by Group then MUID (M2-101 §7): empty when
    /// it is not open or no peer did. The session asks each such peer for its
    /// Profiles when it is found, and keeps them as the peer reports. Shared like
    /// [`ci_peers`](Self::ci_peers): read again after a
    /// [`Polled::Profile`].
    #[must_use]
    pub fn profiles(&self, id: &DeviceId) -> Arc<[PeerProfiles]> {
        self.inputs
            .get(id)
            .map_or_else(Arc::default, |held| held.stream.profiles())
    }

    /// Asks a peer to enable or disable a Profile it lists, on the input the
    /// [`ProfileSwitch`] was made from, as the Initiator (M2-101 §§7.8, 7.9): the request is sent at the
    /// next [`poll`](Self::poll), and its end is an [`Answer::Switch`] (the
    /// peer's Enabled or Disabled Report, its NAK, or [`PEER_REPLY_TIMEOUT`]
    /// with no answer). Make the [`ProfileSwitch`] from a handle in
    /// [`profiles`](Self::profiles).
    ///
    /// # Errors
    ///
    /// [`ProfileError::Peer`] with [`PeerError::NotOpen`] when that input has
    /// since closed, with [`PeerError::NoSuchPeer`] when the peer is gone, and
    /// [`ProfileError::NotListed`] when it no longer lists the Profile there.
    ///
    /// [`PEER_REPLY_TIMEOUT`]: tutti_midi_ci::PEER_REPLY_TIMEOUT
    pub fn set_profile(&mut self, switch: ProfileSwitch) -> std::result::Result<(), ProfileError> {
        self.input_of(switch.at())?.stream.set_profile(switch)
    }

    /// The MIDI-CI peers of the open input `id` that declared Process Inquiry and
    /// answered that they support MIDI Message Report (M2-101 §9), by Group then
    /// MUID: empty when it is not open or no peer did. The session asks each such
    /// peer for its capabilities when it is found. Shared like
    /// [`ci_peers`](Self::ci_peers): read again after a
    /// [`Polled::Process`] that says `Capabilities`.
    #[must_use]
    pub fn report_peers(&self, id: &DeviceId) -> Arc<[Reporter]> {
        self.inputs
            .get(id)
            .map_or_else(Arc::default, |held| held.stream.report_peers())
    }

    /// Asks a peer for a MIDI Message Report, as the Initiator (M2-101 §9.5): the
    /// request is sent at the next [`poll`](Self::poll), and its end is an
    /// [`Answer::Report`]: the peer's End, its NAK, or the wait running out. The
    /// MIDI messages of the report arrive on the input before it, as MIDI.
    /// Make the [`ReportAsk`] from a handle in [`report_peers`](Self::report_peers).
    ///
    /// # Errors
    ///
    /// [`PeerError::NotOpen`] when that input has since closed, and
    /// [`PeerError::NoSuchPeer`] when the peer is gone.
    pub fn report(&mut self, ask: ReportAsk) -> std::result::Result<(), PeerError> {
        self.input_of(ask.peer.at())?.stream.report(ask)
    }

    /// The open input the peer `at` was found on.
    fn input_of(&mut self, at: &PeerRef) -> std::result::Result<&mut Held, PeerError> {
        self.inputs.get_mut(at.device()).ok_or(PeerError::NotOpen)
    }

    /// What is known of the UMP Stream state of the open input `id`: `None`
    /// when it is not open, or its negotiation has not settled yet (the first
    /// [`poll`](Self::poll) starts it). Its
    /// [`config`](DeviceStream::config) is the Protocol and JR Timestamps in
    /// use.
    #[must_use]
    pub fn stream(&self, id: &DeviceId) -> Option<DeviceStream> {
        self.inputs.get(id)?.stream.settled()
    }

    /// Returns what the open input `id` has said about itself so far: its Device
    /// Identity and its Product Instance Id, empty when it is not open or has
    /// said nothing yet (the answers arrive over the first seconds, and
    /// MIDI-CI's Endpoint Inquiry later: read again after the
    /// [`Polled`] that carries them).
    ///
    /// The UMP Endpoint, or a MIDI 1.0 port's Identity Reply, speaks for the
    /// whole device and goes first; a MIDI-CI peer (by Group then MUID) fills
    /// what it left. The OS's description carries neither, so a device that never
    /// answered says nothing. Compare it with a saved device's
    /// through [`Learned::compare`](tutti_midi_device::Learned::compare).
    #[must_use]
    pub fn learned(&self, id: &DeviceId) -> Learned {
        let Some(held) = self.inputs.get(id) else {
            return Learned::new();
        };
        let mut learned = held
            .stream
            .settled()
            .map_or_else(Learned::new, |stream| stream.said());
        for peer in held.stream.ci_peers().iter() {
            let peer = peer.peer();
            let mut said = Learned::new().identified(peer.discovery().identity);
            if let Some(instance) = peer.product_instance_id() {
                said = said.instanced(*instance);
            }
            learned = learned.or(said);
        }
        learned
    }

    /// Returns the open input `id` as an app saves it: the listing's row and what
    /// [`learned`](Self::learned) says now, `None` when it is not open or no
    /// longer listed. Save it once the device has spoken; restore it with
    /// [`Devices::resolve`](crate::Devices::resolve) on
    /// [`SavedDevice::info`] and check it with [`SavedDevice::confirm`].
    #[must_use]
    pub fn saved(&self, id: &DeviceId) -> Option<SavedDevice> {
        let info = self.devices.list().into_iter().find(|d| d.id == *id)?;
        self.is_input_connected(id)
            .then(|| SavedDevice::new(info, self.learned(id)))
    }
}

/// Points the port's event times at what the device's configuration says: its
/// JR Timestamps where it sends them and the input was opened to use them.
/// Arrival until the device has settled.
fn follow_timing(ports: &HardwareMidiInputs, held: &Held) {
    let source = held
        .stream
        .settled()
        .map_or(InputTiming::Arrival, |stream| {
            held.timing.applied(stream.config())
        });
    ports.set_input_timing(held.input.port(), source);
}

/// Makes the port's copy of SysEx7 packets what the negotiation wants.
fn sync_hearing(ports: &HardwareMidiInputs, held: &mut Held, ci: bool) {
    let slot = held.input.port();
    let want = held.stream.hears_sysex(ci);
    if want && !held.hearing {
        held.hearing = ports.hear_sysex(slot, true);
    } else if !want && held.hearing {
        ports.hear_sysex(slot, false);
        held.hearing = false;
    }
}

#[cfg(test)]
mod tests;

/// The refusals the `changes` make news of, as the changes that say so: the
/// zone an Enabled Report on a Manager Channel named, if its zones leave it
/// out, and, once a peer's inquiry is whole, every zone its table already
/// enabled that they leave out. A refusal is said once, with the change that
/// enabled the zone.
fn mpe_zone_refusals(
    changes: &[(PeerId, ProfilesChange)],
    peers: &[PeerProfiles],
) -> Vec<(PeerId, ProfilesChange)> {
    let mut said = Vec::new();
    for (peer, change) in changes {
        let named = match change {
            ProfilesChange::Known => None,
            ProfilesChange::Changed {
                address: CiDeviceId::Channel(manager),
                profile: ProfileId::MPE,
                change: ProfileChange::Enabled,
            } => Some(*manager),
            _ => continue,
        };
        let Some(held) = peers.iter().find(|p| p.at().peer() == *peer) else {
            continue;
        };
        for overlap in held.refused_mpe_zones() {
            if named.is_none_or(|manager| overlap.added.manager() == manager) {
                said.push((*peer, ProfilesChange::MpeZoneRefused(overlap)));
            }
        }
    }
    said
}
