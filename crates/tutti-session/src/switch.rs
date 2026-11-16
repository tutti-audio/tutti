//! A session changes clock: [`Session::into_device`] and
//! [`Session::into_system_clock`], and the [`ClockError`] that hands the
//! session back when they fail.
//!
//! A session moves between the system clock and the sound card's clock by a
//! consuming conversion, a driver switch on the restart machinery: the new
//! stream's device is resolved, every refusal that can be known is made, and
//! only then the old stream stops, the graph moves to the new configuration
//! while no callback runs (the same hook a device restart runs), and the new
//! stream starts.
//!
//! - **Everything the session holds carries over:** the graph, keys, knobs, the
//!   pool, the transport, the heard tap and the load meter. The transport keeps
//!   its place: a rolling one resumes at the beat the last block ended on, so
//!   the switch costs wall-clock time and no musical time.
//! - **Jobs in flight carry over:** a job's lander names nothing of the clock,
//!   so a job still running when the switch begins lands on the converted
//!   session, once. Pool loads are jobs, so the entries loading carry over and
//!   land after the switch.
//! - **Runs asked with [`Session::when_loaded`] cannot:** a run takes the
//!   session of the clock it was asked of. The runs whose entry has loaded run
//!   before the switch begins, with the session as it was; a switch with a run
//!   waiting for an entry that has not loaded is refused
//!   ([`Error::RunsPending`]), before anything stops, and goes through once the
//!   run has fired.
//! - **Captures end** when the session leaves the sound card: each input the
//!   session held closes and is reported as
//!   [`DeviceEvent::InputClosed`](crate::DeviceEvent) (feature `audio-io`).
//! - **A rate that differs** is [`SessionEvent::RateChanged`].
//! - **A failure gives the session back, running on its old clock,** with one
//!   exception. Everything that can be known is refused before the old stream
//!   stops (the graph root and the output map on the new channels, a re-prepare
//!   that cannot run, a run waiting), and a device that will not open fails
//!   before anything stops. A failure after the stop (the new stream will not
//!   open, the re-prepare refuses) is undone by starting the old clock again at
//!   its old rate and block. Only when that fails too, a graph the re-prepare
//!   poisoned, is the session returned **stopped**, and the error then is
//!   [`Error::SwitchNotUndone`], which names both failures.

use tutti_cpal::{AudioEngine, DeviceHost, DeviceSelector, TuttiDriver};
use tutti_engine::{OutputSpec, StreamDriver, TimerDriver};
use tutti_types::{ChannelLayout, SampleRate};

use crate::clock::SystemEngine;
use crate::device::DeviceHandle;
use crate::{
    Device, DeviceClock, Error, LiveSession, RestartError, Result, Session, SessionEvent,
    SystemClock, SystemSession,
};

/// A clock switch that failed, with the session it was asked of.
///
/// As [`std::io::IntoInnerError`] does: the error says what went wrong, and
/// [`into_session`](Self::into_session) gives the session back. **It is running
/// on the clock it was on, unless the error is [`Error::SwitchNotUndone`]**,
/// where starting that clock again failed too and the session is stopped.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct ClockError<S> {
    session: Box<S>,
    error: RestartError,
}

impl<S> ClockError<S> {
    pub(crate) fn new(session: S, error: Error) -> Self {
        Self {
            session: Box::new(session),
            error: RestartError::new(error),
        }
    }

    /// Why the switch failed.
    #[must_use]
    pub fn error(&self) -> &RestartError {
        &self.error
    }

    /// The session, on the clock it was on.
    #[must_use]
    pub fn into_session(self) -> S {
        *self.session
    }

    /// The session and why the switch failed.
    #[must_use]
    pub fn into_parts(self) -> (S, RestartError) {
        (*self.session, self.error)
    }
}

/// The error of a switch that failed after the old stream stopped: the
/// switch's own if the old clock started again, and both if it did not.
fn after_stop(switch: Error, undone: Result<()>) -> Error {
    match undone {
        Ok(()) => switch,
        Err(undo) => Error::SwitchNotUndone {
            switch: Box::new(switch),
            undo: Box::new(undo),
        },
    }
}

