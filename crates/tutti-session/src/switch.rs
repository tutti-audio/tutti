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
//!   land after the switch. The one thing that cannot follow is a run asked
//!   with [`Session::when_loaded`](crate::Session::when_loaded): its closure
//!   takes the session of the clock it was asked of. Those whose entry has
//!   loaded run first, on the old clock; the rest are reported once as
//!   [`PoolEvent::RunDropped`](crate::PoolEvent::RunDropped).
//! - **Captures end** when the session leaves the sound card: each input the
//!   session held closes and is reported as
//!   [`DeviceEvent::InputClosed`](crate::DeviceEvent) (feature `audio-io`).
//! - **A rate that differs** is [`SessionEvent::RateChanged`].
//! - **A failure gives the session back, running on its old clock,** unless the
//!   graph is poisoned (a stream that will not open after the stop is undone
//!   by starting the old clock again at its old spec): everything checkable runs before the old stream stops
//!   (the graph root against the new channel count, and the restart's own
//!   refusals), and a device that will not open fails before anything stops.
//!   After the stop, the only failure is a re-prepare that poisons the graph;
//!   the session then comes back stopped and says so
//!   ([`Error::Reprepare`]).

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
/// [`into_session`](Self::into_session) gives the session back. It is running
/// on its old clock, unless the error is a poisoned graph's
/// ([`Error::Reprepare`] after the stream stopped), where it is stopped.
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

impl<C: crate::Clock> Session<C> {
    /// The refusals a switch can make before anything stops: the graph's root
    /// has to be placeable on the new channel count, and the graph has to be
    /// able to re-prepare (not between its halves, not poisoned).
    fn check_switch(&self, channels: ChannelLayout) -> Result<()> {
        self.check_device(channels, None)?;
        self.check_restart(None)
    }
}

impl Session<SystemClock> {
    /// Moves the session onto the sound card `device` selects: the device is
    /// opened first, and a device that will not open returns the session
    /// running, on the timer, with the error. The graph is re-prepared at the
    /// device's rate and callback size, as a device restart does.
    ///
    /// # Errors
    ///
    /// A [`ClockError`] whose error is [`Error::Device`] when the device cannot
    /// be found or opened, [`Error::Unplaceable`] or [`Error::Reprepare`] when the
    /// graph cannot take the device (a declared root it cannot place, a
    /// re-prepare already between its halves) **before the timer stops**, and
    /// otherwise the failure of the re-prepare or of the card's stream, with
    /// the timer started again (the session stopped only if a re-prepare
    /// poisoned the graph).
    ///
    /// A panic during the switch (a node's `prepare`, the OS) unwinds out of
    /// this call and drops the session: it is not returned as a [`ClockError`].
    pub fn into_device(
        mut self,
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
        if let Err(e) = self.check_switch(resolved.spec().channels) {
            return Err(ClockError::new(self, e));
        }
        let previous = self.graph.prepared().sample_rate();
        // What the timer's stream reported since the last update is reported
        // before the card's stream clears the record.
        let mut unpolled = Vec::new();
        self.fault_poll(&mut unpolled);
        let old = self.running.host().stream().spec().clone();
        let host = self.running.take_host();
        match TuttiDriver::start_resolved(host, engine, resolved, |spec, stopped| {
            self.rerate(spec, stopped, None, None)
        }) {
            Ok(driver) => Ok(self.on_device(driver, device, previous, unpolled)),
            Err((host, e)) => {
                self.running.host = Some(host);
                self.back_to_the_timer(old);
                self.carried_events.append(&mut unpolled);
                Err(ClockError::new(self, e))
            }
        }
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
        mut self,
        spec: OutputSpec,
        driver: D,
    ) -> std::result::Result<LiveSession, ClockError<Self>>
    where
        D::Error: Into<tutti_cpal::Error>,
    {
        if let Err(e) = self.check_switch(spec.channels) {
            return Err(ClockError::new(self, e));
        }
        let previous = self.graph.prepared().sample_rate();
        let mut unpolled = Vec::new();
        self.fault_poll(&mut unpolled);
        let old = self.running.host().stream().spec().clone();
        let host = self.running.take_host();
        match TuttiDriver::start_on(host, spec, driver, |spec, stopped| {
            self.rerate(spec, stopped, None, None)
        }) {
            Ok(driver) => Ok(self.on_device(driver, DeviceSelector::default(), previous, unpolled)),
            Err((host, e)) => {
                self.running.host = Some(host);
                self.back_to_the_timer(old);
                self.carried_events.append(&mut unpolled);
                Err(ClockError::new(self, e))
            }
        }
    }

