//! [`Device`]: the sound card, and restarting it at whatever rate it comes
//! back with ([`Session::restart`]).
//!
//! The driver owns the stream and the graph's *audio* side only, so on its
//! own it can re-open a device and nothing more: a device that comes back at
//! another rate would play a graph built for the old one, every oscillator
//! off pitch, the beat clock off tempo, every figure in samples (PDC, a
//! lookahead) wrong, with no error anywhere. A restart runs the driver's
//! restart with a hook that, while no callback runs, moves everything the
//! session owns to the new configuration:
//!
//! - the graph: both halves of `Editor::reprepare`, inside the hook (the
//!   engine settled from the control side while no callback runs,
//!   `Stopped::apply_pending`), so the first block at the new rate renders
//!   the re-prepared graph, not the silent block of a re-prepare with its
//!   units out;
//! - the engine follows by itself: it adopts the new rate on the first block
//!   that carries it, rescaling its frame clock and every scheduled
//!   `At::Frame` transport command to the same wall-clock time;
//! - the graph's main output, widened to a wider device as the build widens it
//!   ([`main_output_width`](crate::main_output_width)), never narrowed;
//! - the transport's rate, the hardware MIDI input's timestamp rate, the
//!   session's disk streamer (every open stream's conversion ratio) and,
//!   through the re-prepare, the MIDI sync node;
//! - the compensation figures in samples ([`Session::compensation`],
//!   [`Session::latency`]), published before the first block.
//!
//! After the stream plays again, every input the session added
//! (`Session::add_input`, feature `capture`) is reopened at the new rate
//! and swapped into its inlet node: the node's key stays and nothing is
//! rewired.
//!
//! Every unit instance is kept across the re-prepare, and only time-based
//! state resets (`Editor::reprepare`'s rule).
//!
//! **Recovery after a failed hook:** the stream is left stopped, and the
//! driver keeps the old spec and graph rate ([`Device::graph_rate`]). A
//! restart onto the old device at the old rate moves nothing and plays
//! again.

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use tutti_buffers::{ChannelLayout, ChannelTopology, MaxBlock};
use tutti_cpal::{DeviceId, DeviceSelector, SnapshotSource, TuttiDriver};
use tutti_engine::{OutputQuantum, OutputSpec, Stopped, StreamDriver, StreamFaults, StreamLatency};
use tutti_units::{SampleRate, Samples, Seconds};

use crate::callback_size::{CallbackSize, CallbackSizeRule};
use crate::{
    Clock, DeviceClock, DevicePolicy, DeviceWatch, Error, FollowEvent, RealTime, Result, Session,
    SessionEvent, SessionFloat,
};

/// What a session on the device clock holds to run its blocks: the
/// [`Device`], the sound card's stream, and the device watch with what its
/// restarts reported. `!Send`, as the device is, so the session stays on the
/// thread that opened it.
///
/// The device is out of its slot only while a restart runs, because the
/// restart's hook needs the whole session; a panic in the hook puts it back
/// before it unwinds on.
#[derive(Debug)]
pub struct DeviceHandle<F: SessionFloat = f32> {
    device: Option<Device<F>>,
    /// The platform host the device was opened on, which a switch to the
    /// system clock hands on.
    pub(crate) audio_host: tutti_cpal::AudioHost,
    /// The device watch, and what restarts reported since the last poll.
    pub(crate) watch: Option<DeviceWatch>,
    pub(crate) events: Vec<SessionEvent>,
    /// What the stream's callbacks have shown of their size against the one
    /// the graph is prepared for.
    callback_size: CallbackSizeRule,
}

impl<F: SessionFloat> DeviceHandle<F> {
    /// A handle whose device is yet to be started: the builder's, until the
    /// stream is up.
    pub(crate) fn vacant(audio_host: tutti_cpal::AudioHost) -> Self {
        Self {
            device: None,
            audio_host,
            watch: None,
            events: Vec::new(),
            callback_size: CallbackSizeRule::default(),
        }
    }

    /// Puts the started device in its slot.
    pub(crate) fn fill(&mut self, device: Device<F>) {
        self.device = Some(device);
    }

    pub(crate) fn device(&self) -> &Device<F> {
        self.device
            .as_ref()
            .expect("the device is in its slot except while a restart runs")
    }

    pub(crate) fn device_mut(&mut self) -> &mut Device<F> {
        self.device
            .as_mut()
            .expect("the device is in its slot except while a restart runs")
    }

    /// The device, taken out of the handle: a switch to the system clock
    /// moves its host.
    pub(crate) fn into_device(mut self) -> Device<F> {
        self.take()
    }

    /// Takes the device out for a restart; [`fill`](Self::fill) puts it back.
    pub(crate) fn take(&mut self) -> Device<F> {
        self.device
            .take()
            .expect("the device is in its slot except while a restart runs")
    }

    /// The device clock's share of `Phase::Poll`: what restarts reported,
    /// then the watch, told first if the stream was lost.
    pub(crate) fn poll_device(
        &mut self,
        now: tutti_platform::Instant,
        lost: bool,
        out: &mut Vec<SessionEvent>,
    ) {
        out.append(&mut self.events);
        self.poll_callback_size(out);
        if let Some(watch) = &mut self.watch {
            // The loss is taken in first and decided on with the devices this
            // poll finds: a stream lost as its device left must not be sent
            // back to the device the last look still listed.
            if lost {
                watch.observe(FollowEvent::Lost);
            }
            watch.poll(now, out);
        }
    }
}

