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
//!   transport, the heard tap and the load meter. The transport keeps
//!   its place: a rolling one resumes at the beat the last block ended on, so
//!   the switch costs wall-clock time and no musical time.
//! - **Takes in flight carry over:** a take's pump names nothing of the clock,
//!   so a take still running when the switch begins reports to the converted
//!   session, once.
//! - **Captures end** when the session leaves the sound card: each input the
//!   session held closes and is reported as
//!   [`DeviceEvent::InputClosed`](crate::DeviceEvent) (feature `capture`).
//! - **A rate that differs** is [`SessionEvent::RateChanged`].
//! - **A failure gives the session back, running on its old clock,** with one
//!   exception. Everything that can be known is refused before the old stream
//!   stops (the graph root and the output map on the new channels, a re-prepare
//!   that cannot run, a run waiting), and a device that will not open fails
//!   before anything stops. A failure after the stop (the new stream will not
//!   open, the re-prepare refuses) is undone by starting the old clock again at
//!   its old rate and block. Only when that fails too (a graph the re-prepare
//!   poisoned; on a card, a device that is gone) is the session returned
//!   **stopped**, and the error then is [`Error::SwitchNotUndone`], which names
//!   both failures. The host then reports what the graph is prepared at, and
//!   the session is restarted on a card (`Session::restart`) or switched
//!   again (`Session::into_device` on the system clock) once what broke is
//!   fixed.

use tutti_buffers::ChannelLayout;
use tutti_cpal::{AudioEngine, DeviceHost, DeviceSelector, OutputConfig, TuttiDriver};
use tutti_engine::{OutputQuantum, OutputSpec, StreamDriver, TimerDriver};
use tutti_units::SampleRate;

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
/// handed to the inputs (feature `capture`).
#[cfg(feature = "capture")]
fn bind_inputs<K: crate::AppKey, F: crate::SessionFloat>(
    live: &mut LiveSession<K, F>,
    driver: &TuttiDriver<F>,
) -> std::sync::Arc<tutti_engine::StreamLatency> {
    live.inputs.set_output_latency(driver.stream().latency());
    live.inputs.latency()
}

#[cfg(not(feature = "capture"))]
fn bind_inputs<K: crate::AppKey, F: crate::SessionFloat>(
    _: &mut LiveSession<K, F>,
    _: &TuttiDriver<F>,
) -> std::sync::Arc<tutti_engine::StreamLatency> {
    std::sync::Arc::default()
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// What a switch refuses before anything stops, so a refusal
    /// leaves the session as it was. The root and the output map in force have
    /// to be placeable on the new channels, and the graph has to be able to
    /// re-prepare.
    fn check_switch(&self, channels: ChannelLayout) -> Result<()> {
        self.check_device(channels, None)?;
        self.check_restart(None)
    }
}

