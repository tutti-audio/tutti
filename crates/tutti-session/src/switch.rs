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
//! - **Jobs in flight do not:** a job's result lands on the clock it was
//!   submitted on, so each job still running when the switch begins lands as
//!   [`JobFailed::Dropped`](crate::JobFailed::Dropped), reported once by the
//!   events its lander raises. Pool entries are not jobs and are untouched.
//! - **Captures end** when the session leaves the sound card: each input the
//!   session held closes and is reported as
//!   [`DeviceEvent::InputClosed`](crate::DeviceEvent) (feature `audio-io`).
//! - **A rate that differs** is [`SessionEvent::RateChanged`].
//! - **A failure gives the session back, running on its old clock,** unless the
//!   graph is poisoned: everything checkable runs before the old stream stops
//!   (the graph root against the new channel count, and the restart's own
//!   refusals), and a device that will not open fails before anything stops.
//!   After the stop, the only failure is a re-prepare that poisons the graph;
//!   the session then comes back stopped and says so
//!   ([`Error::Reprepare`]).

use tutti_cpal::{AudioEngine, DeviceHost, DeviceSelector, TuttiDriver};
use tutti_engine::TimerDriver;
#[cfg(feature = "testing")]
use tutti_engine::{OutputSpec, StreamDriver};
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
    /// otherwise the re-prepare's own failure with the session stopped (see the
    /// module docs).
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
        let host = self.running.take_host();
        match TuttiDriver::start_resolved(host, engine, resolved, |spec, stopped| {
            self.rerate(spec, stopped, None, None)
        }) {
            Ok(driver) => Ok(self.on_device(driver, device, previous)),
            Err((host, e)) => {
                self.running.host = Some(host);
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
        let host = self.running.take_host();
        match TuttiDriver::start_on(host, spec, driver, |spec, stopped| {
            self.rerate(spec, stopped, None, None)
        }) {
            Ok(driver) => Ok(self.on_device(driver, DeviceSelector::default(), previous)),
            Err((host, e)) => {
                self.running.host = Some(host);
                Err(ClockError::new(self, e))
            }
        }
    }

    /// The session on the sound card's clock, now the card's stream runs.
    fn on_device(
        self,
        driver: TuttiDriver,
        chosen: DeviceSelector,
        previous: SampleRate,
    ) -> LiveSession {
        let mut events = Vec::new();
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
    /// # Errors
    ///
    /// A [`ClockError`] whose error is [`Error::Reprepare`] before the stream
    /// stops when the graph cannot re-prepare, and otherwise the re-prepare's
    /// own failure with the session stopped (see the module docs).
    pub fn into_system_clock(mut self) -> std::result::Result<SystemSession, ClockError<Self>> {
        let rate = self.device().graph_rate();
        let mut spec = self
            .device()
            .spec()
            .clone()
            .with_quantum(self.graph.prepared().max_block());
        spec.sample_rate = rate;
        if let Err(e) = self.check_switch(spec.channels) {
            return Err(ClockError::new(self, e));
        }
        let moved = self.with_device(|session, device| {
            device.driver.host_mut().restart_with(
                || Ok::<_, Error>((spec, TimerDriver::new())),
                |spec, stopped| session.rerate(spec, stopped, None, None),
            )
        });
        if let Err(e) = moved {
            return Err(ClockError::new(self, e));
        }
        let mut events = Vec::new();
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