impl<F: SessionFloat> DeviceHandle<F> {
    /// Reads the sizes of the callbacks since the last update against the one
    /// the graph is prepared for ([`CallbackSizeRule`]): a size the callbacks
    /// settled at is asked for as a restart onto it, and callbacks that never
    /// settle are reported. A stream opened with no fixed callback size has
    /// none to compare. A callback longer than `MAX_FRAMES` is no size here:
    /// the engine renders its head, silences its tail and counts it
    /// (`BlockCounts::truncated`).
    fn poll_callback_size(&mut self, out: &mut Vec<SessionEvent>) {
        let device = self.device();
        let Some(prepared) = device.spec().quantum else {
            self.callback_size = CallbackSizeRule::default();
            return;
        };
        let reading = device.output_stream().callback_frames().take();
        let chosen = device.chosen.clone();
        match self.callback_size.observe(reading, prepared) {
            Some(CallbackSize::Settled(size)) => {
                out.push(SessionEvent::Device(DeviceEvent::RestartWanted(
                    DeviceRestart::wanted(chosen).quantum(size),
                )));
            }
            Some(CallbackSize::Varies) => {
                out.push(SessionEvent::Device(DeviceEvent::CallbackSizeVaries));
            }
            None => {}
        }
    }
}

/// A session on the sound card's clock
/// ([`SessionBuilder::device`](crate::SessionBuilder::device)): the device
/// sets the rate, the width and the callback, and a callback of the card's
/// steps the blocks.
///
/// **The device is the stream.** Dropping the session stops the callback, so
/// a build that is discarded is a warning: `let _ = build()` would drop the
/// session at once and silence it. Keep the session for as long as the sound
/// should play.
///
/// A discarded build is a warning (an error under `deny(unused_must_use)`):
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// fn build() -> tutti_session::LiveSession { unimplemented!() }
/// fn f() {
///     build();
/// }
/// ```
///
/// It is `!Send`: the device stays on the thread that opened it.
///
/// ```compile_fail
/// fn is_send<T: Send>() {}
/// is_send::<tutti_session::LiveSession>();
/// ```
pub type LiveSession<K = &'static str, F = f32> = Session<K, DeviceClock<F>>;

impl<K: crate::AppKey, F: SessionFloat> Session<K, DeviceClock<F>> {
    /// The device that drives the session.
    pub fn device(&self) -> &Device<F> {
        self.running.device()
    }

    /// The device, mutably: [`Device::stop`] and [`Device::start_with`].
    pub fn device_mut(&mut self) -> &mut Device<F> {
        self.running.device_mut()
    }

    /// What the device adds in each direction ([`Device::latency`]).
    #[must_use]
    pub fn device_latency(&self) -> DeviceLatency {
        self.device().latency()
    }

    /// Restarts the stream on the device `request` selects and moves the
    /// graph, the transport and every rate-derived figure to the
    /// configuration it comes back with (see the module docs), then reopens the
    /// inputs the session added.
    ///
    /// A request the device policy made ([`DeviceEvent::RestartWanted`]) is
    /// not a choice: it never changes what the policy follows, and it is
    /// dropped, returning [`RestartOutcome::Stale`], when the host has chosen something else
    /// since it was asked for (it is stale). Any other
    /// request is the host's choice, and what the policy follows from then on.
    ///
    /// The outcome is also reported by the next [`Session::update`] as
    /// [`DeviceEvent::Restarted`] or [`DeviceEvent::RestartFailed`].
    ///
    /// # Errors
    ///
    /// A [`RestartError`] whose [`error`](RestartError::error) is
    ///
    /// - [`Error::Reprepare`] **before the stream stops**, changing
    ///   nothing, when the graph cannot take the request: `max_block` past
    ///   the engine's block capacity (`CommitError::BlockTooLong`), a
    ///   re-prepare already between its halves (`Repreparing`; retry on a
    ///   later update), or a poisoned graph;
    /// - what resolving or opening the device reports ([`Error::Device`]);
    /// - [`Error::Reprepare`] or [`Error::Output`] from the re-prepare itself
    ///   (a graph that no longer compiles, a declared main output the device cannot
    ///   place): the stream is then left **stopped**. Restart again with
    ///   another `max_block`, or onto the old device and rate, which moves
    ///   nothing and plays again.
    pub fn restart(
        &mut self,
        request: DeviceRestart,
    ) -> std::result::Result<RestartOutcome, RestartError> {
        if request.wanted {
            // What the policy follows: the watch's selector, or with no watch
            // the device the host chose.
            let followed = self.running.watch.as_ref().map_or_else(
                || self.device().chosen.clone(),
                |w| w.follow().want().clone(),
            );
            if followed != request.device {
                return Ok(RestartOutcome::Stale);
            }
        } else {
            self.device_mut().chosen = request.device.clone();
            self.note_chosen(request.device.clone());
        }
        self.run_restart(request).map(RestartOutcome::Restarted)
    }

