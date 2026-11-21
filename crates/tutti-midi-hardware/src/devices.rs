//! [`Devices`]: the MIDI devices present, kept current while it lives.
//!
//! A backend watches the OS on a thread of its own and reports each change to a
//! [`ChangeSink`]. The sink feeds the pure [`DeviceTable`] and hands the event
//! it returns to every [`DeviceEvents`] subscribed, so `list` reads one table
//! and no read is an OS call. The watch stops, and the backend's thread with
//! it, when the last [`Devices`] and the last [`DeviceEvents`] are dropped.
//!
//! A subscriber's queue is unbounded, and exists only while its
//! [`DeviceEvents`] does: an app that wants no events subscribes to none.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use tutti_graph::{Losses, UmpAt};
use tutti_platform::Executor;
use tutti_rt::LockExt;

use tutti_graph::EventInletNode;
use tutti_midi_device::{
    DeviceEvent, DeviceInfo, DeviceTable, Endpoint, EndpointId, MidiDeviceId, OsChange, OsDevice,
    PortDirection, Resolved,
};
use tutti_midi_runtime::{DeviceWriters, InputId, Outgoing, Taken};
use tutti_ump::StreamConfig;

use crate::backend;
use crate::capability::{OsEndpoint, described, legacy};
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{MidiHardwareError, NotFound, Result};
use crate::sensing::Heartbeat;
use crate::start::Started;
use crate::wire::Wire;
use crate::{HardwareMidiInputs, InputFault};

// Only a native backend reports through it; the stub has none.
#[cfg_attr(
    midi_backend = "none",
    allow(
        dead_code,
        reason = "only the OS backends with a thread report through it"
    )
)]
pub(crate) mod reporter;

/// Why a device watch ended on its own.
#[derive(Clone, Debug, thiserror::Error)]
pub enum WatchFailure {
    /// The backend's watch thread panicked.
    #[error("the MIDI device watch thread panicked")]
    Panicked,
    /// The OS refused the watch, or it broke: the error is the OS's own, and
    /// its text is shown as it is.
    #[error(transparent)]
    Failed(Arc<MidiHardwareError>),
}

impl From<MidiHardwareError> for WatchFailure {
    fn from(error: MidiHardwareError) -> Self {
        Self::Failed(Arc::new(error))
    }
}

#[derive(Default)]
struct State {
    table: DeviceTable,
    subscribers: Vec<Weak<Queue>>,
    /// Why the watch ended, if it did. The table is then the last it saw.
    failure: Option<WatchFailure>,
    /// Set by the first [`ChangeSink::seed`] and by any [`ChangeSink::send_all`]:
    /// the table has begun, and a seed after that would rewrite what a
    /// subscriber may already have read.
    begun: bool,
}

/// [`ChangeSink::seed`] refused: the list was already seeded, or a change had
/// already been reported. A backend seeds once, first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the device list was already seeded, or a change was already reported")]
pub struct SeedError;

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
}

/// One subscriber's inbox.
#[derive(Default)]
struct Queue {
    inbox: Mutex<Inbox>,
}

#[derive(Default)]
struct Inbox {
    events: VecDeque<DeviceEvent>,
    ended: Option<WatchFailure>,
    /// The futures waiting on this inbox, each woken by the next push or end,
    /// under the id its [`Recv`] holds.
    wakers: Vec<(u64, Waker)>,
    next_id: u64,
}

impl Inbox {
    fn take_wakers(&mut self) -> Vec<Waker> {
        std::mem::take(&mut self.wakers)
            .into_iter()
            .map(|(_, waker)| waker)
            .collect()
    }

    /// Removes the waker registered under `id`; the caller drops it once it
    /// holds no lock, since a waker's drop can run its executor's code.
    #[must_use = "drop it after releasing the inbox lock"]
    fn forget(&mut self, id: u64) -> Option<Waker> {
        let at = self.wakers.iter().position(|(held, _)| *held == id)?;
        Some(self.wakers.swap_remove(at).1)
    }
}

