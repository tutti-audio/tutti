//! CPAL audio I/O: the output device and its stream lifecycle.
//!
//! The callback itself (rendering the graph, `process_audio`, the fault and
//! latency cells) is `tutti-engine`'s; this module resolves a device and holds
//! the [`OutputStream`] that runs on it.

use cpal::traits::DeviceTrait;
use std::sync::Arc;
use tutti_buffers::{Float, SampleFormat};
use tutti_engine::{AudioCallbackState, OutputQuantum, OutputSpec, OutputStream, StreamDriver};
use tutti_platform::Executor;

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
    /// Asks the device for callbacks of `quantum` frames. A size outside the
    /// range the device reports is [`DeviceError::QuantumUnavailable`] when the
    /// stream is opened, never clamped into it. Without this the stream asks
    /// for [`PREFERRED_QUANTUM`](crate::PREFERRED_QUANTUM) frames, clamped into
    /// the range, and a device that reports no range opens with its own buffer.
    ///
    /// The size is a request. A backend that refuses it
    /// (`cpal::ErrorKind::UnsupportedConfig`) is asked again for its default
    /// buffer, and the stream's [`OutputSpec::quantum`](tutti_engine::OutputSpec::quantum)
    /// then reports what the open stream's `buffer_size` says, or `None` when
    /// it cannot say. cpal 0.18 reports it as:
    ///
    /// | Backend | `buffer_size` |
    /// |---|---|
    /// | ALSA | the negotiated period |
    /// | CoreAudio | the device's buffer frame size, which can change while the stream runs |
    /// | JACK | the server's buffer size, which can change |
    /// | WASAPI | the device period, which a shared-mode callback need not match |
    /// | PulseAudio | the minimum request length, not the callback size |
    ///
    /// cpal documents it as an estimate that is not enforced per callback.
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