    /// Starts watching the host's devices: `source` is polled on
    /// every [`Phase::Poll`](crate::Phase::Poll), its changes are reported as
    /// [`DeviceEvent`]s, and `policy` decides which of them restart the stream
    /// ([`DeviceEvent::RestartWanted`]). What the policy follows is what the
    /// device was opened with, or the host last chose with
    /// [`restart`](Self::restart); the device it runs on is the one the stream
    /// opened ([`Device::opened_id`]; unknown, which is never taken for a
    /// different device, on a driver the host supplied). Replaces a watch
    /// already running.
    pub fn watch_devices(
        &mut self,
        source: impl SnapshotSource + Send + 'static,
        policy: DevicePolicy,
    ) {
        let want = self.device().chosen.clone();
        let running_on = self.device().opened_id(&want);
        self.start_watch(source, policy, want, running_on);
    }

    /// Starts watching `host`'s devices: [`watch_devices`](Self::watch_devices)
    /// over a [`DevicePoller`](tutti_cpal::DevicePoller) of `host` that looks every
    /// `interval`. The
    /// loop is a task on the session's executor, each look a step on its blocking pool;
    /// a look that fails is tried again at the next tick, and dropping or replacing the
    /// watch cancels the loop without waiting on a look in progress.
    ///
    /// # Errors
    ///
    /// [`DeviceError::HostUnavailable`](tutti_cpal::DeviceError::HostUnavailable) when
    /// `host` cannot be opened (the loop opens its own), and
    /// [`DeviceError::InvalidConfig`](tutti_cpal::DeviceError::InvalidConfig) when
    /// the session's executor has shut down.
    pub fn watch_host(
        &mut self,
        host: tutti_cpal::AudioHost,
        interval: tutti_cpal::PollInterval,
        policy: DevicePolicy,
    ) -> Result<()> {
        tutti_cpal::DeviceHost::open(host)?;
        let poller = tutti_cpal::DevicePoller::new(host, interval);
        let source = self.devices.poll_source(poller).ok_or_else(|| {
            tutti_cpal::DeviceError::InvalidConfig(
                "cannot watch the devices: the executor has shut down".into(),
            )
        })?;
        self.watch_devices(source, policy);
        Ok(())
    }

