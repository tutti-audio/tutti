//! Inputs: a capture device read into the graph, and reopened by a restart
//! ([`Session::add_input`]).
//!
//! An input is two things that belong together: a capture stream on the
//! session's audio host ([`tutti_cpal::MicIn`]) and the inlet node that copies
//! its monitor ring into the graph ([`Graph::add_inlet`](crate::Graph::add_inlet)). Both are made by one
//! call, and the node is the input's name in the graph: wire it
//! (`connect(input.node(), ..)`) like any node, to hear the device live
//! through effects.
//!
//! A capture stream runs at the rate it was opened at, and an inlet does not
//! resample, so a restart onto a device of another rate leaves every input
//! playing at the wrong speed. The session therefore keeps each input's
//! [`InputConfig`] and reopens them all after a restart, at the graph's new
//! rate, swapping each new ring into the **same node**: nothing is rewired and
//! the key a host holds stays good. An input that cannot be reopened (its
//! device is gone) is reported ([`DeviceEvent::InputReopenFailed`]) and left
//! silent until the next restart tries again.

use std::any::Any;
use std::sync::Arc;

use tutti_buffers::{Float, SampleFormat};
use tutti_cpal::{AudioHost, DeviceSelector, MONITOR_RING, MicConfig, MicIn};
use tutti_engine::StreamLatency;
use tutti_graph::{DECLICK, FadeLaw, InletNode, InletRing};
use tutti_units::{SampleRate, Samples, Seconds};

use crate::graph::CrossfadeRefused;
use crate::{DeviceEvent, Error, Key, Result, Session, SessionEvent, SessionFloat, Trim};

/// What [`Session::add_input`] opens: the capture device, which of its inputs,
/// and how deep the ring that carries them into the graph is.
///
/// `Default` is the host's default input device, its first two inputs (a mono
/// device's one input on both sides) and a ring of about 10 ms. Build one with
/// the methods or by struct update.
///
/// ```
/// use tutti_cpal::DeviceSelector;
/// use tutti_session::InputConfig;
///
/// // Inputs 3 and 4 of a named interface.
/// let config = InputConfig::default()
///     .device(DeviceSelector::named("Interface"))
///     .channels(&[2, 3]);
/// assert_eq!(config.channels.as_deref(), Some(&[2, 3][..]));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct InputConfig {
    /// The input device, on the session's host
    /// ([`SessionBuilder::host`](crate::SessionBuilder::host)).
    pub device: DeviceSelector,
    /// The device inputs to capture, counted from 0: channel `i` of the input
    /// is the device's input `channels[i]` (`&[2, 3]` is inputs 3 and 4 of 16,
    /// chosen with [`Fold::select`](tutti_buffers::Fold::select)). `None`: the
    /// first two.
    pub channels: Option<Vec<usize>>,
    /// The sample format to open the device at, at the graph's rate
    /// ([`MicConfig::sample_format`]): `None` is `F32` when the device offers
    /// it. A format the device lacks fails the open with
    /// [`tutti_cpal::DeviceError::InputFormatUnavailable`].
    pub sample_format: Option<SampleFormat>,
    /// The depth of the ring into the graph, in time at the input's rate:
    /// shallow on purpose, so a monitored input is heard now and not late. A
    /// longer one rides out a longer stall of the graph's callback at that
    /// much latency.
    pub capacity: Seconds,
    /// The manual trim of a take's placement, for a device that misreports its
    /// latency: set once per input and kept across restarts, so it is on the
    /// config and not on the [`Input`] (a plain value naming a node). It is one
    /// of the latency a take states ([`TakeLatency::offset`](crate::TakeLatency::offset)).
    pub offset: Trim,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            device: DeviceSelector::Default,
            channels: None,
            sample_format: None,
            capacity: MONITOR_RING,
            offset: Trim::NONE,
        }
    }
}

impl InputConfig {
    /// This config on `device`.
    #[must_use]
    pub fn device(mut self, device: impl Into<DeviceSelector>) -> Self {
        self.device = device.into();
        self
    }

    /// This config capturing the device inputs `channels` (counted from 0).
    #[must_use]
    pub fn channels(mut self, channels: &[usize]) -> Self {
        self.channels = Some(channels.to_vec());
        self
    }

    /// This config opening the device at `format`.
    #[must_use]
    pub fn sample_format(mut self, format: SampleFormat) -> Self {
        self.sample_format = Some(format);
        self
    }

    /// This config with a ring `capacity` deep.
    #[must_use]
    pub fn capacity(mut self, capacity: Seconds) -> Self {
        self.capacity = capacity;
        self
    }

    /// This config with a manual placement trim ([`InputConfig::offset`]).
    #[must_use]
    pub fn offset(mut self, offset: Trim) -> Self {
        self.offset = offset;
        self
    }
}

/// An input the session opened ([`Session::add_input`]): the name of its
/// inlet node.
///
/// `F` is the session's float. It is a plain value. The input lives as long as the session, which owns the
/// capture stream and reopens it after a restart; remove the node
/// ([`Session::remove`]) to end it.
pub struct Input<F: Float = f32> {
    node: Key<InletNode<F>>,
}

