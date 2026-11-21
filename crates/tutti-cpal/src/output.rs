//! CPAL audio I/O: the output device and its stream lifecycle.
//!
//! The callback itself (rendering the graph, `process_audio`, the fault and
//! latency cells) is `tutti-engine`'s; this module resolves a device and holds
//! the [`OutputStream`] that runs on it.

use cpal::traits::DeviceTrait;
use std::sync::Arc;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, SampleFormat};
use tutti_engine::{
    AudioCallbackState, OutputQuantum, OutputSpec, OutputStream, StreamDriver, StreamFaults,
    StreamLatency,
};
use tutti_platform::Executor;
use tutti_units::SampleRate;

use crate::driver_seam::{CpalDriver, choose_output_config, spec_from_supported};
use crate::error::{DeviceError, Result};
use crate::host::{AudioHost, DeviceHost, DeviceSelector, device_name};
use crate::roster::{DeviceDirection, DeviceId};

/// What [`AudioEngine`] opens: the output device, its sample format and its
/// callback size.
/// [`new`](Self::new) names the device at its default format; `Default` is the
/// host's default device; a [`DeviceSelector`], a [`DeviceId`] or an
/// `Option<DeviceId>` converts to the config that names it. The choice survives
/// a restart.
///
/// ```
/// use tutti_buffers::SampleFormat;
/// use tutti_cpal::{DeviceSelector, OutputConfig};
/// use tutti_engine::OutputQuantum;
///
/// let config = OutputConfig::new(DeviceSelector::named("Interface"))
///     .sample_format(SampleFormat::I16)
///     .quantum(OutputQuantum::of(128));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputConfig {
    pub(crate) device: DeviceSelector,
    pub(crate) sample_format: Option<SampleFormat>,
    pub(crate) quantum: Option<OutputQuantum>,
}

impl Default for OutputConfig {
    /// The host's default device, at its default sample format.
    fn default() -> Self {
        Self::new(DeviceSelector::Default)
    }
}

impl OutputConfig {
    /// The device `device` names, at its default sample format.
    #[must_use]
    pub fn new(device: impl Into<DeviceSelector>) -> Self {
        Self {
            device: device.into(),
            sample_format: None,
            quantum: None,
        }
    }

    /// Names `device` instead; the chosen sample format stays, so a restart
    /// onto another device is asked for the same format.
    pub(crate) fn select(&mut self, device: DeviceSelector) {
        self.device = device;
    }

    /// The device it names.
    #[must_use]
    pub fn device(&self) -> &DeviceSelector {
        &self.device
    }

    /// Opens the device at `format`, at its default rate and width. An
    /// unsigned device format opens as the signed format of its width, and a
    /// format the device does not offer is [`DeviceError::OutputFormatUnavailable`],
    /// never another format.
    #[must_use]
    pub fn sample_format(mut self, format: SampleFormat) -> Self {
        self.sample_format = Some(format);
        self
    }
}

impl OutputConfig {
    /// Opens the device with callbacks of exactly `quantum` frames. A size
    /// outside the range the device reports is
    /// [`DeviceError::QuantumUnavailable`] when the stream is opened, never
    /// clamped into it. Without this the stream asks for
    /// [`PREFERRED_QUANTUM`](crate::PREFERRED_QUANTUM) frames, clamped into the
    /// range, and a device that reports no range opens with its own buffer.
    #[must_use]
    pub fn quantum(mut self, quantum: OutputQuantum) -> Self {
        self.quantum = Some(quantum);
        self
    }
}

impl From<DeviceSelector> for OutputConfig {
    /// The device `device` names, at its default sample format.
    fn from(device: DeviceSelector) -> Self {
        Self::new(device)
    }
}

impl From<DeviceId> for OutputConfig {
    fn from(id: DeviceId) -> Self {
        DeviceSelector::from(id).into()
    }
}

impl From<Option<DeviceId>> for OutputConfig {
    /// `None` is the default device.
    fn from(id: Option<DeviceId>) -> Self {
        DeviceSelector::from(id).into()
    }
}