/// What an engine with no device to open answers a call that needs one.
fn no_device() -> DeviceError {
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
/// exists (its [`stream`](Self::stream)'s [`sample_rate`](OutputStream::sample_rate) is the rate to build the
/// graph at). Every method runs on the control thread.
///
/// The stream runs until [`stop`](Self::stop) or until the engine is dropped.
/// Backend errors go to the stream's [`faults`](OutputStream::faults).
pub struct AudioEngine {
    pub(crate) stream: OutputStream,
    /// What the stream runs on: the device it re-resolves on start, or a
    /// driver the caller supplied.
    pub(crate) origin: Origin,
}

/// What an engine's stream runs on.
#[derive(Debug)]
pub(crate) enum Origin {
    /// The device: where a start finds it again, and the name it opened under,
    /// kept so that naming it never enumerates the host.
    Device { target: Target, opened: Opened },
    /// A driver the caller supplied: no device, by design. `was` is the
    /// device it replaced, which a restart opens again; `None` for an engine
    /// built from a bare spec, which never had one.
    Supplied { was: Option<Target> },
}

impl Origin {
    /// The device to resolve: the one this engine runs on or last ran on.
    ///
    /// # Errors
    /// [`DeviceError::InvalidDevice`] for an engine that never had a device.
    pub(crate) fn target(&self) -> Result<&Target> {
        match self {
            Self::Device { target, .. }
            | Self::Supplied {
                was: Some(target), ..
            } => Ok(target),
            Self::Supplied { was: None } => Err(no_device()),
        }
    }

    /// As [`target`](Self::target), to select another device on.
    pub(crate) fn target_mut(&mut self) -> Result<&mut Target> {
        match self {
            Self::Device { target, .. }
            | Self::Supplied {
                was: Some(target), ..
            } => Ok(target),
            Self::Supplied { was: None } => Err(no_device()),
        }
    }

    fn into_target(self) -> Option<Target> {
        match self {
            Self::Device { target, .. } => Some(target),
            Self::Supplied { was } => was,
        }
    }

    /// The name of the device opened, `Ok(None)` for a supplied driver.
    pub(crate) fn opened_name(&self) -> Result<Option<String>> {
        match self {
            Self::Device { opened, .. } => opened.name().map(Some),
            Self::Supplied { .. } => Ok(None),
        }
    }

    /// The origin after a stream started on `opened`: the device, or with
    /// `None` a supplied driver that keeps the device on record.
    ///
    /// # Errors
    /// [`DeviceError::InvalidDevice`] for a device opened by an engine that
    /// has none.
    pub(crate) fn started(self, opened: Option<Opened>) -> Result<Self> {
        match (self.into_target(), opened) {
            (Some(target), Some(opened)) => Ok(Self::Device { target, opened }),
            (was, None) => Ok(Self::Supplied { was }),
            (None, Some(_)) => Err(no_device()),
        }
    }

    /// Records a restart's stream as running on the device `opened`.
    ///
    /// # Errors
    /// [`DeviceError::InvalidDevice`] for an engine that has no device, as
    /// [`started`](Self::started) answers.
    pub(crate) fn reopened(&mut self, now: Opened) -> Result<()> {
        match self {
            Self::Device { opened, .. } => *opened = now,
            Self::Supplied { was } => {
                let target = was.take().ok_or_else(no_device)?;
                *self = Self::Device {
                    target,
                    opened: now,
                };
            }
        }
        Ok(())
    }

    /// Records the stream as running on a supplied driver, keeping the device.
    pub(crate) fn supply(&mut self) {
        let was = std::mem::replace(self, Self::Supplied { was: None }).into_target();
        *self = Self::Supplied { was };
    }
}

/// The device an engine resolves, with what opens its stream: its host, its config and
/// the executor whose dedicated thread owns the stream.
pub(crate) type Target = (DeviceHost, OutputConfig, Executor);

/// A driver and the config its stream will run at, not yet opened as a stream:
/// what [`AudioEngine::start_resolved`] starts. The driver is the device's
/// ([`AudioEngine::resolve_device`] finds it), or the caller's own
/// ([`supplied`](Self::supplied)).
pub struct Resolved<D = CpalDriver> {
    spec: OutputSpec,
    driver: D,
    /// The device's name; `None` for a supplied driver.
    opened: Option<Opened>,
}

impl<D> Resolved<D> {
    /// The config the stream will run at.
    #[must_use]
    pub fn spec(&self) -> &OutputSpec {
        &self.spec
    }

    /// A driver of the caller's choosing at `spec`, which stands for the config
    /// its device reports: no device is opened, and the device is recorded as
    /// supplied. Behind `testing`.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn supplied(spec: OutputSpec, driver: D) -> Self {
        Self {
            spec,
            driver,
            opened: None,
        }
    }
}

impl<D> std::fmt::Debug for Resolved<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

/// The device a stream runs on, as the engine learned it when it opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Opened {
    /// A device, and its name.
    Named(String),
    /// A device the host could not name.
    Unnamed(String),
}

impl Opened {
    pub(crate) fn of(device: &cpal::Device) -> Self {
        device_name(device).map_or_else(|e| Self::Unnamed(e.to_string()), Self::Named)
    }

    /// The name, or an error for a device the host could not name.
    pub(crate) fn name(&self) -> Result<String> {
        match self {
            Self::Named(name) => Ok(name.clone()),
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
            origin: Origin::Device {
                target: (host, config, executor.clone()),
                opened: Opened::of(&device),
            },
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
            origin: Origin::Supplied { was: None },
        }
    }

    /// The stream, for reads: [`spec`](OutputStream::spec) (the configuration
    /// of the stream that is playing, or the next start's),
    /// [`faults`](OutputStream::faults) (the backend fault record, which survives
    /// stop and restart: CPAL's error callback returns nothing, so a fault such
    /// as an unplugged device is reported only there),
    /// [`latency`](OutputStream::latency), [`sample_rate`](OutputStream::sample_rate),
    /// [`channels`](OutputStream::channels) and
    /// [`is_running`](OutputStream::is_running).
    #[must_use]
    pub fn stream(&self) -> &OutputStream {
        &self.stream
    }