impl<F: Float> Clone for Input<F> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<F: Float> Copy for Input<F> {}

impl<F: Float> std::fmt::Debug for Input<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Input").field("node", &self.node).finish()
    }
}

impl<F: Float> PartialEq for Input<F> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node
    }
}

impl<F: Float> Eq for Input<F> {}

impl<F: Float> Input<F> {
    /// The inlet node the input feeds the graph through: wire it like any
    /// node, to monitor the device through effects. It is the same node across
    /// restarts.
    #[must_use]
    pub fn node(&self) -> Key<InletNode<F>> {
        self.node
    }
}

/// What an [`InputOpener`] opened: the ring into the graph, and the capture
/// stream's owner, dropped to close it.
pub struct OpenedInput<F: Float = f32> {
    /// The ring the capture callback fills, with the rate it fills it at.
    pub ring: InletRing<F>,
    /// Whatever keeps the capture stream running; dropping it stops capture.
    pub keep: Box<dyn Any + Send>,
    /// The fixed lead of the ring, in frames at the ring's rate: how long a
    /// frame sits in it before the graph can pop it, by construction (a ring
    /// the producer prefills). A ring that is not prefilled has none: `0` is
    /// that statement, not a missing figure; the capture's own delay is
    /// [`Device::latency`](crate::Device::latency)'s `input`. A take states it
    /// as [`TakeLatency::lead`](crate::TakeLatency::lead).
    pub lead: Samples,
}

impl<F: Float> std::fmt::Debug for OpenedInput<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedInput")
            .field("ring", &self.ring)
            .finish_non_exhaustive()
    }
}

/// Where a session's inputs come from: the sound card's capture streams
/// ([`MicIn`]), or, behind `testing`, a test's own.
pub trait InputOpener<F: Float = f32>: std::fmt::Debug + Send {
    /// Opens an input as `config` describes it, at the graph's `rate`.
    ///
    /// # Errors
    ///
    /// Whatever the capture device refuses.
    fn open(&mut self, config: &MicConfig, rate: SampleRate) -> tutti_cpal::Result<OpenedInput<F>>;
}

/// An opener that serves both session floats, which is what the builder holds
/// so that its options do not depend on the float.
pub trait Openers: InputOpener<f32> + InputOpener<f64> {}

impl<O: InputOpener<f32> + InputOpener<f64>> Openers for O {}

/// The sound card's capture streams.
#[derive(Debug)]
struct Capture;

impl<F: Float> InputOpener<F> for Capture {
    fn open(&mut self, config: &MicConfig, rate: SampleRate) -> tutti_cpal::Result<OpenedInput<F>> {
        let (mic, ring) = MicIn::<F>::open_with_monitor(config, rate)?;
        Ok(OpenedInput {
            ring,
            keep: Box::new(mic),
            // The monitor ring starts empty.
            lead: Samples::ZERO,
        })
    }
}

/// One input the session holds: what to reopen, and the stream while it runs.
pub(crate) struct Held<F: Float> {
    pub(crate) node: Key<InletNode<F>>,
    pub(crate) config: InputConfig,
    /// The lead of the ring the opener gave last.
    pub(crate) lead: Samples,
    keep: Option<Box<dyn Any + Send>>,
}

/// The session's inputs, and what opens them.
pub(crate) struct Inputs<F: SessionFloat> {
    host: AudioHost,
    /// Where every input's capture callback reports its delay, which the
    /// [`Device`](crate::Device) reads.
    latency: Arc<StreamLatency>,
    opener: Box<dyn InputOpener<F>>,
    held: Vec<Held<F>>,
    /// Where the output stream's callback reports its delay, once the session
    /// has a device ([`Device::latency`](crate::Device::latency)'s `output`).
    output_latency: Option<Arc<StreamLatency>>,
    /// Names the takes begun and carries what they report.
    takes: crate::take::Takes,
}

impl<F: SessionFloat> std::fmt::Debug for Inputs<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inputs")
            .field("host", &self.host)
            .field("open", &self.held.len())
            .finish_non_exhaustive()
    }
}

impl<F: SessionFloat> Inputs<F> {
    /// The inputs of a session built with `options`: on its host, opened by
    /// the builder's opener if it set one.
    pub(crate) fn from_options(options: &mut crate::builder::Options) -> Self {
        Self::new(options.host, options.input_opener.take())
    }

    pub(crate) fn new(host: AudioHost, opener: Option<Box<dyn Openers>>) -> Self {
        Self {
            host,
            latency: Arc::new(StreamLatency::new()),
            opener: opener.map_or_else(
                || Box::new(Capture) as Box<dyn InputOpener<F>>,
                <F as crate::format::Sealed>::upcast_opener,
            ),
            held: Vec::new(),
            output_latency: None,
            takes: crate::take::Takes::new(),
        }
    }