    /// Runs a restart on the session's device.
    pub(crate) fn run_restart(
        &mut self,
        request: DeviceRestart,
    ) -> std::result::Result<DeviceRestarted, RestartError> {
        let selector = request.device.clone();
        let max_block = request.max_block;
        let topology = request.topology;
        let quantum = request.quantum;
        let outcome = self.check_restart(max_block).and_then(|()| {
            let previous = self.device().graph_rate();
            self.with_device(|session, device| {
                let hook = |spec: &OutputSpec, stopped: &Stopped<'_, F>| {
                    session.rerate(spec, stopped, max_block, topology)
                };
                match quantum {
                    Some(quantum) => {
                        device
                            .driver
                            .restart_with_quantum(request.device, quantum, hook)
                    }
                    None => device.driver.restart_with(request.device, hook),
                }
            })
            .map(|()| previous)
        });
        self.note_restart(Some(&selector), outcome)
    }

    /// Runs `f` with the device out of its slot and the session beside it: the
    /// restart's hook moves the whole session while the device's driver runs.
    /// The device goes back before `f`'s panic, if it has one, unwinds on.
    pub(crate) fn with_device<R>(&mut self, f: impl FnOnce(&mut Self, &mut Device<F>) -> R) -> R {
        let mut device = self.running.take();
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self, &mut device)));
        self.running.fill(device);
        outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }

    /// A restart on a driver of the caller's choosing, `spec` standing for the
    /// config the new device reports: the device-free restart, for a test.
    /// `topology` states what the new device's channels are
    /// ([`DeviceRestart::topology`]); `None` takes the engine's order for its
    /// width. Behind `testing`.
    ///
    /// # Errors
    ///
    /// As [`restart`](Self::restart), without the device's own.
    #[cfg(feature = "testing")]
    pub fn restart_on<D: StreamDriver<F>>(
        &mut self,
        spec: OutputSpec,
        driver: D,
        max_block: Option<MaxBlock>,
        topology: Option<ChannelTopology>,
    ) -> std::result::Result<DeviceRestarted, RestartError>
    where
        D::Error: Into<tutti_cpal::DeviceError>,
    {
        let outcome = self.check_restart(max_block).and_then(|()| {
            let previous = self.device().graph_rate();
            self.with_device(|session, device| {
                device.driver.switch(spec, driver, |spec, stopped| {
                    session.rerate(spec, stopped, max_block, topology)
                })
            })
            .map(|()| previous)
        });
        self.note_restart(None, outcome)
    }

    /// Records a restart's outcome: the policy is told, and the next poll
    /// reports it ([`DeviceEvent::Restarted`] or [`DeviceEvent::RestartFailed`]).
    /// A failure that left the stream stopped keeps the old spec and graph
    /// rate, as the module docs say. After a restart that moved the stream
    /// the added inputs are reopened.
    fn note_restart(
        &mut self,
        selector: Option<&DeviceSelector>,
        outcome: Result<SampleRate>,
    ) -> std::result::Result<DeviceRestarted, RestartError> {
        let now = tutti_platform::Instant::now();
        match outcome {
            Ok(previous_rate) => {
                // A custom driver (`restart_on`) names no device: unknown.
                let name = selector.and_then(|s| self.device().opened_id(s));
                // The restart cleared the fault record; its count restarts.
                self.reports
                    .faults_restarted(self.device().faults().count());
                let restarted = DeviceRestarted {
                    previous_rate,
                    rate: self.device().graph_rate(),
                };
                self.running
                    .events
                    .push(SessionEvent::Device(DeviceEvent::Restarted(restarted)));
                self.note_device(FollowEvent::Opened(name), now);
                #[cfg(feature = "capture")]
                self.reopen_inputs();
                Ok(restarted)
            }
            Err(e) => {
                let failed = RestartError::new(e);
                self.running
                    .events
                    .push(SessionEvent::Device(DeviceEvent::RestartFailed(
                        failed.clone(),
                    )));
                self.note_device(
                    FollowEvent::RestartFailed {
                        running: self.device().is_running(),
                    },
                    now,
                );
                Err(failed)
            }
        }
    }

    /// While held, the device policy asks for no restart (the watch still
    /// reports devices). For a host that must not have the stream moved
    /// under it, a take being recorded: a restart can change the rate.
    pub fn pause_device_restarts(&mut self, held: bool) {
        if let Some(watch) = &mut self.running.watch {
            watch.hold(held);
        }
    }

    /// The selector the device policy follows, if a watch runs.
    #[must_use]
    pub fn watch_devices_want(&self) -> Option<DeviceSelector> {
        self.running
            .watch
            .as_ref()
            .map(|w| w.follow().want().clone())
    }

    pub(crate) fn note_chosen(&mut self, selector: DeviceSelector) {
        self.note_device(FollowEvent::Chose(selector), tutti_platform::Instant::now());
    }

    /// Feeds the policy and queues a restart it asks for.
    fn note_device(&mut self, event: FollowEvent, now: tutti_platform::Instant) {
        if let Some(watch) = &mut self.running.watch
            && let Some(request) = watch.note(event, now)
        {
            self.running
                .events
                .push(SessionEvent::Device(DeviceEvent::RestartWanted(request)));
        }
    }

    /// Starts the watch: [`Session::watch_devices`]'s, with the selector
    /// and the device it opened told.
    fn start_watch(
        &mut self,
        source: impl SnapshotSource + Send + 'static,
        policy: DevicePolicy,
        want: DeviceSelector,
        running_on: Option<DeviceId>,
    ) {
        self.running.watch = Some(DeviceWatch::new(source, policy, want, running_on));
    }

    /// [`Session::watch_devices`] over a session on its own, `want` the
    /// selector the stream was started with and `running_on` the device it
    /// opened (`None`: unknown): for a test that holds the pair apart.
    #[cfg(feature = "testing")]
    pub fn watch_devices_on(
        &mut self,
        source: impl SnapshotSource + Send + 'static,
        policy: DevicePolicy,
        want: DeviceSelector,
        running_on: Option<DeviceId>,
    ) {
        self.start_watch(source, policy, want, running_on);
    }
}

/// The sound card: owns the `TuttiDriver`. `!Send`, so it stays on the
/// thread that opened it.
///
/// `cpal::Stream` is `!Send`; `tutti-cpal` wraps it as `Send` on the
/// argument that the stream is created and dropped on the control thread. A
/// `Device` that could move would let a host drop the stream on another
/// thread, and that argument would be false with nothing to say so: here it
/// is a type error.
///
/// ```compile_fail
/// fn moves(device: tutti_session::Device) {
///     std::thread::spawn(move || drop(device));
/// }
/// ```
pub struct Device<F: SessionFloat = f32> {
    pub(crate) driver: TuttiDriver<F>,
    /// What the host last chose to run on (the build's, then a restart's):
    /// what the device policy follows.
    pub(crate) chosen: DeviceSelector,
    /// The capture delay the session's inputs report, read by
    /// [`latency`](Self::latency).
    input_latency: Arc<StreamLatency>,
    _not_send: PhantomData<*const ()>,
}

impl<F: SessionFloat> std::fmt::Debug for Device<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("driver", &self.driver)
            .finish()
    }
}

impl<F: SessionFloat> Device<F> {
    pub(crate) fn new(
        driver: TuttiDriver<F>,
        chosen: DeviceSelector,
        input_latency: Arc<StreamLatency>,
    ) -> Self {
        Self {
            driver,
            chosen,
            input_latency,
            _not_send: PhantomData,
        }
    }