impl Queue {
    fn inbox(&self) -> MutexGuard<'_, Inbox> {
        self.inbox.lock_recover()
    }

    /// Queues `event`; the wakers it returns are woken by the caller once it
    /// holds no lock.
    #[must_use = "wake them after releasing every lock"]
    fn push(&self, event: DeviceEvent) -> Vec<Waker> {
        let mut inbox = self.inbox();
        inbox.events.push_back(event);
        inbox.take_wakers()
    }

    /// Ends the stream; the wakers are woken as for [`push`](Self::push).
    #[must_use = "wake them after releasing every lock"]
    fn end(&self, failure: WatchFailure) -> Vec<Waker> {
        let mut inbox = self.inbox();
        inbox.ended.get_or_insert(failure);
        inbox.take_wakers()
    }
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock_recover()
    }
}

/// Where a backend's watch reports what the OS says. Cloneable and `Send`: the
/// backend moves one onto its thread.
#[derive(Clone)]
pub struct ChangeSink {
    shared: Arc<Shared>,
}

impl ChangeSink {
    /// Reports one change. A change that alters nothing is dropped; one that
    /// does reaches every subscriber and wakes a waiting
    /// [`DeviceEvents::recv`].
    pub fn send(&self, change: OsChange) {
        self.send_all([change]);
    }

    /// Reports `changes` in order under one hold of the state, so no
    /// [`Devices::list`] or [`Devices::subscribe`] sees between them: a device
    /// reported gone and present again is never missing from the list.
    pub fn send_all(&self, changes: impl IntoIterator<Item = OsChange>) {
        let mut state = self.shared.state();
        state.begun = true;
        let mut wakers = Vec::new();
        for change in changes {
            if let Some(event) = state.table.apply(change) {
                // A subscriber that has dropped leaves the list.
                state.subscribers.retain(|queue| {
                    queue
                        .upgrade()
                        .inspect(|queue| wakers.extend(queue.push(event.clone())))
                        .is_some()
                });
            }
        }
        drop(state);
        wakers.into_iter().for_each(Waker::wake);
    }
}

impl ChangeSink {
    /// Records that the watch has ended without being dropped, with the reason.
    /// The list is frozen from here; every subscriber's
    /// [`recv`](DeviceEvents::recv) says so once it has taken
    /// what was queued, and so does the session's `Phase::Poll`.
    pub fn fail(&self, reason: impl Into<WatchFailure>) {
        let reason = reason.into();
        tracing::warn!("MIDI device watch ended: {}", crate::error::chain(&reason));
        let mut state = self.shared.state();
        let reason = state.failure.get_or_insert(reason).clone();
        let wakers: Vec<Waker> = state
            .subscribers
            .iter()
            .filter_map(Weak::upgrade)
            .flat_map(|queue| queue.end(reason.clone()))
            .collect();
        drop(state);
        wakers.into_iter().for_each(Waker::wake);
    }

    /// Lists `devices` as already present: they enter the table and queue no
    /// event, so a `Devices` that has just opened has nothing to poll. A backend
    /// seeds once, before any [`send`](Self::send).
    ///
    /// # Errors
    ///
    /// [`SeedError`] if the sink was already seeded or has had a change
    /// reported; the list is left as it was.
    pub fn seed(&self, devices: Vec<OsDevice>) -> std::result::Result<(), SeedError> {
        let mut state = self.shared.state();
        if state.begun {
            return Err(SeedError);
        }
        state.begun = true;
        for device in devices {
            state.table.apply(OsChange::Present(device));
        }
        Ok(())
    }
}

impl core::fmt::Debug for ChangeSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ChangeSink").finish_non_exhaustive()
    }
}

