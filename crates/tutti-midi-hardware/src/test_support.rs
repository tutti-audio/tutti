//! A MIDI backend with no OS behind it, for testing the session layer.
//!
//! [`MidiSession::open_over`](crate::MidiSession::open_over) is the seam
//! this exists for: the session owns no driver code, so a supplied
//! [`MidiEndpoints`] drives every connect / disconnect / send path
//! deterministically, on any platform, including one with no MIDI hardware at
//! all. Without it a test of that layer can assert only "does not panic".
//!
//! # Why it is exported rather than `#[cfg(test)]`
//!
//! It began as a private fixture in `session.rs`, which covered this crate's own
//! tests and nothing above it. A headless CI box has no MIDI port, so a consumer's
//! tests of its session wiring were either absent or asserted against "no device
//! found". The fixture is the *only* thing standing between those tests and a
//! real device, and it is small, so exporting it is cheaper than each consumer
//! writing its own subtly different one.
//!
//! Behind the `testing` feature, off by default: a host has no use for a
//! fake device list, and shipping one in a release build invites it into
//! production wiring by accident.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use tutti_rt::LockExt;

use crate::capability::{OsEndpoint, from_os};
use crate::devices::ChangeSink;
use crate::devices::reporter::os_devices;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{MidiHardwareError, MidiThread, NotFound, Result};
use crate::start::Started;
use crate::{InputFault, InputProducerHandle};
use tutti_midi_device::{EndpointId, Identity, OsChange, PortDirection};
use tutti_midi_runtime::{MidiOut, Taken};
use tutti_rt::PushError;
use tutti_ump::Protocol;

/// A backend with a fixed device list and no OS behind it.
///
/// Build one with [`FakeBackend::build`] for the default two-in / two-out list,
/// or [`build_with`](FakeBackend::build_with) to name your own. The counters it
/// returns are what make its behaviour observable: `sent` counts events handed
/// to its sink, `opens` counts input ports actually opened — the latter is how
/// "reconnecting does not reopen" is asserted at all, since a second driver
/// connection to one device duplicates every inbound event and is invisible from
/// the outside.
#[derive(Debug)]
pub struct FakeBackend {
    devices: Arc<Mutex<FakeDevices>>,
    sent: Arc<AtomicUsize>,
    opens: Arc<AtomicUsize>,
    watchers: Arc<AtomicUsize>,
    outputs: OutputConnections,
    /// The producer of each open input: the fake device's side of the wire.
    producers: Producers,
    /// What the sinks were handed, for [`FakeCounters::take_sent`].
    log: SentLog,
    /// How many events per batch the sink it hands out will accept.
    accepts: usize,
    /// Whether the sinks it handed out refuse everything for now.
    refusing: Arc<AtomicBool>,
}

/// The producer handle of each input connection that is open, by device.
type Producers = Arc<Mutex<Vec<(EndpointId, InputProducerHandle)>>>;

/// The packets handed to the fake sinks, by the device they were sent to.
type SentLog = Arc<Mutex<Vec<(EndpointId, tutti_ump::Ump, tutti_midi_runtime::SendAt)>>>;

/// The fake OS's device lists, and the channel into each running watch.
#[derive(Debug, Default)]
struct FakeDevices {
    inputs: Vec<OsEndpoint>,
    outputs: Vec<OsEndpoint>,
    /// Whether opening an output fails, as one held by another app does.
    refuse_outputs: bool,
    /// Whether opening an input fails.
    refuse_inputs: bool,
    /// The inputs whose reader has ended on an error: open, and silent.
    dead_inputs: Vec<(EndpointId, InputFault)>,
    watches: Vec<(u64, Sender<Delivery>)>,
    next_watch: u64,
}

/// One change on its way to a watch thread, and the signal that the thread
/// has applied it.
type Delivery = (Report, Sender<()>);

/// What the fake OS tells a watch thread.
#[derive(Clone)]
enum Report {
    Change(OsChange),
    Fail,
}

impl FakeDevices {
    /// Tells every watch what the fake OS now says about `id`, and returns once
    /// each watch thread has reported it to its sink.
    fn announce(&self, id: EndpointId) {
        let change = Report::Change(
            os_devices(&self.inputs, &self.outputs)
                .into_iter()
                .find(|d| d.handle == id)
                .map_or(OsChange::Gone(id), OsChange::Present),
        );
        self.deliver(&change);
    }