/// The config `device` opens at: its default, or the one that offers `wanted`.
fn output_config(
    device: &cpal::Device,
    wanted: Option<SampleFormat>,
) -> Result<cpal::SupportedStreamConfig> {
    let name = device_name(device).unwrap_or_default();
    // The device is asked what it offers only when a format was chosen: a
    // default open reads one config, as it always has.
    let supported: Vec<_> = match wanted {
        Some(_) => device
            .supported_output_configs()
            .map_err(|e| {
                DeviceError::InvalidDevice(format!("cannot query {name:?} output configs: {e}"))
            })?
            .collect(),
        None => Vec::new(),
    };
    choose_output_config(
        supported.into_iter(),
        device
            .default_output_config()
            .map_err(DeviceError::DeviceNotAvailable)?,
        wanted,
        &name,
    )
}

/// What an engine built from a bare spec answers a call that needs a device.
pub(crate) fn no_device() -> DeviceError {
    DeviceError::InvalidDevice(
        "this engine was built from a bare spec and has no device to open; \
         use `start_with` and supply a driver"
            .into(),
    )
}

/// An output device, its stream configuration, and the stream while it runs.
///
/// [`TuttiDriver`](crate::TuttiDriver) wraps one and is what a host normally
/// holds; use `AudioEngine` directly to open the device before the graph
/// exists (its [`sample_rate`](Self::sample_rate) is the rate to build the
/// graph at). Every method runs on the control thread.
///
/// The stream runs until [`stop`](Self::stop) or until the engine is dropped.
/// Backend errors go to [`faults`](Self::faults).
pub struct AudioEngine {
    pub(crate) stream: OutputStream,
    /// `None` for an engine built from a bare spec — no host, no device, so
    /// nothing to re-resolve on start. That is what makes a device-free
    /// lifecycle test possible.
    pub(crate) target: Option<Target>,
    /// What the stream last started is running on, kept so that naming it
    /// never enumerates the host.
    pub(crate) opened: Opened,
}

/// The device an engine resolves, with what opens its stream: its host, its config and
/// the executor whose dedicated thread owns the stream.
pub(crate) type Target = (DeviceHost, OutputConfig, Executor);

/// A device found and its config read, not yet opened as a stream
/// ([`AudioEngine::resolve_device`]): what
/// [`AudioEngine::start_resolved`] starts.
pub struct Resolved {
    spec: OutputSpec,
    pub(crate) driver: CpalDriver,
    pub(crate) opened: Opened,
}

impl Resolved {
    /// The config the stream will run at.
    #[must_use]
    pub fn spec(&self) -> &OutputSpec {
        &self.spec
    }

    pub(crate) fn into_parts(self) -> (OutputSpec, CpalDriver, Opened) {
        (self.spec, self.driver, self.opened)
    }
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

/// The device a stream runs on, as the engine learned it when it opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Opened {
    /// The caller's own driver: no device, by design.
    Supplied,
    /// A device, and its name.
    Named(String),
    /// A device the host could not name.
    Unnamed(String),
}

impl Opened {
    pub(crate) fn of(device: &cpal::Device) -> Self {
        device_name(device).map_or_else(|e| Self::Unnamed(e.to_string()), Self::Named)
    }

    /// The name, `Ok(None)` for a supplied driver, or an error for a device the
    /// host could not name.
    pub(crate) fn name(&self) -> Result<Option<String>> {
        match self {
            Self::Supplied => Ok(None),
            Self::Named(name) => Ok(Some(name.clone())),
            Self::Unnamed(why) => Err(DeviceError::InvalidDevice(format!(
                "the opened device could not be named: {why}"
            ))),
        }
    }
}

// Hand-rolled: the device handles are not `Debug`. Forwards the stream, which
// carries the configuration a host would want in a log line.
impl std::fmt::Debug for AudioEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEngine")
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