/// The devices present now, and a way to open one.
///
/// Cheap to clone: every clone is the same list over the same watch.
///
/// ```no_run
/// # async fn watch() -> Result<(), tutti_midi_hardware::MidiHardwareError> {
/// use tutti_midi_hardware::Devices;
///
/// let executor = tutti_platform::Executor::new();
/// let devices = Devices::open(&executor)?;
/// let events = devices.subscribe();
/// for device in events.initial() {
///     println!("{}: {}", device.id, device.name);
/// }
/// // `recv()` is a future the watch wakes; it errs once the watch has ended.
/// while let Ok(event) = events.recv().await {
///     println!("{event:?}");
/// }
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Devices {
    inner: Arc<Inner>,
}

struct Inner {
    // Declared first, so the watch stops before the backend it came from goes.
    _watch: Box<dyn Watch>,
    backend: Box<dyn MidiEndpoints>,
    shared: Arc<Shared>,
    /// What an output's heartbeat is spawned on.
    executor: Executor,
}

impl core::fmt::Debug for Devices {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Devices")
            .field("devices", &self.list().len())
            .finish_non_exhaustive()
    }
}

impl Devices {
    /// Lists this platform's devices and starts watching them, on a thread of
    /// `executor` where the backend has one; the watch stops when the last
    /// `Devices` and [`DeviceEvents`] over it are dropped.
    ///
    /// Dropping the last `Devices` stops the watch as [`Watch`] says, without
    /// waiting for it; a connection opened through the backend closes, and its
    /// port unsubscribes, as its own thread ends.
    ///
    /// Where the build has no MIDI backend (Windows, or alsa-lib older than
    /// 1.2.10) the list is empty and stays so, as enumeration is; opening a
    /// device reports [`MidiHardwareError::Unsupported`].
    ///
    /// # Errors
    ///
    /// The platform error from starting the watch, such as a thread that could
    /// not be spawned.
    pub fn open(executor: &Executor) -> Result<Self> {
        Self::open_over(executor, backend::active(executor))
    }