/// Reports a rate the switch changed.
fn note_rate_change(events: &mut Vec<SessionEvent>, from: SampleRate, to: SampleRate) {
    if from != to {
        events.push(SessionEvent::RateChanged { from, to });
    }
}

/// The input latency cell the card's session reads, with the output's cell
/// handed to the inputs (feature `audio-io`).
#[cfg(feature = "audio-io")]
fn bind_inputs(
    live: &mut LiveSession,
    driver: &TuttiDriver,
) -> std::sync::Arc<tutti_engine::StreamLatency> {
    live.inputs.set_output_latency(driver.stream().latency());
    live.inputs.latency()
}

#[cfg(not(feature = "audio-io"))]
fn bind_inputs(
    _: &mut LiveSession,
    _: &TuttiDriver,
) -> std::sync::Arc<tutti_engine::StreamLatency> {
    std::sync::Arc::default()
}

impl<C: crate::Clock> Session<C> {
    /// What a switch makes before anything stops, so a refusal leaves the
    /// session as it was:
    ///
    /// - the root and the output map in force have to be placeable on the new
    ///   channels, and the graph has to be able to re-prepare;
    /// - the runs asked with `when_loaded` whose entry has loaded run now, with
    ///   the session as it is (they are the app's closures, and the clock they
    ///   were asked of is the one the session is still on);
    /// - no run may be left waiting for an entry that has not loaded
    ///   ([`Error::RunsPending`]): it cannot follow the session to the next clock.
    fn check_switch(&mut self, channels: ChannelLayout) -> Result<()> {
        self.check_device(channels, None)?;
        self.check_restart(None)?;
        self.refuse_waiting_runs()?;
        self.pool_run_loaded();
        // A run may have asked another.
        self.refuse_waiting_runs()
    }

    fn refuse_waiting_runs(&self) -> Result<()> {
        let keys = self.pool.waiting_runs();
        if keys.is_empty() {
            Ok(())
        } else {
            Err(Error::RunsPending { keys })
        }
    }
}

impl Session<SystemClock> {
    /// Moves the session onto the sound card `device` selects: the device is
    /// opened first, and a device that will not open returns the session
    /// running, on the timer, with the error. The graph is re-prepared at the
    /// device's rate and callback size, as a device restart does.
    ///
    /// **A failure gives the session back running on the timer**: before the
    /// timer stops when the device cannot be opened or the graph cannot take
    /// it, and after it (the card's stream will not open, the re-prepare
    /// refuses) by starting the timer again at the spec it had. The session
    /// comes back stopped only with [`Error::SwitchNotUndone`].
    ///
    /// A panic during the switch (a node's `prepare`, the OS) unwinds out of
    /// this call and drops the session: it is not returned as a [`ClockError`].
    ///
    /// # Errors
    ///
    /// A [`ClockError`] whose error is [`Error::Device`] when the device cannot
    /// be found or opened, [`Error::Unplaceable`], [`Error::MapUnplaceable`],
    /// [`Error::Reprepare`] or [`Error::RunsPending`] when the graph cannot
    /// take the device **before the timer stops**, the re-prepare's or the
    /// card's own failure after it, and [`Error::SwitchNotUndone`] when the
    /// timer could not be started again.
    pub fn into_device(
        self,
        device: DeviceSelector,
    ) -> std::result::Result<LiveSession, ClockError<Self>> {
        let opened = DeviceHost::open(self.running.audio_host).and_then(|host| {
            let engine = AudioEngine::open(host, device.clone())?;
            let resolved = engine.resolve_device()?;
            Ok((engine, resolved))
        });
        let (engine, resolved) = match opened {
            Ok(found) => found,
            Err(e) => return Err(ClockError::new(self, e.into())),
        };
        let channels = resolved.spec().channels;
        self.switch_to_device(channels, device, move |host, session| {
            TuttiDriver::start_resolved(host, engine, resolved, |spec, stopped| {
                session.rerate(spec, stopped, None, None)
            })
        })
    }