impl<K: crate::AppKey, F: crate::SessionFloat> Session<K, SystemClock<F>> {
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
    /// be found or opened (a chosen
    /// [`sample_format`](OutputConfig::sample_format) the device lacks is
    /// [`tutti_cpal::DeviceError::OutputFormatUnavailable`]), [`Error::Unplaceable`], [`Error::MapUnplaceable`],
    /// [`Error::Reprepare`] when the graph cannot
    /// take the device **before the timer stops**, the re-prepare's or the
    /// card's own failure after it, and [`Error::SwitchNotUndone`] when the
    /// timer could not be started again.
    pub fn into_device(
        self,
        device: impl Into<OutputConfig>,
    ) -> std::result::Result<LiveSession<K, F>, ClockError<Self>> {
        let config = device.into();
        let device = config.device().clone();
        let executor = self.background.executor().clone();
        let opened = DeviceHost::open(self.running.audio_host).and_then(|host| {
            let engine = AudioEngine::open(&executor, host, config)?;
            let resolved = engine.resolve_device()?;
            Ok((engine, resolved))
        });
        let (engine, resolved) = match opened {
            Ok(found) => found,
            Err(e) => return Err(ClockError::new(self, e.into())),
        };
        let channels = resolved.spec().channels;
        self.switch_to_device(channels, device, move |host, session| {
            engine
                .start_resolved(host, resolved, |spec, stopped| {
                    session.rerate(spec, stopped, None, None)
                })
                .map_err(Box::new)
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
    pub fn into_device_on<D: StreamDriver<F>>(
        self,
        spec: OutputSpec,
        driver: D,
    ) -> std::result::Result<LiveSession<K, F>, ClockError<Self>>
    where
        D::Error: Into<tutti_cpal::DeviceError>,
    {
        let channels = spec.channels;
        self.switch_to_device(channels, DeviceSelector::Default, move |host, session| {
            AudioEngine::from_spec(spec)
                .start_on(host, driver, |spec, stopped| {
                    session.rerate(spec, stopped, None, None)
                })
                .map_err(Box::new)
        })
    }

    /// The switch to a card whose channels are `channels`; `start` starts its
    /// stream on the timer's host, running the re-prepare in between.
    fn switch_to_device(
        mut self,
        channels: ChannelLayout,
        chosen: DeviceSelector,
        start: impl FnOnce(
            tutti_engine::StreamHost<F>,
            &mut Self,
        ) -> std::result::Result<
            TuttiDriver<F>,
            Box<(tutti_engine::StreamHost<F>, Error)>,
        >,
    ) -> std::result::Result<LiveSession<K, F>, ClockError<Self>> {
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
            Err(failed) => {
                let (host, switch) = *failed;
                self.running.host = Some(host);
                let undone = self.back_to_the_timer(old);
                self.reports.carry(&mut unpolled);
                Err(ClockError::new(self, after_stop(switch, undone)))
            }
        }
    }

    /// A switch failed after the timer stopped: the timer starts again at the
    /// spec it had, the graph re-prepared at its rate and block. If that fails
    /// too the session stays stopped, and the host says what the graph is
    /// prepared at.
    fn back_to_the_timer(&mut self, old: OutputSpec) -> Result<()> {
        let timer = TimerDriver::new(self.background.executor());
        let mut host = self.running.take_host();
        let restored = host.restart_with(
            || Ok::<_, Error>((old, timer)),
            |spec, stopped| self.rerate(spec, stopped, None, None),
        );
        if restored.is_err() {
            let synced = host.sync_to_graph(&self.graph.prepared());
            debug_assert!(synced.is_ok(), "the graph's quantum came from the spec's");
        }
        self.reports
            .faults_restarted(host.stream().faults().count());
        self.running.host = Some(host);
        restored
    }

    /// The session on the sound card's clock, now the card's stream runs.
    fn on_device(
        self,
        driver: TuttiDriver<F>,
        chosen: DeviceSelector,
        previous: SampleRate,
        mut events: Vec<SessionEvent>,
    ) -> LiveSession<K, F> {
        let audio_host = self.running.audio_host;
        let (mut live, _timer) =
            self.with_clock::<DeviceClock<F>>(DeviceHandle::vacant(audio_host));
        let input_latency = bind_inputs(&mut live, &driver);
        let device = Device::<F>::new(driver, chosen, input_latency);
        live.reports.faults_restarted(device.faults().count());
        live.running.fill(device);
        let rate = live.device().graph_rate();
        note_rate_change(&mut events, previous, rate);
        live.reports.carry(&mut events);
        live
    }
}

impl<F: crate::SessionFloat> SystemEngine<F> {
    fn take_host(&mut self) -> tutti_engine::StreamHost<F> {
        self.host
            .take()
            .expect("the stream host is in its slot except while a clock switch runs")
    }
}

impl<K: crate::AppKey, F: crate::SessionFloat> Session<K, DeviceClock<F>> {
    /// Moves the session onto an OS timer thread: the stream stops, the graph
    /// keeps the rate it ran at and takes the graph's block as the timer's,
    /// and the session has no audio in or out. The captures the session held
    /// end ([`DeviceEvent::InputClosed`](crate::DeviceEvent), feature
    /// `capture`); the device watch ends with the device.
    ///
    /// **A failure gives the session back running on the card**: before the
    /// stream stops when the graph cannot re-prepare, and after it (the timer thread would not start,
    /// or the re-prepare refused) by opening the card's device again, which
    /// resolves it afresh: its config is what the host reports then. The
    /// session comes back stopped only with [`Error::SwitchNotUndone`].
    ///
    /// A panic during the switch (a node's `prepare`, the OS) unwinds out of
    /// this call and drops the session: it is not returned as a [`ClockError`].
    ///
    /// # Errors
    ///
    /// A [`ClockError`] whose error is [`Error::Reprepare`] before the stream stops, the re-prepare's or the
    /// timer's own failure ([`Error::Clock`]) after it, and
    /// [`Error::SwitchNotUndone`] when the card could not be started again.
    pub fn into_system_clock(self) -> std::result::Result<SystemSession<K, F>, ClockError<Self>> {
        let timer = TimerDriver::new(self.background.executor());
        self.into_timer(timer, |session, device, _| {
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
    pub fn into_system_clock_on<B: StreamDriver<F>>(
        self,
        timer: impl StreamDriver<F, Error = tutti_engine::TimerError>,
        back: B,
    ) -> std::result::Result<SystemSession<K, F>, ClockError<Self>>
    where
        B::Error: Into<tutti_cpal::DeviceError>,
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
        undo: impl FnOnce(&mut Self, &mut Device<F>, OutputSpec) -> Result<()>,
    ) -> std::result::Result<SystemSession<K, F>, ClockError<Self>>
    where
        D: StreamDriver<F, Error = tutti_engine::TimerError>,
    {
        let rate = self.device().graph_rate();
        let old = self.device().spec().clone();
        let quantum = match OutputQuantum::try_from(self.graph.prepared().max_block()) {
            Ok(quantum) => quantum,
            Err(e) => return Err(ClockError::new(self, e.into())),
        };
        let mut spec = old.clone().with_quantum(quantum);
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
                let prepared = self.graph.prepared();
                let synced = self.device_mut().driver.sync_to_graph(&prepared);
                debug_assert!(synced.is_ok(), "the graph's quantum came from the spec's");
            }
            self.reports
                .faults_restarted(self.device().faults().count());
            self.reports.carry(&mut events);
            return Err(ClockError::new(self, after_stop(switch, undone)));
        }
        #[cfg(feature = "capture")]
        events.extend(self.inputs.close_all().into_iter().map(|input| {
            SessionEvent::Device(crate::DeviceEvent::InputClosed {
                input: input.node().erase(),
            })
        }));
        let audio_host = self.running.audio_host;
        let (mut system, mut handle) = self.with_clock::<SystemClock<F>>(SystemEngine {
            host: None,
            audio_host,
        });
        // What the card's restarts reported and nothing polled yet is not lost
        // with the card.
        events.append(&mut handle.events);
        system.running.host = Some(handle.into_device().driver.into_host());
        system
            .reports
            .faults_restarted(system.running.host().stream().faults().count());
        let now = system.graph.prepared().sample_rate();
        note_rate_change(&mut events, rate, now);
        system.reports.carry(&mut events);
        Ok(system)
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use tutti_buffers::MaxBlock;
    use tutti_engine::{ManualStreamDriver, OutputBlock, RunningStream, StreamFaults, TimerError};
    use tutti_units::Samples;

    use super::*;

    /// The handle of a stream that never opens.
    struct Never;

    impl RunningStream for Never {
        fn stop(self: Box<Self>) {}
    }

    /// A card whose stream will not open.
    struct UnopenableCard;

    impl StreamDriver for UnopenableCard {
        type Running = Never;
        type Error = tutti_cpal::DeviceError;

        fn open(
            self,
            _: &OutputSpec,
            _: OutputBlock,
            _: std::sync::Arc<StreamFaults>,
        ) -> std::result::Result<Never, Self::Error> {
            Err(tutti_cpal::DeviceError::InvalidDevice("no card".into()))
        }
    }

    /// A timer the OS refuses.
    struct UnstartableTimer;

    impl StreamDriver for UnstartableTimer {
        type Running = Never;
        type Error = TimerError;

        fn open(
            self,
            _: &OutputSpec,
            _: OutputBlock,
            _: std::sync::Arc<StreamFaults>,
        ) -> std::result::Result<Never, Self::Error> {
            Err(TimerError::Thread(std::io::Error::other("no thread")))
        }
    }

    /// A node that cannot run above 90 kHz: its `prepare` panics there, which is
    /// how a re-prepare poisons an editor.
    struct Grenade;

    impl tutti_graph::Node for Grenade {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
        }

        fn prepare(&mut self, prepare: &tutti_graph::Prepare) {
            assert!(prepare.sample_rate().get() < 90_000.0, "no such rate");
        }

        fn process(
            &mut self,
            _: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            io.output(0).fill(0.0);
            tutti_graph::Status::Modified
        }

        fn reset(&mut self) {}
    }

    fn spec(rate: SampleRate, quantum: usize) -> OutputSpec {
        OutputSpec::new(
            rate,
            ChannelLayout::STEREO,
            tutti_buffers::SampleFormat::F32,
        )
        .with_quantum(OutputQuantum::of(quantum))
    }

    /// **A card that cannot be started again after a timer that would not
    /// start leaves the session stopped, its stream reporting what the graph is
    /// prepared at**: the error names both failures, and the stream's rate and
    /// callback size are the prepared graph's.
    #[test]
    fn a_double_failure_on_the_card_leaves_it_stopped_agreeing_with_the_graph() {
        let (driver, _stream) = ManualStreamDriver::new();
        let live = Session::builder()
            .device(tutti_cpal::DeviceSelector::Default)
            .build_device_on(spec(SampleRate::SR_48K, 256), driver)
            .expect("builds with no device");
        let err = live
            .into_system_clock_on(UnstartableTimer, UnopenableCard)
            .expect_err("neither starts");
        assert!(
            matches!(err.error().error(), Error::SwitchNotUndone { .. }),
            "{}",
            err.error()
        );
        let live = err.into_session();
        assert!(!live.device().is_running(), "stopped");
        let prepared = live.graph.prepared();
        assert_eq!(live.device().spec().sample_rate, prepared.sample_rate());
        assert_eq!(
            live.device().spec().quantum.map(OutputQuantum::max_block),
            prepared.quantum()
        );
        assert_eq!(live.device().graph_rate(), prepared.sample_rate());
    }

    /// **A poisoned graph on the timer leaves it stopped, its stream reporting
    /// what the graph is prepared at**: the rate and the block.
    #[test]
    fn a_poisoned_graph_on_the_timer_leaves_it_stopped_agreeing_with_the_graph() {
        let mut s = Session::builder()
            .sample_rate(SampleRate::SR_48K)
            .max_block(MaxBlock::of(Samples::new(480)))
            .system_clock()
            .build()
            .expect("builds");
        s.graph().add(tutti_graph::Unforkable(Grenade));
        let _ = s.update();
        let (driver, _stream) = ManualStreamDriver::new();
        let err = s
            .into_device_on(spec(SampleRate::of(96_000.0), 256), driver)
            .expect_err("the graph is poisoned by the re-prepare");
        assert!(
            matches!(err.error().error(), Error::SwitchNotUndone { .. }),
            "{}",
            err.error()
        );
        let s = err.into_session();
        let prepared = s.graph.prepared();
        assert_eq!(s.stream_spec().sample_rate, prepared.sample_rate());
        assert_eq!(
            s.stream_spec().quantum.map(OutputQuantum::max_block),
            prepared.quantum()
        );
    }
}