    /// Hands `report` to every watch thread and waits for each to take it.
    fn deliver(&self, report: &Report) {
        let acks: Vec<_> = self
            .watches
            .iter()
            .map(|(_, watch)| {
                let (done, ack) = mpsc::channel();
                // A watch whose thread has ended has nothing to tell.
                let _ = watch.send((report.clone(), done));
                ack
            })
            .collect();
        for ack in acks {
            let _ = ack.recv();
        }
    }
}

/// The counters a [`FakeBackend`] bumps, handed back at construction.
///
/// A struct rather than a tuple: the two are both `Arc<AtomicUsize>` and reading
/// the wrong one is a test that passes for the wrong reason.
#[derive(Clone, Debug)]
pub struct FakeCounters {
    /// Events handed to the fake sink, across every batch.
    pub sent: Arc<AtomicUsize>,
    /// Input ports opened. Counts *opens*, not connections, which is what makes
    /// a duplicate open detectable.
    pub opens: Arc<AtomicUsize>,
    /// Watch threads running. Zero once every `Devices` over this backend has
    /// dropped: the leak check.
    pub watchers: Arc<AtomicUsize>,
    outputs: OutputConnections,
    devices: Arc<Mutex<FakeDevices>>,
    producers: Producers,
    log: SentLog,
    refusing: Arc<AtomicBool>,
}

impl FakeCounters {
    /// Makes every sink the fake handed out refuse everything (or take what its
    /// `accepts` allows again): a device whose output filled up after it was
    /// opened.
    pub fn refuse_sends(&self, refuse: bool) {
        self.refusing.store(refuse, Ordering::SeqCst);
    }

    /// Takes the packets sent to the output of device `id` since the last call,
    /// oldest first: what the fake device received on the wire.
    pub fn take_sent(&self, id: EndpointId) -> Vec<tutti_ump::Ump> {
        let mut log = self.log.lock_recover();
        let (mine, rest) = log.drain(..).partition(|(device, ..)| *device == id);
        *log = rest;
        mine.into_iter().map(|(_, ump, _)| ump).collect()
    }

    /// [`take_sent`](Self::take_sent), each packet with the time the sink was
    /// told to send it at.
    pub fn take_sent_timed(
        &self,
        id: EndpointId,
    ) -> Vec<(tutti_ump::Ump, tutti_midi_runtime::SendAt)> {
        let mut log = self.log.lock_recover();
        let (mine, rest) = log.drain(..).partition(|(device, ..)| *device == id);
        *log = rest;
        mine.into_iter().map(|(_, ump, at)| (ump, at)).collect()
    }

    /// The devices whose input connection is open now, oldest first: a
    /// connection closes when its port is closed or its session ends.
    #[must_use]
    pub fn connected_inputs(&self) -> Vec<EndpointId> {
        self.producers
            .lock_recover()
            .iter()
            .map(|(device, _)| *device)
            .collect()
    }

    /// The devices whose output connection is open now, oldest first: a
    /// connection closes when its output is dropped.
    #[must_use]
    pub fn connected_outputs(&self) -> Vec<EndpointId> {
        self.outputs.lock_recover().live.clone()
    }

    /// The most output connections to one device that were open at once.
    #[must_use]
    pub fn most_output_connections(&self) -> usize {
        self.outputs.lock_recover().most
    }

    /// Events handed to the sink so far.
    #[must_use]
    pub fn sent(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }

    /// The fake device `id` sends `ump`, which the OS delivered at `arrival`:
    /// pushed through the producer of its open input connection, as a driver's
    /// thread would. `false` when no connection to `id` is open, or the
    /// port's inlet is full ([`PushError::Full`]); an event kept out of the graph
    /// on purpose ([`Delivered::Diverted`](crate::Delivered)) was taken.
    pub fn send_input(
        &self,
        id: EndpointId,
        ump: tutti_ump::Ump,
        arrival: tutti_platform::Instant,
    ) -> bool {
        let mut producers = self.producers.lock_recover();
        producers
            .iter_mut()
            .find(|(device, _)| *device == id)
            .is_some_and(|(_, producer)| producer.push(ump, arrival) != Err(PushError::Full))
    }