    /// Builds a stream on the selected device and starts it. A stream that is
    /// already running is an error: the state handed in is not adopted.
    ///
    /// Re-reads the device's config first (at the chosen format, if any), so the
    /// [`spec`](OutputStream::spec),
    /// [`sample_rate`](OutputStream::sample_rate) and [`channels`](OutputStream::channels)
    /// describe the device that is actually playing. The stream is opened with
    /// a fixed buffer of the [`quantum`](OutputConfig::quantum) asked for, which
    /// the device must offer, or else of
    /// [`PREFERRED_QUANTUM`](crate::PREFERRED_QUANTUM) frames clamped to the
    /// device's range, when the device reports one. Clears
    /// [`faults`](OutputStream::faults).
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
        if self.stream.is_running() {
            return Err(DeviceError::AlreadyRunning);
        }
        let (driver, spec, opened) = self.resolve()?;
        self.stream.start_at(state, spec, driver)?;
        self.origin.reopened(opened)
    }

    /// Resolve the selected device and read its config, without opening a
    /// stream: the first half of [`start`](Self::start), split out so a restart
    /// can see the config the stream will run at before it runs
    /// (`TuttiDriver::restart_with`).
    pub(crate) fn resolve(&self) -> Result<(CpalDriver, OutputSpec, Opened)> {
        Self::resolve_target(self.origin.target()?)
    }

    /// Resolves the selected device and reads its config, without opening a
    /// stream: the device is found and its config known, so a host that moves
    /// a running graph onto it can refuse what the device cannot take (a main output
    /// it cannot place) while the old stream still plays.
    ///
    /// # Errors
    /// As [`start`](Self::start) up to the stream's opening.
    pub fn resolve_device(&self) -> Result<Resolved> {
        let (driver, spec, opened) = self.resolve()?;
        Ok(Resolved {
            spec,
            driver,
            opened: Some(opened),
        })
    }

    /// [`resolve`](Self::resolve) for a bare `target`, which a restart reads
    /// while the stream is borrowed elsewhere.
    pub(crate) fn resolve_target(
        (host, config, executor): &Target,
    ) -> Result<(CpalDriver, OutputSpec, Opened)> {
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
            CpalDriver::new(device, format, executor.clone()),
            spec,
            opened,
        ))
    }

    /// Starts `resolved` (this engine's device, found by
    /// [`resolve_device`](Self::resolve_device), or a
    /// [`supplied`](Resolved::supplied) driver, which opens no device) on `host`,
    /// a host whose stream another driver ran (a timer): the driver takes over
    /// the stream, `rerate` runs between the stop and the start as in
    /// [`TuttiDriver::restart_with`](crate::TuttiDriver::restart_with), and the
    /// host's stream, fault record and latency cell are the same ones.
    ///
    /// # Errors
    /// The hook's error, or what the driver's `open` reports, as `E`, **with
    /// the host back** and its stream stopped at its old spec: nothing plays,
    /// and the driver the host ran is to be started again by the caller.
    pub fn start_resolved<F: Float, D: StreamDriver<F>, E: From<DeviceError>>(
        self,
        host: tutti_engine::StreamHost<F>,
        resolved: Resolved<D>,
        rerate: impl FnOnce(&OutputSpec, &tutti_engine::Stopped<'_, F>) -> core::result::Result<(), E>,
    ) -> core::result::Result<crate::TuttiDriver<F>, (tutti_engine::StreamHost<F>, E)>
    where
        D::Error: Into<DeviceError>,
    {
        let Resolved {
            spec,
            driver,
            opened,
        } = resolved;
        match self.origin.started(opened) {
            Ok(origin) => crate::TuttiDriver::take_over(host, spec, driver, rerate, origin),
            Err(e) => Err((host, e.into())),
        }
    }

    /// Starts a stream at the current [`spec`](OutputStream::spec) through `driver`.
    /// A stream that is already running is an error: the driver is not opened.
    ///
    /// Unlike [`start`](Self::start) it resolves no device: the driver decides
    /// where the callback runs. With a
    /// [`ManualStreamDriver`](tutti_engine::ManualStreamDriver) the caller runs
    /// each callback by hand. Clears [`faults`](OutputStream::faults).
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
        self.origin.opened_name()
    }
}

#[cfg(test)]
pub(crate) mod tests;