    /// A switch failed after the timer stopped: the timer starts again at the
    /// spec it had, the graph re-prepared at its rate and block. If that fails
    /// too (a graph that no longer compiles), the session stays stopped.
    fn back_to_the_timer(&mut self, old: OutputSpec) {
        let mut host = self.running.take_host();
        let restored = host.restart_with(
            || Ok::<_, Error>((old, TimerDriver::new())),
            |spec, stopped| self.rerate(spec, stopped, None, None),
        );
        self.faults_seen = host.stream().faults().count();
        self.running.host = Some(host);
        let _ = restored;
    }

    /// The session on the sound card's clock, now the card's stream runs.
    fn on_device(
        self,
        driver: TuttiDriver,
        chosen: DeviceSelector,
        previous: SampleRate,
        unpolled: Vec<SessionEvent>,
    ) -> LiveSession {
        let mut events = unpolled;
        let audio_host = self.running.audio_host;
        let (mut live, _timer) =
            self.with_clock::<DeviceClock>(DeviceHandle::vacant(audio_host), &mut events);
        #[cfg(feature = "audio-io")]
        live.inputs.set_output_latency(driver.stream().latency());
        #[cfg(feature = "audio-io")]
        let input_latency = live.inputs.latency();
        #[cfg(not(feature = "audio-io"))]
        let input_latency = std::sync::Arc::default();
        let device = Device::new(driver, chosen, input_latency);
        live.faults_seen = device.faults().count();
        live.running.fill(device);
        let rate = live.device().graph_rate();
        if rate != previous {
            events.push(SessionEvent::RateChanged {
                from: previous,
                to: rate,
            });
        }
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
    /// stream stops when the graph cannot re-prepare, and after it (the timer
    /// thread would not start, or the graph would not re-prepare) by starting
    /// the card again at the spec it had. Only when that fails too is the
    /// session returned stopped.
    ///
    /// A panic during the switch (a node's `prepare`, the OS) unwinds out of
    /// this call and drops the session: it is not returned as a [`ClockError`].
    ///
    /// # Errors
    ///
    /// A [`ClockError`] whose error is [`Error::Reprepare`] before the stream
    /// stops when the graph cannot re-prepare, and otherwise the re-prepare's
    /// or the timer's own failure ([`Error::Clock`]).
    pub fn into_system_clock(self) -> std::result::Result<SystemSession, ClockError<Self>> {
        self.into_timer(TimerDriver::new(), |session, device, old| {
            let chosen = device.chosen.clone();
            device
                .driver
                .restart_with(chosen, |spec, stopped| {
                    session.rerate(spec, stopped, None, None)
                })
                .map_err(|_: Error| ())?;
            let _ = old;
            Ok(())
        })
    }

    /// [`into_system_clock`](Self::into_system_clock) on a timer driver of the
    /// caller's choosing, and `back` the driver the card is started again on
    /// should that fail: the device-free switch, for a test. Behind `testing`.
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
            device
                .driver
                .switch(old, back, |spec, stopped| {
                    session.rerate(spec, stopped, None, None)
                })
                .map_err(|_: Error| ())
        })
    }

    /// The switch to a timer driven by `timer`; `undo` starts the card again.
    fn into_timer<D>(
        mut self,
        timer: D,
        undo: impl FnOnce(&mut Self, &mut Device, OutputSpec) -> std::result::Result<(), ()>,
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
            device.driver.host_mut().restart_with(
                || Ok::<_, Error>((spec, timer)),
                |spec, stopped| session.rerate(spec, stopped, None, None),
            )
        });
        if let Err(e) = moved {
            // The card again, at its old rate and block.
            let _ = self.with_device(|session, device| undo(session, device, old));
            self.faults_seen = self.device().faults().count();
            self.carried_events.append(&mut events);
            return Err(ClockError::new(self, e));
        }
        #[cfg(feature = "audio-io")]
        events.extend(
            self.inputs
                .close_all()
                .into_iter()
                .map(|input| SessionEvent::Device(crate::DeviceEvent::InputClosed { input })),
        );
        let audio_host = self.running.audio_host;
        let (mut system, handle) = self.with_clock::<SystemClock>(
            SystemEngine {
                host: None,
                audio_host,
            },
            &mut events,
        );
        // What the card's restarts reported and nothing polled yet is not lost
        // with the card.
        let mut handle = handle;
        events.append(&mut handle.events);
        system.running.host = Some(handle.into_device().driver.into_host());
        system.faults_seen = system.running.host().stream().faults().count();
        let now = system.graph.prepared().sample_rate();
        if now != rate {
            events.push(SessionEvent::RateChanged {
                from: rate,
                to: now,
            });
        }
        system.carried_events.append(&mut events);
        Ok(system)
    }
}