    /// Whether the stream is open and its device has not been reported gone.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.driver.stream().is_running()
    }

    /// The identity of the device `selector` opened: the id it names, or for
    /// the default the device's name (cpal names the default and nothing
    /// else, so ordinal 0). `None` if the host cannot name it.
    #[must_use]
    pub fn opened_id(&self, selector: &DeviceSelector) -> Option<tutti_cpal::DeviceId> {
        match selector {
            DeviceSelector::Id(id) => Some(id.clone()),
            DeviceSelector::Default => self
                .device_name()
                .ok()
                .flatten()
                .map(tutti_cpal::DeviceId::new),
        }
    }

    /// The name of the output device the engine last opened, as it was then.
    /// It asks the host nothing, so it never enumerates the devices (which
    /// can block for good) and is safe on the thread that builds a frame.
    ///
    /// `Ok(None)` means the engine runs on a driver the host supplied
    /// (`SessionBuilder::build_device_on`, behind `testing`): it names no
    /// device, by design.
    ///
    /// # Errors
    ///
    /// When the device opened but the host could not name it.
    pub fn device_name(&self) -> Result<Option<String>> {
        Ok(self.driver.opened_name()?)
    }

    pub(crate) fn output_stream(&self) -> &tutti_engine::OutputStream {
        self.driver.stream()
    }

    /// The backend fault record, which survives stop and restart.
    /// [`Session::update`] reports each new fault as
    /// [`SessionEvent::Fault`].
    pub fn faults(&self) -> Arc<StreamFaults> {
        self.driver.stream().faults()
    }

    /// What the device adds in each direction, in frames at the graph's rate,
    /// as the open streams' callbacks last reported it: **output**, the
    /// callback's timestamp against the playback it predicts for the block
    /// just rendered; **input**, the capture instant of a frame against the
    /// callback that delivered it (what `Session::add_input`'s inputs
    /// report, feature `capture`).
    ///
    /// A direction is `None` until its stream's first callback has reported,
    /// after a restart (the new stream reports afresh, at the new rate), and
    /// on a backend whose timestamps give no difference to read: never a zero
    /// standing in for it. A device-free driver reports what its callbacks are
    /// told to. This is the device's, in time at the stream; the graph's own
    /// (the plan's compensation) is [`Session::latency`].
    #[must_use]
    pub fn latency(&self) -> DeviceLatency {
        let rate = self.graph_rate();
        let frames = |delay: Duration| Seconds::new(delay.as_secs_f64()).to_samples(rate);
        DeviceLatency {
            input: self.input_latency.get().map(frames),
            output: self.driver.stream().latency().get().map(frames),
        }
    }

    /// The configuration of the stream that is playing, or that would be.
    pub fn spec(&self) -> &OutputSpec {
        self.driver.stream().spec()
    }

    /// The rate the graph runs at, as far as the device knows: the build's,
    /// then that of the last restart whose hook succeeded.
    pub fn graph_rate(&self) -> SampleRate {
        self.driver.graph_rate()
    }

    /// Stops the stream. Idempotent.
    pub fn stop(&mut self) {
        self.driver.stop();
    }

    /// Stops any running stream and starts a new one on `driver`, at the
    /// current [`spec`](Self::spec): the same lifecycle with no device open.
    ///
    /// # Errors
    ///
    /// What the driver's `open` reports.
    pub fn start_with<D: StreamDriver<F>>(&mut self, driver: D) -> Result<()>
    where
        D::Error: Into<tutti_cpal::DeviceError>,
    {
        Ok(self.driver.start_with(driver)?)
    }

    /// The default host's output devices.
    ///
    /// **Blocks** for as long as the host's enumeration does: on ALSA it opens
    /// every PCM, which can wait on a sound server for good. Call it off the
    /// thread that renders or builds frames; the session's own listing
    /// ([`Session::list_outputs`]) is that, and a host may choose to call
    /// this one anyway.
    ///
    /// # Errors
    ///
    /// When the host cannot enumerate.
    pub fn devices() -> Result<impl Iterator<Item = DeviceId>> {
        let snapshot = tutti_cpal::DeviceHost::open(tutti_cpal::AudioHost::Default)?.snapshot()?;
        Ok(snapshot
            .devices(tutti_cpal::DeviceDirection::Output)
            .to_vec()
            .into_iter())
    }
}

/// What [`Session::restart`] did.
#[derive(Clone, Copy, Debug, PartialEq)]
#[must_use = "a stale request restarted nothing"]
pub enum RestartOutcome {
    /// The stream restarted; the rates before and after.
    Restarted(DeviceRestarted),
    /// A request the device policy made was dropped: the host has chosen
    /// something else since it was asked for. Nothing moved.
    Stale,
}

/// A restart that moved the stream ([`DeviceEvent::Restarted`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceRestarted {
    /// The rate the graph ran at before.
    pub previous_rate: SampleRate,
    /// The rate it runs at now.
    pub rate: SampleRate,
}

impl DeviceRestarted {
    /// Whether the rate moved: a capture stream opened at the old rate now
    /// feeds the graph frames of the wrong rate until it is reopened.
    #[must_use]
    pub fn rate_changed(&self) -> bool {
        self.previous_rate != self.rate
    }
}

