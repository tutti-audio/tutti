//! [`TuttiDriver`]: the CPAL stream lifecycle.
//!
//! Owns the [`StreamHost`] that runs the [`AudioCallbackState`] and the device
//! it plays on. A device that comes back at another rate needs the graph
//! re-rated, which this driver cannot do (it holds only the graph's audio
//! side): [`restart_with`] runs the host's hook between the stop and the start,
//! and a plain [`restart`] refuses the rate change.
//!
//! [`restart_with`]: TuttiDriver::restart_with
//! [`restart`]: TuttiDriver::restart

use std::sync::Arc;

use tutti_buffers::Float;
use tutti_engine::{
    AudioCallbackState, OutputQuantum, OutputSpec, OutputStream, StartError, Stopped, StreamDriver,
    StreamHost,
};

use crate::Result;
use crate::driver_seam::CpalDriver;
use crate::host::DeviceSelector;
use crate::output::{AudioEngine, Origin};

/// The output device lifecycle a host holds: start, restart on another device
/// or rate, stop.
///
/// Built with [`new`](Self::new) from an opened [`AudioEngine`] and the
/// [`AudioCallbackState`] its callback renders. Reads (spec, running state,
/// faults) go through [`stream`](Self::stream). Every method runs on the control
/// thread and there is no internal lock; the stream is not `Sync`, so keep the
/// driver on one thread.
///
/// See the [crate-level example](crate) for building one.
pub struct TuttiDriver<F: Float = f32> {
    host: StreamHost<F>,
    origin: Origin,
}

