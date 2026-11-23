//! What this crate's tests share: the inlets a port list is built over, and a
//! graph that plays the audio thread, so a test sees an event where the graph
//! would: on its frame, after the inlet has held it for its block.

use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;

use tutti_fixtures::{BlockClock, EventRig};
use tutti_graph::{EventInletNode, NodeKey};
use tutti_midi_runtime::InputInlets;
use tutti_ump::Ump;
use tutti_units::{Frame, SampleRate};

use crate::HardwareMidiInputs;

pub(crate) const RATE: f64 = 44_100.0;

/// A list of ports over inlets of 64 events, with no lead.
pub(crate) fn ports() -> Arc<HardwareMidiInputs> {
    Arc::new(HardwareMidiInputs::new(InputInlets::new(
        tutti_graph::EventCapacity::of(64),
        Duration::ZERO,
    )))
}

/// The graph's owner and its audio thread in one: it adds the inlets it is
/// handed ([`attach`](Self::attach)) as a session's `Poll` phase does, writes the
/// block clock and renders blocks of 64 frames.
pub(crate) struct Rig {
    rig: EventRig,
    clock: BlockClock,
    /// When frame 0 began.
    pub(crate) origin: Instant,
    pub(crate) frames: u64,
    /// How many inlets are attached: the sink input the next one feeds.
    attached: usize,
}

impl Rig {
    pub(crate) const BLOCK: usize = 64;
    /// How many inlets the sink has an event input for.
    pub(crate) const INPUTS: u16 = 4;

    /// A rig over `inlets`, whose clock reads `origin` at frame 0.
    pub(crate) fn over(inlets: &Arc<InputInlets>, origin: Instant) -> Self {
        let mut rig = EventRig::new(RATE, Self::BLOCK, Self::INPUTS);
        rig.commit();
        let clock = BlockClock::new();
        inlets.set_clock(Some(clock.reader()));
        Self {
            rig,
            clock,
            origin,
            frames: 0,
            attached: 0,
        }
    }

    /// Adds `inlet` to the graph and wires it to the sink's next event input,
    /// whose index (the slot of what the sink logs) it returns.
    pub(crate) fn attach(&mut self, inlet: EventInletNode) -> usize {
        let input = self.attached;
        assert!(
            input < usize::from(Self::INPUTS),
            "the sink has no input left"
        );
        self.attached += 1;
        let key = NodeKey(100 + input as u64);
        self.rig.editor.insert(key, "inlet", inlet);
        self.rig.feed(key, 0, input as u16);
        self.rig.commit();
        input
    }

    /// The OS time at which graph frame `frame` begins.
    pub(crate) fn time_of(&self, frame: u64) -> Instant {
        self.origin + Duration::from_secs_f64(frame as f64 / RATE)
    }

    /// Renders `blocks` blocks, each preceded by its clock reading as the
    /// engine writes it.
    pub(crate) fn run(&mut self, blocks: usize) {
        for _ in 0..blocks {
            self.clock.write(
                self.time_of(self.frames),
                Frame::new(self.frames),
                SampleRate::of(RATE),
            );
            self.rig.block(Self::BLOCK);
            self.frames += Self::BLOCK as u64;
        }
    }

    /// Everything the sink has received, taken.
    pub(crate) fn take(&self) -> Vec<(u16, u64, Ump)> {
        self.rig.take()
    }
}

/// The proof that a test backend is ready: a fake backend has nothing to start.
pub(crate) fn started() -> crate::Started {
    crate::Started::new()
}

/// A backend with no OS behind it and nothing to open: its watch is the closure
/// it was built with, its lists are what [`inputs`](Self::inputs) and
/// [`unlistable`](Self::unlistable) set (empty by default), and every open is
/// refused.
pub(crate) struct Stub {
    watch: Box<dyn Fn(crate::ChangeSink) -> crate::Result<Box<dyn crate::Watch>> + Send + Sync>,
    inputs: Result<Vec<crate::OsEndpoint>, &'static str>,
    outputs: Result<Vec<crate::OsEndpoint>, &'static str>,
}

impl Stub {
    /// A backend whose watch is `watch`.
    pub(crate) fn new(
        watch: impl Fn(crate::ChangeSink) -> crate::Result<Box<dyn crate::Watch>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            watch: Box::new(watch),
            inputs: Ok(Vec::new()),
            outputs: Ok(Vec::new()),
        }
    }

    /// A backend that hands its watch's `ChangeSink` to `slot` and watches nothing.
    pub(crate) fn capturing(slot: &Arc<tutti_rt::Mutex<Option<crate::ChangeSink>>>) -> Self {
        let slot = Arc::clone(slot);
        Self::new(move |sink| {
            *slot.lock() = Some(sink);
            Ok(Box::new(crate::endpoints::NoWatch))
        })
    }

    /// A backend whose watch seeds `inputs`, which it also lists.
    pub(crate) fn inputs(inputs: Vec<crate::OsEndpoint>) -> Self {
        let seed = inputs.clone();
        let mut stub = Self::new(move |sink| {
            sink.seed(crate::devices::reporter::os_devices(&seed, &[]))?;
            Ok(Box::new(crate::endpoints::NoWatch))
        });
        stub.inputs = Ok(inputs);
        stub
    }

    /// As `self`, its OS unable to list either direction: both fail with `why`.
    pub(crate) fn unlistable(mut self, why: &'static str) -> Self {
        self.inputs = Err(why);
        self.outputs = Err(why);
        self
    }
}

impl crate::MidiEndpoints for Stub {
    fn watch(&self, sink: crate::ChangeSink) -> crate::Result<Box<dyn crate::Watch>> {
        (self.watch)(sink)
    }
    fn inputs(&self) -> crate::Result<Vec<crate::OsEndpoint>> {
        self.inputs
            .clone()
            .map_err(crate::MidiHardwareError::Unsupported)
    }
    fn outputs(&self) -> crate::Result<Vec<crate::OsEndpoint>> {
        self.outputs
            .clone()
            .map_err(crate::MidiHardwareError::Unsupported)
    }
    fn open_input(
        &self,
        _: crate::Started,
        _: tutti_midi_device::EndpointId,
        _: crate::InputProducerHandle,
    ) -> crate::Result<Box<dyn crate::InputConnection>> {
        Err(crate::MidiHardwareError::Unsupported("refused"))
    }
    fn open_output(
        &self,
        _: crate::Started,
        _: tutti_midi_device::EndpointId,
    ) -> crate::Result<Box<dyn crate::DeviceOut>> {
        Err(crate::MidiHardwareError::Unsupported("refused"))
    }
}