/// What a restart asks for ([`Session::restart`]): the output device, and
/// at most how large a block the graph is prepared for from then on.
///
/// ```
/// use tutti_cpal::DeviceSelector;
/// use tutti_session::DeviceRestart;
///
/// let request = DeviceRestart::new(DeviceSelector::named("Interface"));
/// assert_eq!(request.max_block, None);
/// assert_eq!(DeviceRestart::default().device, DeviceSelector::Default);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRestart {
    /// The output device; the host's default by default.
    pub device: DeviceSelector,
    /// The largest block the graph is prepared for from the restart on;
    /// `None` keeps the one it has. At most the engine's block capacity
    /// (`Engine::block_capacity`): past it the restart is refused before the
    /// stream stops.
    ///
    /// A host's choice, not the device's: the device hands over whatever
    /// callback it likes, and the engine renders one longer than this as
    /// consecutive graph blocks.
    pub max_block: Option<MaxBlock>,
    /// What the new device's channels are, when the host states it
    /// ([`topology`](Self::topology)); `None` takes the engine's order for
    /// the device's width, and the device is read back as
    /// [`Placement::Default`](tutti_buffers::Placement::Default). It is the
    /// request's own: another device is wired another way, so a statement
    /// made for the device the session opened on is not carried over.
    pub topology: Option<ChannelTopology>,
    /// The callback size to open the device with; `None` keeps the one it was
    /// opened with. The size a device that delivers callbacks other than the
    /// requested one settled at is asked for this way
    /// ([`DeviceEvent::RestartWanted`]): the graph is prepared for it, and so
    /// is a hosted plugin's chunk.
    pub quantum: Option<OutputQuantum>,
    /// The device policy asked for it ([`DeviceEvent::RestartWanted`]): not a
    /// choice of the host's.
    wanted: bool,
}

impl Default for DeviceRestart {
    /// A restart onto the host's default device, keeping the block size.
    fn default() -> Self {
        Self::new(DeviceSelector::Default)
    }
}

impl DeviceRestart {
    /// A restart onto `device`, keeping the block size.
    #[must_use]
    pub fn new(device: impl Into<DeviceSelector>) -> Self {
        Self {
            device: device.into(),
            max_block: None,
            topology: None,
            quantum: None,
            wanted: false,
        }
    }

    /// This restart stating what the new device's channels are: the graph
    /// main output is folded into `topology` by speaker. The restart is refused
    /// ([`RestartError`]) when `topology` is not the new device's width.
    #[must_use]
    pub fn topology(mut self, topology: ChannelTopology) -> Self {
        self.topology = Some(topology);
        self
    }

    /// This restart preparing the graph for blocks of up to `max_block`.
    #[must_use]
    pub fn max_block(mut self, max_block: MaxBlock) -> Self {
        self.max_block = Some(max_block);
        self
    }

    /// This restart opening the device with callbacks of `quantum` frames
    /// ([`quantum`](Self::quantum)).
    #[must_use]
    pub fn quantum(mut self, quantum: OutputQuantum) -> Self {
        self.quantum = Some(quantum);
        self
    }

    /// What the device policy asks for: a restart onto `device`, which the
    /// host runs through [`Session::restart`] like any other.
    pub(crate) fn wanted(device: DeviceSelector) -> Self {
        Self {
            device,
            max_block: None,
            topology: None,
            quantum: None,
            wanted: true,
        }
    }
}

/// What a restart is refused or fails with ([`Session::restart`],
/// [`DeviceEvent::RestartFailed`]): the session's [`Error`], shared, so the
/// caller and the next [`Session::update`] both hold it. It is the error's
/// [`source`](std::error::Error::source) and
/// [`error`](Self::error) says which.
#[derive(Clone, Debug)]
pub struct RestartError {
    source: Arc<Error>,
}

impl std::fmt::Display for RestartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the device restart failed: {}", self.source)
    }
}

impl std::error::Error for RestartError {
    /// The session's [`Error`] itself (not the shared pointer to it), so a
    /// chain walker can downcast to it.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

impl RestartError {
    pub(crate) fn new(error: Error) -> Self {
        Self {
            source: Arc::new(error),
        }
    }

    /// Why the restart failed.
    #[must_use]
    pub fn error(&self) -> &Error {
        &self.source
    }
}

/// How much delay a device adds, in frames at the graph's rate
/// ([`Device::latency`]).
///
/// A direction nobody has reported is `None`, never a zero standing in for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceLatency {
    /// Capture to callback: how long before a graph block a frame was sung
    /// (`None`: no input is open, or its callbacks have reported nothing).
    pub input: Option<Samples>,
    /// Callback to playback: how long after a block is rendered it is heard.
    pub output: Option<Samples>,
}