    /// As [`open`](Self::open), over a supplied backend: a test's
    /// `test_support::FakeBackend`. `executor` runs the outputs' Active Sensing.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn open_over(executor: &Executor, backend: Box<dyn MidiEndpoints>) -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let watch = backend.watch(ChangeSink {
            shared: Arc::clone(&shared),
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                _watch: watch,
                backend,
                shared,
                executor: executor.clone(),
            }),
        })
    }

    /// The wait for the backend to be ready to open endpoints and ports: resolves
    /// to the [`Started`] that opening takes, or to the error the backend failed to
    /// start with. On macOS the process's CoreMIDI client is made on a thread of the
    /// backend's own, and the device list is seeded when it resolves; a failed start
    /// ends no watch, and a later start that succeeds seeds it. Every other backend
    /// is ready on the first poll.
    pub fn start(&self) -> crate::Start {
        self.inner.backend.start()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner.shared.state()
    }

    /// The devices present now, ordered by id.
    #[must_use]
    pub fn list(&self) -> Vec<DeviceInfo> {
        self.state().table.list().cloned().collect()
    }

    /// Subscribes to the changes from now on. Each subscriber has a queue of
    /// its own, so one that reads does not take another's events.
    ///
    /// [`DeviceEvents::initial`] is the list at the same instant, which no
    /// event of the subscription repeats: there is no gap or overlap between
    /// reading the list and subscribing, whichever a caller does first.
    ///
    /// The queue is unbounded and a subscriber that never reads keeps one small
    /// event per change until it is dropped. Drop a `DeviceEvents` that is no
    /// longer read.
    pub fn subscribe(&self) -> DeviceEvents {
        let mut state = self.state();
        let queue = Arc::new(Queue::default());
        if let Some(failure) = &state.failure {
            // Nobody waits on a queue not yet handed out.
            drop(queue.end(failure.clone()));
        }
        state.subscribers.push(Arc::downgrade(&queue));
        DeviceEvents {
            initial: state.table.list().cloned().collect(),
            queue,
            _devices: self.clone(),
        }
    }

    /// Opens `id` for input. Its events land in an event inlet of `ports`, read
    /// by the audio thread on their frames: the node to add to the graph is
    /// returned with the [`OpenInput`], which holds the connection and the
    /// port, and dropping it closes both.
    ///
    /// The input's clock map reports its timing to `clock`; its writers drop
    /// with the [`OpenInput`], and with it on an error.
    ///
    /// Control thread only: the backend may start a thread.
    ///
    /// Opening an id that is already open opens a second connection to the
    /// device, and every event arrives twice. `Devices` does not track what is
    /// open; the session does, and its `connect_input` opens a device once.
    ///
    /// The list is not held while the OS opens the handle. A device that left
    /// in between, and whose handle the OS gave to another at once, opens that
    /// other one; the `Removed` that follows is how a holder finds out.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::NotFound`] when `id` is not present, [`MidiHardwareError::WrongDirection`]
    /// when it is not an input, or the platform error from opening it. A failed
    /// open leaves no port in `ports`.
    pub(crate) fn open_input(
        &self,
        started: Started,
        id: &MidiDeviceId,
        ports: &Arc<HardwareMidiInputs>,
        writers: DeviceWriters,
    ) -> Result<(OpenInput, EventInletNode)> {
        let (info, handle) = self.present(id, PortDirection::is_input)?;
        let (producer, inlet) = ports.open_input_port_with(info.name.as_str(), writers);
        let mut input = OpenInput {
            connection: None,
            port: producer.port(),
            ports: Arc::clone(ports),
            info,
            _executor: self.inner.executor.clone(),
        };
        // On an error `input` drops here, and takes the port with it.
        input.connection = Some(self.inner.backend.open_input(started, handle, producer)?);
        Ok((input, inlet))
    }

    /// Opens `id` for output. `started` is what [`start`](Self::start) resolved to:
    /// the backend is ready.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::NotFound`] when `id` is not present, [`MidiHardwareError::WrongDirection`]
    /// when it is not an output, or the platform error from opening it.
    pub fn open_output(&self, started: Started, id: &MidiDeviceId) -> Result<OpenOutput> {
        let (info, handle) = self.present(id, PortDirection::is_output)?;
        // What the OS says it speaks, until a negotiation says better.
        let described = described(&self.capability(id)?.unwrap_or_else(legacy)).active();
        let sink = self.inner.backend.open_output(started, handle)?;
        sink.follow(described.protocol);
        Ok(OpenOutput {
            info,
            sink: Arc::from(sink),
            wire: Wire::new(described),
            refused: 0,
            unrepresentable: 0,
            heartbeat: None,
            executor: self.inner.executor.clone(),
        })
    }

    /// The listing and OS handle of `id`, if it is present and `wanted` takes
    /// its direction.
    fn present(
        &self,
        id: &MidiDeviceId,
        wanted: fn(PortDirection) -> bool,
    ) -> Result<(DeviceInfo, EndpointId)> {
        let state = self.state();
        let (info, handle) = state
            .table
            .get(id)
            .zip(state.table.handle(id))
            .ok_or_else(|| NotFound::Device(id.clone()))?;
        wanted(info.direction)
            .then(|| (info.clone(), handle))
            .ok_or_else(|| MidiHardwareError::WrongDirection {
                id: id.clone(),
                direction: info.direction,
            })
    }

    /// What `saved`, a device from an earlier run, names among the devices
    /// present now. Bind a [`Resolved::Exact`] without asking; a
    /// [`Resolved::SameName`] may be another unit of the same name (ALSA
    /// identifies a device by its name only). See
    /// [`DeviceTable::resolve`](tutti_midi_device::DeviceTable::resolve) for the
    /// rule. A host that holds a session reaches this through
    /// `MidiSession::devices`.
    #[must_use]
    pub fn resolve(&self, saved: &DeviceInfo) -> Resolved {
        self.state().table.resolve(saved)
    }

    /// What the OS says `id` is, asked of the backend now: the Endpoint it
    /// describes, with its protocol in use, the protocols it supports and its
    /// Function Blocks. `None` when `id` is not present.
    ///
    /// # Errors
    ///
    /// The backend's error when the OS cannot be asked ([`MidiEndpoints::inputs`]).
    pub fn capability(&self, id: &MidiDeviceId) -> Result<Option<Endpoint>> {
        Ok(self.os_endpoint(id)?.map(|e| e.capability))
    }

    /// How many Function Blocks the OS declares for `id` that the spec cannot
    /// hold, which [`capability`](Self::capability) therefore leaves out: a
    /// device that declared more blocks than its Endpoint lists. `None` when
    /// `id` is not present.
    ///
    /// # Errors
    ///
    /// As [`capability`](Self::capability).
    pub fn blocks_left_out(&self, id: &MidiDeviceId) -> Result<Option<usize>> {
        Ok(self.os_endpoint(id)?.map(|e| e.blocks_left_out))
    }

    /// The backend's description of `id` now, `None` when `id` is not present.
    fn os_endpoint(&self, id: &MidiDeviceId) -> Result<Option<OsEndpoint>> {
        let Some(handle) = self.state().table.handle(id) else {
            return Ok(None);
        };
        let backend = &self.inner.backend;
        let mut found = backend.inputs()?;
        found.extend(backend.outputs()?);
        Ok(found.into_iter().find(|e| e.id == handle))
    }
}