    /// [`into_device`](Self::into_device) on a driver of the caller's choosing,
    /// `spec` standing for the config its device reports: the device-free
    /// switch, for a test. Behind `testing`.
    ///
    /// # Errors
    ///
    /// As [`into_device`](Self::into_device), without the device's own.
    #[cfg(feature = "testing")]
    pub fn into_device_on<D: StreamDriver>(
        self,
        spec: OutputSpec,
        driver: D,
    ) -> std::result::Result<LiveSession, ClockError<Self>>
    where
        D::Error: Into<tutti_cpal::Error>,
    {
        let channels = spec.channels;
        self.switch_to_device(channels, DeviceSelector::default(), move |host, session| {
            TuttiDriver::start_on(host, spec, driver, |spec, stopped| {
                session.rerate(spec, stopped, None, None)
            })
        })
    }

    /// The switch to a card whose channels are `channels`; `start` starts its
    /// stream on the timer's host, running the re-prepare in between.
    fn switch_to_device(
        mut self,
        channels: ChannelLayout,
        chosen: DeviceSelector,
        start: impl FnOnce(
            tutti_engine::StreamHost,
            &mut Self,
        )
            -> std::result::Result<TuttiDriver, (tutti_engine::StreamHost, Error)>,
    ) -> std::result::Result<LiveSession, ClockError<Self>> {
        if let Err(e) = self.check_switch(channels) {
            return Err(ClockError::new(self, e));
        }
        let previous = self.graph.prepared().sample_rate();
        // What the timer's stream reported since the last update is reported
        // before the card's stream clears the record.
        let mut unpolled = Vec::new();
        self.fault_poll(&mut unpolled);
        let old = self.running.host().stream().spec().clone();
        let host = self.running.take_host();
        match start(host, &mut self) {
            Ok(driver) => Ok(self.on_device(driver, chosen, previous, unpolled)),
            Err((host, switch)) => {
                self.running.host = Some(host);
                let undone = self.back_to_the_timer(old);
                self.carried_events.append(&mut unpolled);
                Err(ClockError::new(self, after_stop(switch, undone)))
            }
        }
    }

    /// A switch failed after the timer stopped: the timer starts again at the
    /// spec it had, the graph re-prepared at its rate and block. If that fails
    /// too the session stays stopped, and the host says what the graph is
    /// prepared at.
    fn back_to_the_timer(&mut self, old: OutputSpec) -> Result<()> {
        let mut host = self.running.take_host();
        let restored = host.restart_with(
            || Ok::<_, Error>((old, TimerDriver::new())),
            |spec, stopped| self.rerate(spec, stopped, None, None),
        );
        if restored.is_err() {
            host.settle(self.graph.prepared().sample_rate());
        }
        self.faults_seen = host.stream().faults().count();
        self.running.host = Some(host);
        restored
    }

    /// The session on the sound card's clock, now the card's stream runs.
    fn on_device(
        self,
        driver: TuttiDriver,
        chosen: DeviceSelector,
        previous: SampleRate,
        mut events: Vec<SessionEvent>,
    ) -> LiveSession {
        let audio_host = self.running.audio_host;
        let (mut live, _timer) = self.with_clock::<DeviceClock>(DeviceHandle::vacant(audio_host));
        let input_latency = bind_inputs(&mut live, &driver);
        let device = Device::new(driver, chosen, input_latency);
        live.faults_seen = device.faults().count();
        live.running.fill(device);
        let rate = live.device().graph_rate();
        note_rate_change(&mut events, previous, rate);
        live.carried_events.append(&mut events);
        live
    }
}

impl SystemEngine {
    fn take_host(&mut self) -> tutti_engine::StreamHost {
        self.host
            .take()
            .expect("the stream host is in its slot except while a clock switch runs")
    }
}