impl AudioEngine {
    /// Opens a device on the default host and reads its output config,
    /// without starting a stream. The stream runs on a dedicated thread of
    /// `executor`.
    ///
    /// # Errors
    /// As [`open`](Self::open).
    pub fn new(executor: &Executor, config: impl Into<OutputConfig>) -> Result<Self> {
        Self::open(executor, DeviceHost::open(AudioHost::Default)?, config)
    }

    /// Opens a device on the given host and reads its output config (the
    /// device's default, or the one that offers the chosen
    /// [`sample_format`](OutputConfig::sample_format)), without starting a
    /// stream. The stream runs on a dedicated thread of `executor`.
    ///
    /// # Errors
    /// As [`new`](Self::new), plus whatever resolving the device on `host`
    /// reports, and [`DeviceError::OutputFormatUnavailable`] for a chosen format the
    /// device does not offer.
    pub fn open(
        executor: &Executor,
        host: DeviceHost,
        config: impl Into<OutputConfig>,
    ) -> Result<Self> {
        let config = config.into();
        let device = host.device(DeviceDirection::Output, &config.device)?;
        let name = device_name(&device).unwrap_or_default();
        let (spec, _) = spec_from_supported(
            &output_config(&device, config.sample_format)?,
            &name,
            config.quantum,
        )?;
        Ok(Self {
            stream: OutputStream::new(spec),
            target: Some((host, config, executor.clone())),
            opened: Opened::of(&device),
        })
    }

    /// Creates an engine with no host and no device, at the caller's spec.
    ///
    /// Pairs with [`ManualStreamDriver`](tutti_engine::ManualStreamDriver) to run the
    /// whole lifecycle — start, render, fault, stop, restart — with no sound
    /// card. Such an engine can only be started with
    /// [`start_with`](Self::start_with); [`start`](Self::start) returns
    /// [`DeviceError::InvalidDevice`], and [`opened_name`](Self::opened_name) is
    /// `Ok(None)`.
    #[must_use]
    pub fn from_spec(spec: OutputSpec) -> Self {
        Self {
            stream: OutputStream::new(spec),
            target: None,
            opened: Opened::Supplied,
        }
    }

    /// Returns the configuration of the stream that is playing, or that the
    /// next start would use.
    pub fn spec(&self) -> &OutputSpec {
        self.stream.spec()
    }

    /// Returns the backend fault record, which survives stop and restart.
    ///
    /// Take it once at startup and poll it; CPAL's error callback returns
    /// nothing, so a fault such as an unplugged device is reported only here.
    pub fn faults(&self) -> Arc<StreamFaults> {
        self.stream.faults()
    }

    /// The delay the output callback last reported between rendering a block
    /// and its playing ([`StreamLatency`]): `None` until a callback of the
    /// running stream reports one, and again after a restart.
    pub fn latency(&self) -> Arc<StreamLatency> {
        self.stream.latency()
    }

    /// Builds a stream on the selected device and starts it. A stream that is
    /// already running is an error: the state handed in is not adopted.
    ///
    /// Re-reads the device's config first (at the chosen format, if any), so after a
    /// [`select_device`](Self::select_device) the [`spec`](Self::spec),
    /// [`sample_rate`](Self::sample_rate) and [`channels`](Self::channels)
    /// describe the device that is actually playing. The stream is opened with
    /// a fixed buffer of the [`quantum`](OutputConfig::quantum) asked for, which
    /// the device must offer, or else of
    /// [`PREFERRED_QUANTUM`](crate::PREFERRED_QUANTUM) frames clamped to the
    /// device's range, when the device reports one. Clears
    /// [`faults`](Self::faults).
    ///
    /// # Errors
    /// [`DeviceError::AlreadyRunning`] if a stream is running (it is left running),
    /// [`DeviceError::InvalidDevice`] for an unresolvable selector,
    /// [`DeviceError::DeviceNotAvailable`] if the config cannot be read,
    /// [`DeviceError::OutputFormatUnavailable`] for a chosen format the device does
    /// not offer, [`DeviceError::QuantumUnavailable`] for a chosen callback size
    /// outside its range, [`DeviceError::InvalidConfig`] for a sample format the engine does
    /// not build,
    /// or [`DeviceError::BuildStream`] / [`DeviceError::PlayStream`] from CPAL.
    pub fn start<F: Float>(&mut self, state: Arc<AudioCallbackState<F>>) -> Result<()>
    where
        CpalDriver: StreamDriver<F, Error = crate::DeviceError>,
    {
        if self.is_running() {
            return Err(DeviceError::AlreadyRunning);
        }
        let (driver, spec, opened) = self.resolve()?;
        self.stream.start_at(state, spec, driver)?;
        self.opened = opened;
        Ok(())
    }