/// Something about the sound card or its devices that an
/// [`update`](Session::update) found ([`SessionEvent::Device`]).
#[derive(Debug)]
#[non_exhaustive]
pub enum DeviceEvent {
    /// A device is no longer listed by the host (a [`DeviceWatch`] is
    /// running). The stream's own loss is [`SessionEvent::Fault`].
    Lost {
        /// Which way it played or captured.
        direction: tutti_cpal::DeviceDirection,
        /// The device.
        id: DeviceId,
    },
    /// A device is listed that was not (the first look lists them all).
    Appeared {
        /// Which way it plays or captures.
        direction: tutti_cpal::DeviceDirection,
        /// The device.
        id: DeviceId,
    },
    /// The host's default device moved; `None`: there is none now.
    DefaultChanged {
        /// Which direction's default.
        direction: tutti_cpal::DeviceDirection,
        /// The new default.
        id: Option<DeviceId>,
    },
    /// The device policy wants the stream restarted: run it with
    /// [`Session::restart`]. A request with a [`quantum`](DeviceRestart::quantum)
    /// says the stream's callbacks settled at that size and not the one the
    /// graph is prepared for; asked once for each size. The outcome is reported by the next update
    /// ([`Restarted`](Self::Restarted) or [`RestartFailed`](Self::RestartFailed)),
    /// and tells the policy, which waits out its backoff before it asks again.
    RestartWanted(DeviceRestart),
    /// A restart moved the stream. An input the session added is reopened by
    /// the restart itself (`Session::add_input`); a capture stream a host
    /// holds itself at the graph's rate is its own to reopen when
    /// [`rate_changed`](DeviceRestarted::rate_changed).
    Restarted(DeviceRestarted),
    /// A restart was refused or failed. See [`Session::restart`] for the
    /// stream's state.
    RestartFailed(RestartError),
    /// An input the session added could not be reopened by a restart: it
    /// stays in the graph, silent, and is tried again at the next restart.
    #[cfg(feature = "capture")]
    InputReopenFailed {
        /// The input's inlet node ([`Input::node`](crate::Input::node), erased).
        input: tutti_graph::NodeKey,
        /// Why not.
        error: Error,
    },
    /// A capture the session held ended because the session left the sound
    /// card's clock ([`Session::into_system_clock`]): the input's node stays in
    /// the graph, silent. One event for each input.
    #[cfg(feature = "capture")]
    InputClosed {
        /// The input's inlet node ([`Input::node`](crate::Input::node), erased).
        input: tutti_graph::NodeKey,
    },
    /// The stream's callbacks keep changing size and settle at none, so no
    /// size the graph is prepared for lines up with them (a hosted plugin
    /// then loses most of its output). Reported once for a stream, until a
    /// restart or a callback of the prepared size.
    CallbackSizeVaries,
    /// The listing started with [`Session::list_outputs`] answered: the
    /// host's output devices, in enumeration order. Reported once.
    OutputsListed(Vec<DeviceId>),
    /// The listing started with [`Session::list_outputs`] failed. Reported
    /// once.
    OutputsListFailed(tutti_cpal::DeviceError),
    /// The listing started with [`Session::list_outputs`] has not answered
    /// after 5 s of [`PhaseCx::now`](crate::PhaseCx::now): the host's
    /// enumeration is stuck or slow. Reported once; the listing goes on waiting.
    OutputsListSlow(Duration),
}

impl From<DeviceEvent> for SessionEvent {
    fn from(event: DeviceEvent) -> Self {
        Self::Device(event)
    }
}

impl<K: crate::AppKey, C: RealTime> Session<K, C> {
    /// Starts `listing` (a [`DeviceListing::new`](tutti_cpal::DeviceListing::new) of a
    /// host, or a [`from_fn`](tutti_cpal::DeviceListing::from_fn) fake): enumeration can
    /// block for good, so it runs as one step on the session executor's blocking pool and
    /// never here. The answer is taken without blocking by a later
    /// [`Phase::Poll`](crate::Phase::Poll) and reported once, as
    /// [`DeviceEvent::OutputsListed`] or [`DeviceEvent::OutputsListFailed`];
    /// [`DeviceEvent::OutputsListSlow`] is reported once if it takes 5 s.
    /// Replaces a listing still waiting, whose step is left to end on its
    /// own.
    pub fn list_outputs(&mut self, listing: tutti_cpal::DeviceListing) {
        self.devices.start(listing);
    }

    /// [`list_outputs`](Self::list_outputs), once: the first call starts `listing` and
    /// later calls drop theirs unrun.
    pub fn list_outputs_once(&mut self, listing: tutti_cpal::DeviceListing) {
        if self.devices.listed() {
            return;
        }
        self.list_outputs(listing);
        self.devices.mark_listed();
    }
}

impl<K: crate::AppKey, C: Clock> Session<K, C> {
    /// The checks that must pass before the stream stops.
    pub(crate) fn check_restart(&self, max_block: Option<MaxBlock>) -> Result<()> {
        self.graph.check_reprepare(max_block)?;
        Ok(())
    }

    /// Whether the main output in force can be placed on `device`: what a
    /// restart or a clock switch refuses before anything moves.
    pub(crate) fn check_device(&self, device: &tutti_buffers::ChannelSet) -> Result<()> {
        let channels = device.width();
        // A map names the main output's own channels, so a device swap does not widen
        // a main output that has one; without it a wider device widens an undeclared
        // main output by the build's rule.
        let policy = self.output.policy.current();
        let mapped = policy.map.targets().is_some();
        let main_output = self
            .graph
            .main_output_topology()
            .cloned()
            .unwrap_or_else(|| {
                let width = if mapped {
                    self.graph.outputs()
                } else {
                    crate::main_output_width(ChannelLayout::clamped(self.graph.outputs()), channels)
                };
                ChannelTopology::default_for(ChannelLayout::clamped(width))
                    .expect("a main output is at most MAX_CHANNELS wide")
            });
        // The policy in force has to place the main output on the new device too: a
        // device a map does not fit is refused, never reset to the identity;
        // the host sets a new map.
        policy
            .routing()
            .fold::<C::Float>(&main_output, device)
            .map_err(|refusal| match refusal {
                tutti_engine::RoutingError::Fold(source) => Error::Unplaceable {
                    main_output: self.graph.main_output_id(),
                    width: main_output.width(),
                    device: channels,
                    source,
                },
                map => Error::MapUnplaceable {
                    map: policy.map.clone(),
                    device: channels,
                    source: map,
                },
            })?;
        Ok(())
    }