/// The changes to the device list since [`Devices::subscribe`], oldest first.
///
/// One consumer's view: it is not `Clone`, and another subscription's reads
/// leave it alone. Dropping it unsubscribes at once.
pub struct DeviceEvents {
    initial: Vec<DeviceInfo>,
    queue: Arc<Queue>,
    // Keeps the watch running while events can still be expected from it.
    _devices: Devices,
}

impl Drop for DeviceEvents {
    fn drop(&mut self) {
        let mine = Arc::as_ptr(&self.queue);
        self._devices
            .state()
            .subscribers
            .retain(|queue| queue.as_ptr() != mine);
    }
}

impl core::fmt::Debug for DeviceEvents {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceEvents").finish_non_exhaustive()
    }
}

impl DeviceEvents {
    /// The devices present when this was subscribed to; every later change is
    /// an event.
    #[must_use]
    pub fn initial(&self) -> &[DeviceInfo] {
        &self.initial
    }

    /// The changes queued now, oldest first, taking each as it is iterated.
    /// Never blocks. `for event in &events` is the same.
    pub fn pending(&self) -> Pending<'_> {
        Pending { events: self }
    }

    /// The oldest change, once there is one: a future the watch's thread wakes.
    ///
    /// It resolves to the change, or to the [`WatchFailure`] once the watch has
    /// ended and every change it reported has been taken. Several futures
    /// awaiting one `DeviceEvents` are all woken and the first to poll takes the
    /// change; each change goes to one.
    pub fn recv(&self) -> Recv<'_> {
        Recv {
            events: self,
            id: None,
        }
    }

    /// Why the watch ended on its own, if it did: the list is then the last it
    /// saw and nothing more arrives. `None` while the watch runs.
    #[must_use]
    pub fn failure(&self) -> Option<WatchFailure> {
        self.queue.inbox().ended.clone()
    }
}

/// The wait for the next change of a [`DeviceEvents`]: [`DeviceEvents::recv`].
#[must_use = "a recv does nothing until it is awaited"]
pub struct Recv<'a> {
    events: &'a DeviceEvents,
    /// The id of the waker this registered in the inbox, until it is woken,
    /// resolved or dropped.
    id: Option<u64>,
}

impl core::fmt::Debug for Recv<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Recv").finish_non_exhaustive()
    }
}