    /// The cell the output stream reports into, for a take's placement.
    pub(crate) fn set_output_latency(&mut self, latency: Arc<StreamLatency>) {
        self.output_latency = Some(latency);
    }

    /// The input whose inlet node is `node`.
    pub(crate) fn held(&self, node: Key<InletNode<F>>) -> Option<&Held<F>> {
        self.held.iter().find(|h| h.node == node)
    }

    /// Closes and forgets the input whose inlet node is `node`, if it is one:
    /// its capture stream drops, and no restart reopens it.
    pub(crate) fn forget(&mut self, node: tutti_graph::NodeKey) {
        self.held
            .retain(|h| tutti_graph::NodeKey::from(h.node) != node);
    }

    /// Closes every input: the capture streams drop and no restart reopens
    /// them. The inputs closed, in the order they were added.
    pub(crate) fn close_all(&mut self) -> Vec<Input<F>> {
        std::mem::take(&mut self.held)
            .into_iter()
            .map(|held| Input { node: held.node })
            .collect()
    }

    /// What a take reads the device's delay through.
    pub(crate) fn latency_probe(&self) -> crate::take::LatencyProbe {
        crate::take::LatencyProbe::new(Arc::clone(&self.latency), self.output_latency.clone())
    }

    /// The takes' numbering and report channel.
    pub(crate) fn takes(&self) -> &crate::take::Takes {
        &self.takes
    }

    /// Lands what the takes' pumps reported since the last update, in the order sent.
    pub(crate) fn land_takes(&mut self, out: &mut Vec<crate::SessionEvent>) {
        self.takes.land(out);
    }

    /// The cell the capture callbacks report into.
    pub(crate) fn latency(&self) -> Arc<StreamLatency> {
        Arc::clone(&self.latency)
    }

    fn mic(&self, config: &InputConfig) -> MicConfig {
        MicConfig {
            host: self.host,
            device: config.device.clone(),
            inputs: config.channels.clone(),
            sample_format: config.sample_format,
            latency: Some(Arc::clone(&self.latency)),
            monitor_depth: config.capacity,
        }
    }
}

impl<K: crate::AppKey, F: crate::SessionFloat> Session<K, crate::DeviceClock<F>> {
    /// Opens a capture input on the session's host and puts it in the graph:
    /// the capture stream ([`tutti_cpal::MicIn`]) and the inlet node that copies
    /// it in ([`Graph::add_inlet`](crate::Graph::add_inlet)), in one call. Wire
    /// [`Input::node`] to monitor it.
    ///
    /// The session keeps the input's config and **reopens it after every
    /// restart** ([`Session::restart`](crate::Session::restart)), at
    /// the graph's new rate, into the same node: nothing to rewire. The capture
    /// delay it reports reads back as [`Device::latency`](crate::Device::latency)'s
    /// `input`.
    ///
    /// # Errors
    ///
    /// [`Error::Device`] when the host or the device cannot be opened, cannot
    /// run at the graph's rate, or has not the inputs asked for;
    /// [`Error::Inlet`] when the stream's rate is not the graph's.
    pub fn add_input(&mut self, config: InputConfig) -> Result<Input<F>> {
        let rate = self.graph.prepared().sample_rate();
        let opened = self.inputs.opener.open(&self.inputs.mic(&config), rate)?;
        let node = self.graph.add_inlet(opened.ring)?;
        self.inputs.held.push(Held {
            node,
            config,
            lead: opened.lead,
            keep: Some(opened.keep),
        });
        Ok(Input { node })
    }

    /// Reopens every input at the graph's rate, after a restart: the old stream
    /// first (a device may not be opened twice), then a new ring swapped into
    /// the same node. What cannot be reopened is reported and left silent.
    pub(crate) fn reopen_inputs(&mut self) {
        let rate = self.graph.prepared().sample_rate();
        // The new streams have reported nothing yet.
        self.inputs.latency.reset();
        let mut held = std::mem::take(&mut self.inputs.held);
        held.retain_mut(|input| match self.reopen(input, rate) {
            Ok(()) => true,
            // The host removed the node: there is no input to keep.
            Err(Error::Crossfade(CrossfadeRefused::NoSuchNode(_))) => false,
            Err(error) => {
                self.running
                    .events
                    .push(SessionEvent::Device(DeviceEvent::InputReopenFailed {
                        input: input.node.erase(),
                        error,
                    }));
                true
            }
        });
        self.inputs.held = held;
    }

    fn reopen(&mut self, input: &mut Held<F>, rate: SampleRate) -> Result<()> {
        input.keep = None;
        let opened = self
            .inputs
            .opener
            .open(&self.inputs.mic(&input.config), rate)?;
        let node = self.graph.inlet_node(opened.ring)?;
        self.graph
            // The old ring is silent by now (its stream was dropped first), so
            // the fade only smooths the step.
            .crossfade(input.node, node, DECLICK, FadeLaw::default())?;
        input.keep = Some(opened.keep);
        input.lead = opened.lead;
        Ok(())
    }
}