    /// Input ports opened so far.
    #[must_use]
    pub fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }

    /// Watch threads running now.
    #[must_use]
    pub fn watchers(&self) -> usize {
        self.watchers.load(Ordering::SeqCst)
    }

    fn devices(&self) -> MutexGuard<'_, FakeDevices> {
        self.devices.lock_recover()
    }

    /// Plugs `endpoint` in, as `direction` says, and tells every running
    /// watch. A device already plugged in under that id is replaced, which a
    /// watch sees as a change. The watch thread delivers it, and this returns
    /// once it has: the event is waiting in every subscriber's queue.
    pub fn plug(&self, endpoint: OsEndpoint, direction: PortDirection) {
        let mut devices = self.devices();
        let id = endpoint.id;
        devices.inputs.retain(|e| e.id != id);
        devices.outputs.retain(|e| e.id != id);
        if direction.is_input() {
            devices.inputs.push(endpoint.clone());
        }
        if direction.is_output() {
            devices.outputs.push(endpoint);
        }
        devices.announce(id);
    }

    /// Makes every running watch fail as if its descriptor broke, as a real one does when
    /// the OS call it blocks in fails: the list freezes and
    /// every subscriber's `DeviceEvents::failure` says why.
    pub fn fail_watch(&self) {
        self.devices().deliver(&Report::Fail);
    }

    /// Makes every later output open fail (or succeed again), as an output
    /// another app holds does; the listed devices stay as they are.
    pub fn refuse_output_opens(&self, refuse: bool) {
        self.devices().refuse_outputs = refuse;
    }

    /// Makes every later input open fail (or succeed again); the listed devices
    /// stay as they are.
    pub fn refuse_input_opens(&self, refuse: bool) {
        self.devices().refuse_inputs = refuse;
    }

    /// Ends the reader of the open input of device `id` on an error, as a
    /// backend's does: the connection is still held and the device still listed,
    /// and [`InputConnection::is_alive`] reads `false` and
    /// [`InputConnection::fault`] reads `fault` until it is opened again.
    pub fn kill_input_reader(&self, id: EndpointId, fault: InputFault) {
        self.devices().dead_inputs.push((id, fault));
    }

    /// Unplugs the device with `id` from both lists and tells every running
    /// watch, returning once each has reported it. A held connection to it stays open, as an OS leaves one.
    pub fn unplug(&self, id: EndpointId) {
        let mut devices = self.devices();
        devices.inputs.retain(|e| e.id != id);
        devices.outputs.retain(|e| e.id != id);
        devices.announce(id);
    }
}

/// The fake's watch: a thread that forwards the changes `plug` and `unplug`
/// send, as a real backend's thread forwards the OS's.
struct FakeWatch {
    devices: Arc<Mutex<FakeDevices>>,
    key: u64,
    thread: Option<JoinHandle<()>>,
}

impl Watch for FakeWatch {}