impl Future for Recv<'_> {
    type Output = std::result::Result<DeviceEvent, WatchFailure>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut inbox = this.events.queue.inbox();
        if let Some(event) = inbox.events.pop_front() {
            let old = this.id.take().and_then(|id| inbox.forget(id));
            drop(inbox);
            drop(old);
            return Poll::Ready(Ok(event));
        }
        if let Some(failure) = inbox.ended.clone() {
            let old = this.id.take().and_then(|id| inbox.forget(id));
            drop(inbox);
            drop(old);
            return Poll::Ready(Err(failure));
        }
        // A re-poll replaces this future's own waker; a waker already taken by a
        // wake is registered again under the same id.
        let id = *this.id.get_or_insert_with(|| {
            inbox.next_id += 1;
            inbox.next_id
        });
        // The replaced waker is dropped once the inbox lock is released.
        let old = match inbox.wakers.iter_mut().find(|(held, _)| *held == id) {
            Some((_, waker)) if waker.will_wake(cx.waker()) => None,
            Some((_, waker)) => Some(std::mem::replace(waker, cx.waker().clone())),
            None => {
                inbox.wakers.push((id, cx.waker().clone()));
                None
            }
        };
        drop(inbox);
        drop(old);
        Poll::Pending
    }
}

impl Drop for Recv<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let old = self.events.queue.inbox().forget(id);
            drop(old);
        }
    }
}

/// The changes of a [`DeviceEvents`] queued now, taken as they are read:
/// [`DeviceEvents::pending`], and `&DeviceEvents`' `IntoIterator`.
#[derive(Debug)]
pub struct Pending<'a> {
    events: &'a DeviceEvents,
}

impl Iterator for Pending<'_> {
    type Item = DeviceEvent;

    fn next(&mut self) -> Option<DeviceEvent> {
        self.events.queue.inbox().events.pop_front()
    }
}

impl<'a> IntoIterator for &'a DeviceEvents {
    type Item = DeviceEvent;
    type IntoIter = Pending<'a>;

    fn into_iter(self) -> Pending<'a> {
        self.pending()
    }
}

/// An input held open: the OS connection to a device, and the ring port its
/// events land in.
///
/// Dropping it closes both, connection first, so nothing is still pushing into
/// a ring that is being retired. Drop it on a control
/// thread only: closing the connection joins the backend's thread and retiring
/// the port publishes. A device that leaves does not close it: the
/// holder learns from a [`DeviceEvent::Removed`] naming [`id`](Self::id) and
/// drops it (the session does, in `Phase::Poll`).
pub struct OpenInput {
    /// `None` only while [`Devices::open_input`] is still opening it.
    connection: Option<Box<dyn InputConnection>>,
    port: InputId,
    ports: Arc<HardwareMidiInputs>,
    info: DeviceInfo,
    /// Keeps the executor of the connection's thread alive: declared last, so the
    /// connection ends its thread and frees the port before the last clone of the
    /// executor can shut it down.
    _executor: Executor,
}

impl core::fmt::Debug for OpenInput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpenInput")
            .field("device", &self.info.id)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl OpenInput {
    /// The device this input is open on.
    #[must_use]
    pub fn id(&self) -> &MidiDeviceId {
        &self.info.id
    }

    /// The device's name when it was opened.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// Whether the connection is still delivering. `false` once the backend's
    /// reader has ended on an error: the device is still listed, but nothing
    /// will arrive until the input is reopened. [`fault`](Self::fault) says why.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.connection.as_ref().is_some_and(|c| c.is_alive())
    }

    /// Why the connection stopped delivering, `None` while it delivers (and
    /// for a connection already closed).
    #[must_use]
    pub fn fault(&self) -> Option<InputFault> {
        self.connection.as_ref().and_then(|c| c.fault())
    }

    /// The port its events land in, for the list's `set_port_active`.
    #[must_use]
    pub fn port(&self) -> InputId {
        self.port
    }
}