    /// The hook: move everything the session owns to `spec`, with no
    /// callback running.
    pub(crate) fn rerate(
        &mut self,
        spec: &OutputSpec,
        stopped: &Stopped<'_, C::Float>,
        max_block: Option<MaxBlock>,
        topology: Option<ChannelTopology>,
    ) -> Result<()> {
        let rate = spec.sample_rate;
        // The new device is what the request states or the engine's order for
        // its width. The main output in force has to be placeable on it:
        // refused before the graph moves, the stream still stopped, like any
        // refusal here. The main output a wider device will widen an
        // undeclared one to is the main output in force.
        let device = crate::policy::device_placement(spec.channels, topology)?;
        self.check_device(&device.channels)?;
        // The new device's callback size too: a hosted plugin's pipeline
        // follows it, and its declared latency with it.
        self.graph
            .reprepare(rate, max_block, spec.quantum.map(OutputQuantum::max_block))?;
        // The transport's rate first: it marks every frame-timed command sent
        // from here on as already in the new rate's frames, and the engine's
        // settle below rescales only the ones before the mark.
        self.transport.set_sample_rate(rate);
        // Both halves of the re-prepare, now, while no callback runs: the
        // executor checks its units out, the editor re-prepares them and
        // sends the resumed plan, and the executor adopts it.
        stopped.apply_pending();
        self.graph.collect();
        self.sync_main_output_refusal();
        // A unit that panicked in `prepare`, or shapes that no longer
        // compile, poison the editor here: refuse, the stream still stopped.
        self.graph.check_reprepare(None)?;
        // Every refusal above left the engine's device as it was: the new one
        // is placed only now, so a refused restart moves nothing, and a
        // restart onto the old device places it again.
        self.graph.place_device(device)?;
        // The outputs' taps read frames of one rate: reopened at the
        // new one, committed with the rest below.
        if self
            .midi
            .follow_rate(&mut self.graph, tutti_platform::Instant::now())
        {
            self.commit();
        }
        // A declaration a device refused is asked again of this one: if it
        // places, it is in force, committed and settled below with the rest.
        if self.output.reattempt(&mut self.graph) {
            self.commit();
        }
        // A wider device widens the main output, by the build's rule; a narrower one
        // keeps it, and the engine folds. Committed and settled here too, so
        // the first block has the width. A commit refused for now leaves the
        // edit flag set, and the next update retries.
        // A declared main output keeps the width it declared: the device is no floor
        // under it.
        let mapped = self.output.policy.current().map.targets().is_some();
        let width =
            crate::main_output_width(ChannelLayout::clamped(self.graph.outputs()), spec.channels);
        if self.graph.main_output_topology().is_none() && !mapped && width > self.graph.outputs() {
            // Undeclared, so it cannot be refused.
            let widened = self.graph.widen_outputs(width);
            debug_assert!(widened.is_ok());
            self.commit();
        }
        let running = stopped.apply_pending();
        debug_assert!(running, "the re-prepare resumed inside the hook");
        // The figures the first block runs at, published before it.
        self.graph.publish_sent();
        // The hardware inlets place an arrival a block after it came, a
        // block of this device's callback; the clock map follows the rate
        // from the engine's own readings.
        self.midi.set_block(
            spec.quantum
                .map_or(self.graph.prepared().max_block(), OutputQuantum::max_block)
                .samples(),
            rate,
        );
        // Every open stream's ratio (file rate / session rate) is
        // re-derived, so it plays on at its pitch.
        self.sampler.set_sample_rate(rate);
        // The heard tap opens at the device's width from here on; one open
        // across this change keeps counting its blocks lost (see
        // `Session::open_heard_tap`).
        self.output.set_device_width(spec.channels);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use tutti_buffers::ChannelLayout;
    use tutti_engine::{ManualStreamDriver, OutputSpec};
    use tutti_units::SampleRate;

    use crate::Session;

    /// **A panic inside a restart's hook leaves the device in its slot**: the
    /// device is out of the session while the hook runs, and the guard puts it
    /// back before the panic unwinds on, so the session still has its device
    /// (and its stream's rate) afterwards.
    #[test]
    fn a_panic_in_a_restart_hook_puts_the_device_back_in_its_slot() {
        let (driver, _stream) = ManualStreamDriver::new();
        let spec = OutputSpec::new(
            SampleRate::SR_48K,
            ChannelLayout::STEREO,
            tutti_buffers::SampleFormat::F32,
        );
        let mut session = Session::builder()
            .device(tutti_cpal::DeviceSelector::Default)
            .build_device_on(spec, driver)
            .expect("builds with no device");
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            session.with_device(|_, _| panic!("a hook that fails"))
        }));
        assert!(outcome.is_err(), "the panic unwinds on");
        assert_eq!(session.device().graph_rate(), SampleRate::SR_48K);
    }
}