impl Session<DeviceClock> {
    /// Moves the session onto an OS timer thread: the stream stops, the graph
    /// keeps the rate it ran at and takes the graph's block as the timer's,
    /// and the session has no audio in or out. The captures the session held
    /// end ([`DeviceEvent::InputClosed`](crate::DeviceEvent), feature
    /// `audio-io`); the device watch ends with the device.
    ///
    /// **A failure gives the session back running on the card**: before the
    /// stream stops when the graph cannot re-prepare or a run waits
    /// ([`Error::RunsPending`]), and after it (the timer thread would not start,
    /// or the re-prepare refused) by opening the card's device again, which
    /// resolves it afresh: its config is what the host reports then. The
    /// session comes back stopped only with [`Error::SwitchNotUndone`].
    ///
    /// A panic during the switch (a node's `prepare`, the OS) unwinds out of
    /// this call and drops the session: it is not returned as a [`ClockError`].
    ///
    /// # Errors
    ///
    /// A [`ClockError`] whose error is [`Error::Reprepare`] or
    /// [`Error::RunsPending`] before the stream stops, the re-prepare's or the
    /// timer's own failure ([`Error::Clock`]) after it, and
    /// [`Error::SwitchNotUndone`] when the card could not be started again.
    pub fn into_system_clock(self) -> std::result::Result<SystemSession, ClockError<Self>> {
        self.into_timer(TimerDriver::new(), |session, device, _| {
            let chosen = device.chosen.clone();
            device.driver.restart_with(chosen, |spec, stopped| {
                session.rerate(spec, stopped, None, None)
            })
        })
    }

    /// [`into_system_clock`](Self::into_system_clock) on a timer driver of the
    /// caller's choosing, and `back` the driver the card is started again on
    /// should that fail, at the spec it had: the device-free switch, for a
    /// test. Behind `testing`.
    ///
    /// # Errors
    ///
    /// As [`into_system_clock`](Self::into_system_clock).
    #[cfg(feature = "testing")]
    pub fn into_system_clock_on<B: StreamDriver>(
        self,
        timer: impl StreamDriver<Error = tutti_engine::TimerError>,
        back: B,
    ) -> std::result::Result<SystemSession, ClockError<Self>>
    where
        B::Error: Into<tutti_cpal::Error>,
    {
        self.into_timer(timer, |session, device, old| {
            device.driver.switch(old, back, |spec, stopped| {
                session.rerate(spec, stopped, None, None)
            })
        })
    }

    /// The switch to a timer driven by `timer`; `undo` starts the card again,
    /// given the spec it had.
    fn into_timer<D>(
        mut self,
        timer: D,
        undo: impl FnOnce(&mut Self, &mut Device, OutputSpec) -> Result<()>,
    ) -> std::result::Result<SystemSession, ClockError<Self>>
    where
        D: StreamDriver<Error = tutti_engine::TimerError>,
    {
        let rate = self.device().graph_rate();
        let old = self.device().spec().clone();
        let mut spec = old.clone().with_quantum(self.graph.prepared().max_block());
        spec.sample_rate = rate;
        if let Err(e) = self.check_switch(spec.channels) {
            return Err(ClockError::new(self, e));
        }
        // What the card's stream reported since the last update is reported
        // before the timer's start clears the record.
        let mut events = Vec::new();
        self.fault_poll(&mut events);
        let moved = self.with_device(|session, device| {
            device.driver.move_to(spec, timer, |spec, stopped| {
                session.rerate(spec, stopped, None, None)
            })
        });
        if let Err(switch) = moved {
            // The card again.
            let undone = self.with_device(|session, device| undo(session, device, old));
            if undone.is_err() {
                let graph_rate = self.graph.prepared().sample_rate();
                self.device_mut().driver.settle(graph_rate);
            }
            self.faults_seen = self.device().faults().count();
            self.carried_events.append(&mut events);
            return Err(ClockError::new(self, after_stop(switch, undone)));
        }
        #[cfg(feature = "audio-io")]
        events.extend(
            self.inputs
                .close_all()
                .into_iter()
                .map(|input| SessionEvent::Device(crate::DeviceEvent::InputClosed { input })),
        );
        let audio_host = self.running.audio_host;
        let (mut system, mut handle) = self.with_clock::<SystemClock>(SystemEngine {
            host: None,
            audio_host,
        });
        // What the card's restarts reported and nothing polled yet is not lost
        // with the card.
        events.append(&mut handle.events);
        system.running.host = Some(handle.into_device().driver.into_host());
        system.faults_seen = system.running.host().stream().faults().count();
        let now = system.graph.prepared().sample_rate();
        note_rate_change(&mut events, rate, now);
        system.carried_events.append(&mut events);
        Ok(system)
    }
}