impl Drop for FakeWatch {
    fn drop(&mut self) {
        // The thread ends when its channel's last sender is gone.
        self.devices
            .lock_recover()
            .watches
            .retain(|(key, _)| *key != self.key);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Decrements the live-watch count when the thread ends, however it ends.
struct Running(Arc<AtomicUsize>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// An open input: its producer is the fake device's, and goes with it.
struct FakeConn {
    producers: Producers,
    devices: Arc<Mutex<FakeDevices>>,
    id: EndpointId,
}

impl InputConnection for FakeConn {
    fn fault(&self) -> Option<InputFault> {
        let devices = self.devices.lock_recover();
        devices
            .dead_inputs
            .iter()
            .find(|(id, _)| *id == self.id)
            .map(|&(_, fault)| fault)
    }
}

impl Drop for FakeConn {
    fn drop(&mut self) {
        self.producers
            .lock_recover()
            .retain(|(device, _)| *device != self.id);
    }
}

/// The open output connections, and the most one device held at once.
#[derive(Debug, Default)]
struct OutputLedger {
    live: Vec<EndpointId>,
    most: usize,
}

type OutputConnections = Arc<Mutex<OutputLedger>>;

/// A sink that counts what it was handed and accepts `accepts` of each batch.
///
/// `accepts` is what lets a test express a device that refuses. A
/// `MidiOut::queue` returning `()` would make that shape unrepresentable, and
/// therefore make `send`'s accepted count untestable.
struct FakeSink {
    sent: Arc<AtomicUsize>,
    accepts: usize,
    refusing: Arc<AtomicBool>,
    id: EndpointId,
    log: SentLog,
    connections: OutputConnections,
}

impl Drop for FakeSink {
    fn drop(&mut self) {
        let mut ledger = self.connections.lock_recover();
        if let Some(at) = ledger.live.iter().position(|id| *id == self.id) {
            ledger.live.remove(at);
        }
    }
}

impl DeviceOut for FakeSink {}

impl MidiOut for FakeSink {
    fn queue_timed(&self, events: &[tutti_midi_runtime::Outgoing]) -> Taken {
        self.sent.fetch_add(events.len(), Ordering::SeqCst);
        let accepts = if self.refusing.load(Ordering::SeqCst) {
            0
        } else {
            self.accepts
        };
        let taken = Taken::within(events, accepts);
        self.log.lock_recover().extend(
            events
                .iter()
                .take(taken.packets())
                .map(|e| (self.id, e.ump, e.send)),
        );
        taken
    }
}

/// An [`OsEndpoint`] with a raw id and a name, MIDI 2.0 capable.
#[must_use]
pub fn endpoint(raw: u64, name: &str) -> OsEndpoint {
    OsEndpoint {
        id: EndpointId::from_raw(raw),
        identity: Identity::alsa(name, "port"),
        name: name.to_string(),
        capability: from_os(Protocol::Midi2, None, []),
    }
}

/// Like [`endpoint`], but known by a CoreMIDI unique id (the raw id), the way
/// a device is told for certain across a replug.
#[must_use]
pub fn persistent_endpoint(raw: u32, name: &str) -> OsEndpoint {
    OsEndpoint {
        identity: Identity::CoreMidi(raw.cast_signed()),
        ..endpoint(u64::from(raw), name)
    }
}

impl FakeBackend {
    /// The default backend: inputs `Keystep Pro` (id 1) and `IAC Bus 1` (id 2),
    /// outputs `Synth A` (id 10) and `IAC Bus 1` (id 11). Its sink accepts
    /// everything.
    ///
    /// Two names collide across the two directions deliberately — a session that
    /// keyed its connection map by name rather than by [`EndpointId`] would pass
    /// every test against a list where they did not.
    ///
    /// `build` rather than `new`: it returns the backend *and* its counters, and
    /// a `new` that does not return `Self` reads as one that does.
    #[must_use]
    pub fn build() -> (Box<dyn MidiEndpoints>, FakeCounters) {
        Self::build_with(
            vec![endpoint(1, "Keystep Pro"), endpoint(2, "IAC Bus 1")],
            vec![endpoint(10, "Synth A"), endpoint(11, "IAC Bus 1")],
            usize::MAX,
        )
    }

    /// As [`build`](Self::build), but the sink accepts at most `accepts` events
    /// per batch — `0` models a device refusing everything.
    #[must_use]
    pub fn build_accepting(accepts: usize) -> (Box<dyn MidiEndpoints>, FakeCounters) {
        Self::build_with(
            vec![endpoint(1, "Keystep Pro"), endpoint(2, "IAC Bus 1")],
            vec![endpoint(10, "Synth A"), endpoint(11, "IAC Bus 1")],
            accepts,
        )
    }

    /// A backend over caller-supplied device lists.
    ///
    /// For the cases the defaults cannot express — an empty list (no hardware),
    /// or a list whose ids a test wants to control.
    #[must_use]
    pub fn build_with(
        inputs: Vec<OsEndpoint>,
        outputs: Vec<OsEndpoint>,
        accepts: usize,
    ) -> (Box<dyn MidiEndpoints>, FakeCounters) {
        let producers = Producers::default();
        let log = SentLog::default();
        let refusing = Arc::new(AtomicBool::new(false));
        let counters = FakeCounters {
            refusing: Arc::clone(&refusing),
            producers: Arc::clone(&producers),
            log: Arc::clone(&log),
            sent: Arc::new(AtomicUsize::new(0)),
            opens: Arc::new(AtomicUsize::new(0)),
            watchers: Arc::new(AtomicUsize::new(0)),
            outputs: OutputConnections::default(),
            devices: Arc::new(Mutex::new(FakeDevices {
                inputs,
                outputs,
                ..FakeDevices::default()
            })),
        };
        let backend = FakeBackend {
            devices: Arc::clone(&counters.devices),
            sent: Arc::clone(&counters.sent),
            opens: Arc::clone(&counters.opens),
            watchers: Arc::clone(&counters.watchers),
            outputs: Arc::clone(&counters.outputs),
            producers,
            log,
            accepts,
            refusing,
        };
        (Box::new(backend), counters)
    }
}

impl FakeBackend {
    fn devices(&self) -> MutexGuard<'_, FakeDevices> {
        self.devices.lock_recover()
    }
}

impl MidiEndpoints for FakeBackend {
    #[expect(
        clippy::disallowed_methods,
        reason = "test support: the fake backend's watch thread stands in for an OS one"
    )]
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        let (changes, incoming) = mpsc::channel();
        let mut devices = self.devices();
        // What is plugged in now is reported before this returns.
        sink.seed(os_devices(&devices.inputs, &devices.outputs))?;
        let key = devices.next_watch;
        devices.next_watch += 1;
        devices.watches.push((key, changes));
        self.watchers.fetch_add(1, Ordering::SeqCst);
        let running = Running(Arc::clone(&self.watchers));
        let thread = std::thread::Builder::new()
            .name(MidiThread::DeviceWatch.name().to_string())
            .spawn(move || {
                let _running = running;
                for (report, done) in incoming {
                    match report {
                        Report::Change(change) => sink.send(change),
                        Report::Fail => {
                            // Like a real watch that has died: nothing more.
                            sink.fail(MidiHardwareError::DescriptorBroken);
                            let _ = done.send(());
                            return;
                        }
                    }
                    let _ = done.send(());
                }
            })
            .map_err(|source| MidiHardwareError::Spawn {
                thread: MidiThread::DeviceWatch,
                source,
            })?;
        Ok(Box::new(FakeWatch {
            devices: Arc::clone(&self.devices),
            key,
            thread: Some(thread),
        }))
    }

    fn inputs(&self) -> Vec<OsEndpoint> {
        self.devices().inputs.clone()
    }

    fn outputs(&self) -> Vec<OsEndpoint> {
        self.devices().outputs.clone()
    }

    fn open_input(
        &self,
        _started: Started,
        id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        {
            let mut devices = self.devices();
            if devices.refuse_inputs {
                return Err(MidiHardwareError::InUse(id));
            }
            if !devices.inputs.iter().any(|e| e.id == id) {
                return Err(NotFound::Endpoint(id).into());
            }
            // A new connection has a reader of its own.
            devices.dead_inputs.retain(|(dead, _)| *dead != id);
        }
        self.opens.fetch_add(1, Ordering::SeqCst);
        self.producers.lock_recover().push((id, producer));
        Ok(Box::new(FakeConn {
            producers: Arc::clone(&self.producers),
            devices: Arc::clone(&self.devices),
            id,
        }))
    }

    fn open_output(&self, _started: Started, id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        let devices = self.devices();
        if devices.refuse_outputs {
            return Err(MidiHardwareError::InUse(id));
        }
        if !devices.outputs.iter().any(|e| e.id == id) {
            return Err(NotFound::Endpoint(id).into());
        }
        drop(devices);
        {
            let mut ledger = self.outputs.lock_recover();
            ledger.live.push(id);
            let held = ledger.live.iter().filter(|open| **open == id).count();
            ledger.most = ledger.most.max(held);
        }
        Ok(Box::new(FakeSink {
            sent: Arc::clone(&self.sent),
            accepts: self.accepts,
            refusing: Arc::clone(&self.refusing),
            id,
            log: Arc::clone(&self.log),
            connections: Arc::clone(&self.outputs),
        }))
    }
}

/// A peer of Property Exchange as the session lists it, for a test that holds no
/// session of its own.
#[must_use]
pub fn property_peer(
    device: tutti_midi_device::DeviceId,
    group: tutti_ump::MidiGroup,
    muid: tutti_midi_ci::Muid,
    resources: tutti_midi_ci::PeResourceList,
) -> crate::PropertyPeer {
    crate::PropertyPeer {
        at: crate::PeerRef::new(device, crate::PeerId { group, muid }),
        resources: Arc::new(resources),
    }
}