    /// Resolve the selected device and read its config, without opening a
    /// stream: the first half of [`start`](Self::start), split out so a restart
    /// can see the config the stream will run at before it runs
    /// (`TuttiDriver::restart_with`).
    pub(crate) fn resolve(&self) -> Result<(CpalDriver, OutputSpec, Opened)> {
        Self::resolve_target(self.target.as_ref())
    }

    /// Resolves the selected device and reads its config, without opening a
    /// stream: the device is found and its config known, so a host that moves
    /// a running graph onto it can refuse what the device cannot take (a root
    /// it cannot place) while the old stream still plays.
    ///
    /// # Errors
    /// As [`start`](Self::start) up to the stream's opening.
    pub fn resolve_device(&self) -> Result<Resolved> {
        let (driver, spec, opened) = self.resolve()?;
        Ok(Resolved {
            spec,
            driver,
            opened,
        })
    }

    /// [`resolve`](Self::resolve) for a bare `target`, which a restart reads
    /// while the stream is borrowed elsewhere.
    pub(crate) fn resolve_target(
        target: Option<&Target>,
    ) -> Result<(CpalDriver, OutputSpec, Opened)> {
        let Some((host, config, executor)) = target else {
            return Err(no_device());
        };
        let device = host.device(DeviceDirection::Output, &config.device)?;
        let name = device_name(&device).unwrap_or_default();
        let (spec, format) = spec_from_supported(
            &output_config(&device, config.sample_format)?,
            &name,
            config.quantum,
        )?;
        // What it is called is recorded by the caller once the stream runs
        // (`opened`): a restart that fails leaves the name of the device the
        // previous stream ran on.
        let opened = Opened::of(&device);
        Ok((
            CpalDriver::new(device, format, executor.clone())?,
            spec,
            opened,
        ))
    }