// Hand-rolled: the host holds the RT-side graph handles and is not `Debug`
// in full. Forwards it, which carries the configuration a host logs.
impl<F: Float> std::fmt::Debug for TuttiDriver<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuttiDriver")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl<F: Float> TuttiDriver<F>
where
    CpalDriver: StreamDriver<F, Error = crate::DeviceError>,
{
    /// Creates a driver from an opened device and the state its callback will
    /// read. Does not start or stop anything.
    ///
    /// From here on the driver's streams are the only thing that renders
    /// `callback_state`: a host must not also call
    /// [`process_audio`](tutti_engine::process_audio) on a clone of it. A
    /// restart relies on that: its hook reaches the engine from the control
    /// side ([`Stopped::apply_pending`]) because the stream it stopped was the
    /// only renderer.
    pub fn new(audio_engine: AudioEngine, callback_state: Arc<AudioCallbackState<F>>) -> Self {
        let AudioEngine { stream, origin } = audio_engine;
        Self {
            host: StreamHost::new(stream, callback_state),
            origin,
        }
    }

    /// Returns the rate the graph runs at, as far as this driver knows.
    ///
    /// This is the device's rate at [`new`](Self::new), then the
    /// rate of the last restart whose hook returned `Ok`. A plain
    /// [`restart`](Self::restart) compares the new device against it.
    pub fn graph_rate(&self) -> tutti_units::SampleRate {
        self.host.graph_rate()
    }

    /// Restarts the stream on a (possibly different) output device, which
    /// must run at the rate the graph was built at.
    ///
    /// Selects `device` (a [`DeviceSelector`], or an `Option<DeviceId>`
    /// with `None` for the default), stops the current stream, reads the
    /// device's config and starts a new stream on it.
    ///
    /// # Errors
    ///
    /// [`DeviceError::InvalidDevice`](crate::DeviceError::InvalidDevice) when
    /// the driver has never had a device (built from a bare spec), **with the
    /// stream left running**: the refusal comes before the stop.
    ///
    /// [`DeviceError::RateChanged`](crate::DeviceError::RateChanged) when the device now
    /// runs at another rate, **with the stream left stopped**: this driver
    /// holds the graph's audio side only, so it can re-rate nothing, and a
    /// graph at the previous rate would play every node off pitch and off
    /// tempo without a word. Restart through
    /// [`restart_with`](Self::restart_with) to re-rate the graph between
    /// the stop and the start. Otherwise what [`AudioEngine::start`] reports.
    pub fn restart(&mut self, device: impl Into<DeviceSelector>) -> Result<()> {
        self.restart_with(device, refuse_rate_change(self.host.graph_rate()))
    }

    /// Restarts like [`restart`](Self::restart), running `rerate` between the
    /// stop and the start with the config the new stream will run at.
    ///
    /// The hook is where a host re-rates what the driver does not own: the
    /// graph's control side, the transport's rate, the device config it
    /// publishes, while no callback runs: the stream starts only once it has
    /// returned `Ok`, so the first block at the new rate renders the
    /// re-rated graph. A hook that fails leaves the stream **stopped** and
    /// its error returned: nothing plays at a rate the graph was not moved
    /// to. Its `Ok` is the claim that the graph now runs at the spec's rate
    /// ([`graph_rate`](Self::graph_rate)); after a failure the driver keeps
    /// the previous rate and [`spec`](OutputStream::spec), so a plain `restart`
    /// onto the previous device recovers.
    ///
    /// The hook is also handed a [`Stopped`]: the engine, reachable from the
    /// control side for as long as the hook runs and no callback can, so the
    /// host can finish a graph re-prepare before the first block
    /// ([`Stopped::apply_pending`]).
    ///
    /// # Errors
    ///
    /// What resolving or opening the device reports (as `E`), or the hook's
    /// own error.
    pub fn restart_with<E: From<crate::DeviceError>>(
        &mut self,
        device: impl Into<DeviceSelector>,
        rerate: impl FnOnce(&OutputSpec, &Stopped<'_, F>) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), E> {
        self.origin.target_mut()?.1.select(device.into());
        self.restart_selected(rerate)
    }

    /// [`restart_with`](Self::restart_with) opening the device with callbacks
    /// of exactly `quantum` frames. The request stays `quantum` from then on,
    /// so a later restart asks for it too; a restart that fails puts back what
    /// was requested before.
    ///
    /// # Errors
    ///
    /// As [`restart_with`](Self::restart_with), and
    /// [`DeviceError::QuantumUnavailable`](crate::DeviceError::QuantumUnavailable)
    /// when the device does not offer `quantum`.
    pub fn restart_with_quantum<E: From<crate::DeviceError>>(
        &mut self,
        device: impl Into<DeviceSelector>,
        quantum: OutputQuantum,
        rerate: impl FnOnce(&OutputSpec, &Stopped<'_, F>) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), E> {
        let (_, config, _) = self.origin.target_mut()?;
        config.select(device.into());
        let requested = config.quantum.replace(quantum);
        let restarted = self.restart_selected(rerate);
        if restarted.is_err()
            && let Ok((_, config, _)) = self.origin.target_mut()
        {
            config.quantum = requested;
        }
        restarted
    }

    /// The stop, resolve and start of a restart onto the target as it is now.
    fn restart_selected<E: From<crate::DeviceError>>(
        &mut self,
        rerate: impl FnOnce(&OutputSpec, &Stopped<'_, F>) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), E> {
        self.host.stop();
        let mut opened = None;
        self.host
            .restart_with(
                || {
                    let (driver, spec, now) = AudioEngine::resolve_target(self.origin.target()?)?;
                    opened = Some(now);
                    Ok((spec, driver))
                },
                |spec, stopped| rerate(spec, stopped).map_err(Restart::Hook),
            )
            .map_err(Restart::into_user)?;
        if let Some(opened) = opened {
            self.origin.reopened(opened)?;
        }
        Ok(())
    }

    /// Switches the stream to a driver of the caller's choosing at the caller's
    /// `spec`, running `rerate` between the stop and the start, resolving no
    /// device: a restart whose driver is given, not found. **The driver is now
    /// the device**: the driver's errors are the device layer's
    /// ([`DeviceError`](crate::DeviceError)), and the device is recorded as supplied (it
    /// names none). [`move_to`](Self::move_to) is the move to a driver that is
    /// not the device's.
    ///
    /// The device-free counterpart of `restart_with`, as
    /// [`start_with`](Self::start_with) is of `restart`: over a
    /// [`ManualStreamDriver`](tutti_engine::ManualStreamDriver), `spec` stands
    /// for the config the new device reports.
    ///
    /// # Errors
    ///
    /// The hook's error (the stream left stopped), or what the driver's
    /// `open` reports.
    pub fn switch<D: StreamDriver<F>, E: From<crate::DeviceError>>(
        &mut self,
        spec: OutputSpec,
        driver: D,
        rerate: impl FnOnce(&OutputSpec, &Stopped<'_, F>) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), E>
    where
        D::Error: Into<crate::DeviceError>,
    {
        self.host
            .restart_with(
                || Ok((spec, driver)),
                |spec, stopped| rerate(spec, stopped).map_err(Restart::Hook),
            )
            .map_err(Restart::into_user)?;
        self.origin.supply();
        Ok(())
    }

    /// The start [`AudioEngine::start_resolved`] makes: `driver` takes the
    /// host's stream.
    pub(crate) fn take_over<D: StreamDriver<F>, E: From<crate::DeviceError>>(
        mut host: StreamHost<F>,
        spec: OutputSpec,
        driver: D,
        rerate: impl FnOnce(&OutputSpec, &Stopped<'_, F>) -> core::result::Result<(), E>,
        origin: Origin,
    ) -> core::result::Result<Self, (StreamHost<F>, E)>
    where
        D::Error: Into<crate::DeviceError>,
    {
        match host.restart_with(
            || Ok((spec, driver)),
            |spec, stopped| rerate(spec, stopped).map_err(Restart::Hook),
        ) {
            Ok(()) => Ok(Self { host, origin }),
            Err(e) => Err((host, e.into_user())),
        }
    }

    /// Moves the stream, still this driver's, to `driver` at `spec`: a driver
    /// that is not the device's (a timer), whose errors are its own and whose
    /// move leaves the device on record, so a restart can open it again
    /// ([`switch`](Self::switch) makes the driver the device). `rerate` runs between the stop and
    /// the start, as in [`restart_with`](Self::restart_with); a failure leaves
    /// the stream stopped at its old spec. [`into_host`](Self::into_host) hands
    /// the stream on once the device is no longer wanted.
    ///
    /// # Errors
    /// The hook's error, or what the driver's `open` reports, as `E`.
    pub fn move_to<D: StreamDriver<F>, E: From<StartError<D::Error>>>(
        &mut self,
        spec: OutputSpec,
        driver: D,
        rerate: impl FnOnce(&OutputSpec, &Stopped<'_, F>) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), E> {
        self.host.restart_with(|| Ok((spec, driver)), rerate)
    }

    /// [`StreamHost::sync_to_graph`] on this driver's stream.
    ///
    /// # Errors
    /// As [`StreamHost::sync_to_graph`].
    pub fn sync_to_graph(
        &mut self,
        prepared: &tutti_graph::Prepare,
    ) -> core::result::Result<(), tutti_engine::QuantumOutOfRange> {
        self.host.sync_to_graph(prepared)
    }

    /// The stream host, taken out of the driver: the device is no longer this
    /// driver's, and the stream is as it was left.
    #[must_use]
    pub fn into_host(self) -> StreamHost<F> {
        self.host
    }

    /// The stream this driver runs, for reads: [`spec`](OutputStream::spec),
    /// [`is_running`](OutputStream::is_running), [`faults`](OutputStream::faults),
    /// [`latency`](OutputStream::latency).
    #[must_use]
    pub fn stream(&self) -> &OutputStream {
        self.host.stream()
    }

    /// The name of the device the running (or last started) stream opened;
    /// see [`AudioEngine::opened_name`].
    ///
    /// # Errors
    /// [`DeviceError::InvalidDevice`](crate::DeviceError::InvalidDevice) when the device
    /// opened but the host could not name it.
    pub fn opened_name(&self) -> Result<Option<String>> {
        self.origin.opened_name()
    }

    /// Selects a device for the next [`restart`](Self::restart).
    ///
    /// # Errors
    /// [`DeviceError::InvalidDevice`](crate::DeviceError::InvalidDevice) on a
    /// driver built from a bare spec, which has no device to select.
    pub fn select_device(&mut self, sel: DeviceSelector) -> Result<&mut Self> {
        self.origin.target_mut()?.1.select(sel);
        Ok(self)
    }

    /// Stops any running stream and starts a new one on a driver of the
    /// caller's choosing, at the current [`spec`](OutputStream::spec).
    ///
    /// The device path is [`restart`](Self::restart), which builds a real
    /// CPAL stream. This lets a host or a test run the same lifecycle over a
    /// [`ManualStreamDriver`](tutti_engine::ManualStreamDriver) with no device
    /// open.
    ///
    /// # Errors
    /// As [`AudioEngine::start_with`].
    pub fn start_with<D: StreamDriver<F>>(&mut self, driver: D) -> Result<()>
    where
        D::Error: Into<crate::DeviceError>,
    {
        Ok(self.host.start_with(driver)?)
    }

    /// Stops the stream. Idempotent.
    pub fn stop(&mut self) {
        self.host.stop();
    }
}

/// A restart's failure, before it is told apart for the caller: the device
/// layer's own, or the caller's hook's.
enum Restart<E> {
    Device(crate::DeviceError),
    Hook(E),
}

impl<E> From<crate::DeviceError> for Restart<E> {
    fn from(err: crate::DeviceError) -> Self {
        Self::Device(err)
    }
}

impl<E, D: Into<crate::DeviceError> + std::error::Error + 'static> From<StartError<D>>
    for Restart<E>
{
    fn from(err: StartError<D>) -> Self {
        Self::Device(err.into())
    }
}

impl<E: From<crate::DeviceError>> Restart<E> {
    fn into_user(self) -> E {
        match self {
            Self::Device(err) => E::from(err),
            Self::Hook(err) => err,
        }
    }
}

/// The hook a plain [`TuttiDriver::restart`] runs: the new device must run
/// at `graph`, the rate the graph was built at, or the restart stops there
/// with [`DeviceError::RateChanged`](crate::DeviceError::RateChanged).
fn refuse_rate_change<F: Float>(
    graph: tutti_units::SampleRate,
) -> impl FnOnce(&OutputSpec, &Stopped<'_, F>) -> Result<()> {
    move |spec, _| {
        if spec.sample_rate == graph {
            Ok(())
        } else {
            Err(crate::DeviceError::RateChanged {
                device: spec.sample_rate,
                graph,
            })
        }
    }
}

#[cfg(test)]
mod tests;