/// An output held open: the device, and the sink that sends to it. Dropping it
/// closes the connection.
///
/// **What it queues is put in the device's wire form first** (M2-104 §3.2, §3.3.2
/// and §7.1.6.2): a device in the MIDI 2.0 Protocol is sent the graph's MIDI 2.0
/// Channel Voice as it is; one in the MIDI 1.0 Protocol (and a MIDI 1.0 port) is
/// never sent type `0x4`, and its MIDI 2.0 messages go through one `Midi1Translator`
/// for this output (a Registered Controller as Control Changes 101, 100, 6 and
/// 38); and JR Timestamps go only to a device whose configuration receives them.
/// The configuration is the OS's description of the device until
/// [`set_config`](Self::set_config) says what a negotiation settled
/// ([`follow`](Self::follow) takes it from a
/// [`MidiSession::poll`](crate::MidiSession::poll) report). UMP Stream messages pass in every form.
pub struct OpenOutput {
    /// The Active Sensing task, once [`active_sensing`](Self::active_sensing)
    /// started it. Declared first: it is cancelled before the sink drops. A task
    /// being polled when it is cancelled ends on its driver, which then drops the
    /// sink it holds, so the connection may close on that thread.
    heartbeat: Option<Heartbeat>,
    info: DeviceInfo,
    sink: Arc<dyn DeviceOut>,
    wire: Wire,
    /// The packets the sink had no room for, for [`losses`](Self::losses).
    refused: u64,
    /// The events that became no packet in the device's wire form, for
    /// [`unrepresentable`](Self::unrepresentable).
    unrepresentable: u64,
    /// What the heartbeat is spawned on. Declared after `sink`: the sink ends its
    /// sender and frees the port before the last clone of the executor can shut
    /// it down.
    executor: Executor,
}

impl core::fmt::Debug for OpenOutput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpenOutput")
            .field("device", &self.info.id)
            .finish_non_exhaustive()
    }
}

impl OpenOutput {
    /// Starts this output sending Active Sensing (`0xFE`) from a task on the
    /// executor its [`Devices`] was opened with, whenever it has been silent for
    /// [`ACTIVE_SENSING_INTERVAL`](tutti_midi_runtime::ACTIVE_SENSING_INTERVAL),
    /// until it is dropped: a transmitter that starts goes on (MIDI 1.0
    /// Detailed Specification p. 32), so there is no stopping it short of
    /// closing the output. Dropping the output cancels the task without waiting
    /// for a send under way.
    ///
    /// **The task runs only while something drives the executor.** On an executor
    /// the app drives, Active Sensing stops when the app stops driving it, and a
    /// receiver that has seen one releases its notes after about 300 ms.
    /// The task sends nothing while the output's
    /// Protocol is MIDI 2.0, which has no Active Sensing, and sends again if
    /// the Protocol returns to MIDI 1.0.
    pub fn active_sensing(&mut self) {
        if self.heartbeat.is_none() {
            self.heartbeat = Some(Heartbeat::start(
                &self.executor,
                Arc::clone(&self.sink),
                self.wire.config().protocol,
            ));
        }
    }