    /// Starts `resolved` (this engine's device, found by
    /// [`resolve_device`](Self::resolve_device)) on `host`, a host whose stream
    /// another driver ran (a timer): the device takes over the stream, `rerate`
    /// runs between the stop and the start as in
    /// [`TuttiDriver::restart_with`](crate::TuttiDriver::restart_with), and the
    /// host's stream, fault record and latency cell are the same ones.
    ///
    /// # Errors
    /// The hook's error, or what the driver's `open` reports, as `E`, **with
    /// the host back** and its stream stopped at its old spec: nothing plays,
    /// and the driver the host ran is to be started again by the caller.
    pub fn start_resolved<F: Float, E: From<DeviceError>>(
        self,
        host: tutti_engine::StreamHost<F>,
        resolved: Resolved,
        rerate: impl FnOnce(&OutputSpec, &tutti_engine::Stopped<'_, F>) -> core::result::Result<(), E>,
    ) -> core::result::Result<crate::TuttiDriver<F>, (tutti_engine::StreamHost<F>, E)>
    where
        CpalDriver: StreamDriver<F, Error = crate::DeviceError>,
    {
        let (spec, driver, opened) = resolved.into_parts();
        crate::TuttiDriver::take_over(host, spec, driver, rerate, self.target, opened)
    }

    /// [`start_resolved`](Self::start_resolved) with a driver of the caller's
    /// choosing at this engine's spec ([`from_spec`](Self::from_spec)): no
    /// device is opened, as [`TuttiDriver::switch`](crate::TuttiDriver::switch)
    /// opens none. Behind `testing`.
    ///
    /// # Errors
    /// As [`start_resolved`](Self::start_resolved).
    #[cfg(feature = "testing")]
    pub fn start_on<F: Float, D: StreamDriver<F>, E: From<DeviceError>>(
        self,
        host: tutti_engine::StreamHost<F>,
        driver: D,
        rerate: impl FnOnce(&OutputSpec, &tutti_engine::Stopped<'_, F>) -> core::result::Result<(), E>,
    ) -> core::result::Result<crate::TuttiDriver<F>, (tutti_engine::StreamHost<F>, E)>
    where
        CpalDriver: StreamDriver<F, Error = crate::DeviceError>,
        D::Error: Into<DeviceError>,
    {
        let spec = self.spec().clone();
        crate::TuttiDriver::take_over(host, spec, driver, rerate, self.target, self.opened)
    }

    /// Starts a stream at the current [`spec`](Self::spec) through `driver`.
    /// A stream that is already running is an error: the driver is not opened.
    ///
    /// Unlike [`start`](Self::start) it resolves no device: the driver decides
    /// where the callback runs. With a
    /// [`ManualStreamDriver`](tutti_engine::ManualStreamDriver) the caller runs
    /// each callback by hand. Clears [`faults`](Self::faults).
    ///
    /// # Errors
    /// [`DeviceError::AlreadyRunning`] if a stream is running (it is left running);
    /// otherwise whatever the driver's [`open`](StreamDriver::open) reports; for
    /// [`CpalDriver`], [`DeviceError::BuildStream`] / [`DeviceError::PlayStream`] from CPAL.
    pub fn start_with<F: Float, D: StreamDriver<F>>(
        &mut self,
        state: Arc<AudioCallbackState<F>>,
        driver: D,
    ) -> Result<()>
    where
        D::Error: Into<DeviceError>,
    {
        Ok(self.stream.start_with(state, driver)?)
    }

    /// Stops and drops the running stream, if any. Idempotent.
    ///
    /// When this returns, the callback no longer runs.
    pub fn stop(&mut self) {
        self.stream.stop();
    }

    /// Returns the sample rate of the running stream, or of the config the
    /// next start would use.
    pub fn sample_rate(&self) -> SampleRate {
        self.stream.sample_rate()
    }

    /// Returns the channel layout of the running stream, or of the config the
    /// next start would use. This is the width
    /// [`process_audio`](tutti_engine::process_audio) is handed.
    pub fn channels(&self) -> ChannelLayout {
        self.stream.channels()
    }

    /// Returns whether a stream is open **and** the backend has not reported
    /// the device gone.
    ///
    /// A stream whose device was unplugged reads `false` here even though it
    /// has not been stopped; see [`StreamFaults::is_disconnected`].
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.stream.is_running()
    }

    /// Selects the device the next [`start`](Self::start) opens, by name or
    /// index. Does not disturb a running stream.
    ///
    /// # Errors
    /// [`DeviceError::InvalidDevice`] on an engine built with
    /// [`from_spec`](Self::from_spec), which has no device to select.
    pub fn select_device(&mut self, sel: DeviceSelector) -> Result<()> {
        let Some((_, current, _)) = &mut self.target else {
            return Err(no_device());
        };
        current.select(sel);
        Ok(())
    }

    /// The name of the device the running (or last started) stream opened,
    /// as it was then. It asks the host nothing, so it never enumerates
    /// (which can block for good in a sound server's ALSA plugin) and is
    /// safe on a thread that renders a frame.
    ///
    /// `Ok(None)` is a stream on a driver the caller supplied
    /// ([`from_spec`](Self::from_spec), or a restart onto one): it has no
    /// device, by design.
    ///
    /// # Errors
    /// [`DeviceError::InvalidDevice`] when the device opened but the host could
    /// not name it.
    pub fn opened_name(&self) -> Result<Option<String>> {
        self.opened.name()
    }
}

#[cfg(test)]
mod tests;