    /// Whether the output can still send; see
    /// [`DeviceOut::is_alive`].
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.sink.is_alive()
    }

    /// What the output did not send: what the device had no room for when
    /// [`queue`](Self::queue) offered it (refused), and what the output took
    /// and then could not send ([`DeviceOut::losses`]).
    #[must_use]
    pub fn losses(&self) -> Losses {
        self.sink.losses() + Losses::refused(self.refused)
    }

    /// How many events the output took as sent that have no form on the wire of
    /// its device, and so went nowhere: a JR Timestamp the device does not take,
    /// a per-note controller for a MIDI 1.0 device, a message a byte port cannot
    /// carry ([`DeviceOut::unrepresentable`]). The drop is the spec's (M2-104
    /// Appendix D.2.8, M2-115); this is the count of it, apart from
    /// [`losses`](Self::losses), which counts only what the device had no room
    /// for.
    #[must_use]
    pub fn unrepresentable(&self) -> u64 {
        self.unrepresentable + self.sink.unrepresentable()
    }

    /// How many events the output has taken and not yet sent; see
    /// [`DeviceOut::pending`].
    #[must_use]
    pub fn pending(&self) -> usize {
        self.sink.pending()
    }

    /// The device this output is open on.
    #[must_use]
    pub fn id(&self) -> &MidiDeviceId {
        &self.info.id
    }

    /// The device's name when it was opened.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// The Protocol and JR Timestamps this output sends in now.
    #[must_use]
    pub fn config(&self) -> StreamConfig {
        self.wire.config()
    }

    /// Sends in `config` from now on: what the device's UMP Stream negotiation
    /// settled.
    pub fn set_config(&mut self, config: StreamConfig) {
        self.wire.set_config(config);
        self.sink.follow(config.protocol);
        if let Some(heartbeat) = &self.heartbeat {
            heartbeat.follow(config.protocol);
        }
    }

    /// Follows what a [`MidiSession::poll`](crate::MidiSession::poll) reported:
    /// a settled or changed Stream of this output's device is its
    /// [`set_config`](Self::set_config), and any other report, or another
    /// device's, changes nothing.
    pub fn follow(&mut self, polled: &crate::Polled) {
        if let crate::Polled::Stream(id, stream) = polled
            && *id == self.info.id
        {
            self.set_config(stream.config());
        }
    }
}

impl OpenOutput {
    /// Queues `events` in the device's wire form, each to go now, and returns
    /// what the device [`Taken`]. An event that has no form on the wire (a JR
    /// Timestamp the device does not take, a per-note controller for MIDI 1.0)
    /// counts as taken, and is counted in [`unrepresentable`](Self::unrepresentable).
    /// What is taken ends between two messages, so a System
    /// Exclusive message goes whole or not at all; what is not taken is
    /// counted in [`losses`](Self::losses). For a control thread: it allocates
    /// the batch it converts, and never blocks.
    pub fn queue(&mut self, events: &[UmpAt]) -> Taken {
        self.queue_timed(&Outgoing::now_batch(events))
    }

    /// As [`queue`](Self::queue), each packet going to the device at its
    /// [`SendAt`](tutti_midi_runtime::SendAt), which the conversion keeps with
    /// the event it came from.
    pub fn queue_timed(&mut self, events: &[Outgoing]) -> Taken {
        let (taken, refused) = self.offer(events);
        self.refused += refused;
        taken
    }

    /// Offers `events` as [`queue_timed`](Self::queue_timed) does, without
    /// counting what the device had no room for: that is returned, for a
    /// caller that offers the same events again until they are taken and
    /// counts the first refusal itself.
    pub(crate) fn offer(&mut self, events: &[Outgoing]) -> (Taken, u64) {
        self.wire.convert(events);
        let taken = self.sink.queue_timed(self.wire.batch());
        let refused = taken.left_of(self.wire.batch()) as u64;
        // Every send passes here: a packet the device took is traffic.
        if let Some(heartbeat) = &self.heartbeat
            && (self.wire.batch().len() as u64) > refused
        {
            heartbeat.sent();
        }
        let accepted = self.wire.accepted(taken, events);
        self.unrepresentable += self.wire.unrepresentable_in(accepted.packets());
        (accepted, refused)
    }

    /// Counts `packets` the device had no room for ([`offer`](Self::offer)).
    pub(crate) fn count_refused(&mut self, packets: u64) {
        self.refused += packets;
    }
}

impl Drop for OpenInput {
    fn drop(&mut self) {
        drop(self.connection.take());
        self.ports.remove_input_port(self.port);
    }
}

#[cfg(test)]
mod tests;
