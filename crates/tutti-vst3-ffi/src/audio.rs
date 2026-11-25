//! The audio-thread half of a plugin: `IAudioProcessor::process` and
//! `setProcessing`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tutti_rt::{Claim, ClaimGuard};
use vst3::Steinberg::Vst::{
    IAudioProcessor, IAudioProcessorTrait, ProcessContext, ProcessData, ProcessSetup,
};
use vst3::{ComPtr, ComWrapper};

use crate::bus_buffers::BoundBuses;
use crate::com::{EventList, ParameterChangesImpl, event_list_ptr, param_changes_ptr};
use crate::error::{Vst3Error, check};
use crate::types::ProcessMode;

/// What `setupProcessing` was last given, as far as `process` needs to hold a
/// block to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Setup {
    /// The `symbolicSampleSize` of the setup: the width the plugin reads.
    pub(crate) symbolic_size: i32,
    /// `maxSamplesPerBlock`: the longest block the plugin was set up for.
    pub(crate) max_block: usize,
}

/// What the plugin has been put in, shared by a [`Plugin`](crate::Plugin) and
/// the halves that outlive no call of it.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// The last successful `setupProcessing`, cleared when the bus
    /// arrangement changes under it or a later setup is refused.
    pub(crate) setup: Option<Setup>,
    /// The channel count of every audio bus, read when the plugin was
    /// activated: what `process` holds the bound buses to.
    pub(crate) buses: Option<Arrangement>,
}

/// The audio bus layout of an active plugin, per direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Arrangement {
    pub(crate) inputs: Box<[usize]>,
    pub(crate) outputs: Box<[usize]>,
}

impl Arrangement {
    /// Whether buses bound with these channel counts are the plugin's. A
    /// direction in which the plugin reports no audio bus accepts any binding,
    /// as the plugin reads none of it.
    pub(crate) fn matches(&self, inputs: &[usize], outputs: &[usize]) -> bool {
        let same = |expected: &[usize], bound: &[usize]| expected.is_empty() || expected == bound;
        same(&self.inputs, inputs) && same(&self.outputs, outputs)
    }
}

/// The processor entry, its state and its claim, shared by a plugin and its
/// audio half.
///
/// The claim owns the data ([`Live`]): `process` and `set_processing` run
/// under it, and so does every control-side state change, so none of them
/// overlaps another. The audio thread only ever `try_claim`s, and refuses the
/// block when it cannot have it, so it never waits on a control thread. The
/// teardown takes the data out for good, so no entry reaches the processor
/// afterwards. The audio half holds no owning reference to the plugin, so the
/// plugin's destruction, and the unloading of its module, always happen on the
/// thread that drops the [`Plugin`](crate::Plugin), never on the audio thread
/// that drops an [`AudioSide`].
pub(crate) struct Shared {
    /// An [`AudioSide`] exists. While it does, control-side state changes are
    /// refused.
    audio_out: AtomicBool,
    /// `setActive(1)` has run and `setActive(0)` has not. Written under the
    /// claim, so it answers right when the claim is dead.
    active: AtomicBool,
    live: Claim<Live>,
}

/// What the claim owns: the plugin's state and its processor.
pub(crate) struct Live {
    pub(crate) state: State,
    pub(crate) processor: ComPtr<IAudioProcessor>,
}

// SAFETY: the processor is used only through a claim guard, and released only
// by the `take` in `Parts::drop` (or the deactivation that `set_active(false)`
// runs on a dead claim), on the thread that drops the plugin, so moving the
// `Live` moves no access of its own.
unsafe impl Send for Live {}

impl Shared {
    pub(crate) fn new(processor: &ComPtr<IAudioProcessor>) -> Self {
        Self {
            audio_out: AtomicBool::new(false),
            active: AtomicBool::new(false),
            live: Claim::new(Live {
                state: State::default(),
                processor: processor.clone(),
            }),
        }
    }

    /// The claim, waiting out an entry in flight, with no check of the audio
    /// half (reads and a call on the plugin's other interfaces): `None` once
    /// torn down or dead.
    pub(crate) fn claim(&self) -> Option<ClaimGuard<'_, Live>> {
        self.live.claim()
    }

    /// Whether `setActive(1)` has run and `setActive(0)` has not.
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Records an activation and the bus layout read with it, under the claim
    /// (`live` proves it).
    pub(crate) fn record_activation(
        &self,
        live: &mut ClaimGuard<'_, Live>,
        active: bool,
        buses: Option<Arrangement>,
    ) {
        live.state.buses = buses;
        self.active.store(active, Ordering::Release);
    }

    /// The claim for one audio-side entry: [`Vst3Error::Poisoned`] when a
    /// holder panicked, [`Vst3Error::NotInitialized`] once torn down or while a
    /// control thread holds it. Never blocks.
    fn try_enter(&self) -> Result<ClaimGuard<'_, Live>, Vst3Error> {
        self.live.try_claim().ok_or_else(|| self.refusal())
    }

    fn refusal(&self) -> Vst3Error {
        if self.live.is_dead() {
            Vst3Error::Poisoned
        } else {
            Vst3Error::NotInitialized
        }
    }

    /// The claim, for a control-side change: waits out an entry in flight.
    /// Refused with [`Vst3Error::Poisoned`] when a holder panicked,
    /// [`Vst3Error::NotInitialized`] once torn down, and [`Vst3Error::Busy`]
    /// while an audio half exists.
    pub(crate) fn lock_for_control(&self) -> Result<ClaimGuard<'_, Live>, Vst3Error> {
        let live = self.live.claim().ok_or_else(|| self.refusal())?;
        if self.audio_out.load(Ordering::Acquire) {
            return Err(Vst3Error::Busy);
        }
        Ok(live)
    }

    /// Ends the claim, waiting out an entry in flight, and returns whether the
    /// plugin was active (and is now recorded as not). No entry runs after
    /// this returns, and a second call finds nothing to end.
    pub(crate) fn teardown(&self) -> bool {
        drop(self.live.take());
        self.active.swap(false, Ordering::AcqRel)
    }

    /// Takes the one audio half, if the plugin is set up and active and none
    /// is out.
    pub(crate) fn take_audio_side(self: &Arc<Self>) -> Option<AudioSide> {
        let live = self.claim()?;
        let ready = self.is_active() && live.state.setup.is_some();
        if !ready || self.audio_out.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(AudioSide {
            shared: self.clone(),
        })
    }
}

/// The audio-thread half of a plugin, taken once from
/// `Plugin::audio_side`, which hands it out only
/// for a plugin that is set up and active.
///
/// It names no main-thread entry, so a processor that owns it cannot make a
/// main-thread call, and it owns no reference to the plugin, so dropping it
/// (on any thread) destroys nothing. Each entry runs under the instance's
/// claim and is refused with [`Vst3Error::NotInitialized`], without reaching
/// the plugin, once the plugin has been torn down or deactivated.
pub struct AudioSide {
    shared: Arc<Shared>,
}

opaque_debug!(AudioSide);

impl Drop for AudioSide {
    fn drop(&mut self) {
        self.shared.audio_out.store(false, Ordering::Release);
    }
}

/// One block's worth of everything `process` is handed.
///
/// The buses are bound to the block's channel slices
/// ([`BusBuffers::bind_inputs_f32`](crate::BusBuffers::bind_inputs_f32)) and the lists
/// are the host's COM objects; the lifetime ties every pointer the plugin sees
/// to the borrows that produced it.
pub struct ProcessIo<'a> {
    /// `kRealtime`, `kPrefetch` or `kOffline` for this block.
    pub mode: ProcessMode,
    /// The block length in frames.
    pub num_samples: usize,
    /// The input buses.
    pub inputs: BoundBuses<'a>,
    /// The output buses.
    pub outputs: BoundBuses<'a>,
    /// The events the plugin reads.
    pub input_events: &'a ComWrapper<EventList>,
    /// The events the plugin writes.
    pub output_events: &'a ComWrapper<EventList>,
    /// The parameter changes the plugin reads. A block with none passes an
    /// empty list, never null: `inputParameterChanges` is not annotated
    /// optional in `ProcessData` (ivstaudioprocessor.h).
    pub input_params: &'a ComWrapper<ParameterChangesImpl>,
    /// The parameter changes the plugin writes.
    pub output_params: &'a ComWrapper<ParameterChangesImpl>,
    /// The transport and time.
    pub context: &'a mut ProcessContext,
    /// The setup the plugin was given, which the conformance observer reports
    /// beside the data.
    pub setup: &'a ProcessSetup,
}

impl AudioSide {
    /// `IAudioProcessor::setProcessing`.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::NotInitialized`] if the plugin is gone or not active, or the
    /// plugin's refusal.
    pub fn set_processing(&mut self, on: bool) -> Result<(), Vst3Error> {
        let live = self.shared.try_enter()?;
        if !self.shared.is_active() {
            return Err(Vst3Error::NotInitialized);
        }
        // SAFETY: a plain call on the live processor, under the claim.
        check(unsafe { live.processor.setProcessing(u8::from(on)) })
    }

    /// Renders one block through `IAudioProcessor::process`.
    ///
    /// Allocation-free and wait-free: it takes the instance's claim with a
    /// `try_claim`, so it never waits on a control thread.
    ///
    /// # Errors
    ///
    /// - [`Vst3Error::NotInitialized`]: the plugin is gone, not active or not set
    ///   up, or a control thread holds the claim.
    /// - [`Vst3Error::InvalidArgument`]: the buses are bound at a width other
    ///   than the setup's `symbolicSampleSize` (or the input and output widths
    ///   differ), the block is longer than the setup's `maxSamplesPerBlock`,
    ///   longer than a bound channel or the scratch, or does not fit the ABI's
    ///   `int32`.
    /// - otherwise the plugin's refusal.
    pub fn process(&mut self, mut io: ProcessIo<'_>) -> Result<(), Vst3Error> {
        let live = self.shared.try_enter()?;
        let setup = match (self.shared.is_active(), live.state.setup) {
            (true, Some(setup)) => setup,
            _ => return Err(Vst3Error::NotInitialized),
        };
        let arrangement_fits = live
            .state
            .buses
            .as_ref()
            .is_none_or(|a| a.matches(io.inputs.bus_channels(), io.outputs.bus_channels()));
        if !arrangement_fits {
            return Err(Vst3Error::InvalidArgument);
        }
        if io.inputs.width() != io.outputs.width()
            || io.inputs.width().symbolic_size() != setup.symbolic_size
        {
            return Err(Vst3Error::InvalidArgument);
        }
        if io.num_samples > setup.max_block
            || io.num_samples > io.inputs.capacity()
            || io.num_samples > io.outputs.capacity()
        {
            return Err(Vst3Error::InvalidArgument);
        }
        let num_samples = i32::try_from(io.num_samples).map_err(|_| Vst3Error::InvalidArgument)?;
        let num_inputs =
            i32::try_from(io.inputs.num_buses()).map_err(|_| Vst3Error::InvalidArgument)?;
        let num_outputs =
            i32::try_from(io.outputs.num_buses()).map_err(|_| Vst3Error::InvalidArgument)?;

        let mut data = ProcessData {
            processMode: io.mode.to_vst3(),
            symbolicSampleSize: io.inputs.width().symbolic_size(),
            numSamples: num_samples,
            numInputs: num_inputs,
            numOutputs: num_outputs,
            inputs: io.inputs.finish(),
            outputs: io.outputs.finish(),
            inputParameterChanges: param_changes_ptr(io.input_params),
            outputParameterChanges: param_changes_ptr(io.output_params),
            inputEvents: event_list_ptr(io.input_events),
            outputEvents: event_list_ptr(io.output_events),
            processContext: io.context,
        };

        #[cfg(feature = "conformance")]
        crate::conformance::observe(&data, io.setup);
        #[cfg(not(feature = "conformance"))]
        let _ = io.setup;

        // SAFETY: every pointer in `data` outlives the call and is valid for its field:
        // the bus tables point at slices of at least `numSamples` samples (checked
        // above), and the lists and context are `io`'s borrows. Input channels are
        // `*mut` in the ABI but shared slices here; the plugin must not write them.
        let result = check(unsafe { live.processor.process(&mut data) });
        drop(live);
        result
    }
}

/// What a parameter flush is handed: the changes alone, with the process mode.
pub struct FlushIo<'a> {
    /// `kRealtime`, `kPrefetch` or `kOffline`, as the blocks go in.
    pub mode: ProcessMode,
    /// The parameter changes to deliver.
    pub input_params: &'a ComWrapper<ParameterChangesImpl>,
    /// The parameter changes the plugin writes, which a flush ignores.
    pub output_params: &'a ComWrapper<ParameterChangesImpl>,
}

impl AudioSide {
    /// `IAudioProcessor::process` with no audio, to deliver parameter changes
    /// to a plugin that is active but not processing (the portal's FAQ,
    /// "Processing": the host "can call process without buffers (numInputs and
    /// numOutputs are zeroed)... to flush parameters"). `numSamples` is 0, no
    /// bus is passed, and no event list or process context is: a flush carries
    /// nothing else, and the lists a block filled still hold that block's.
    ///
    /// Same claim and refusals as [`process`](Self::process).
    ///
    /// # Errors
    ///
    /// [`Vst3Error::NotInitialized`] if the plugin is gone, not active or not set
    /// up, or a control thread holds the claim; otherwise the plugin's refusal.
    pub fn flush(&mut self, io: FlushIo<'_>) -> Result<(), Vst3Error> {
        let live = self.shared.try_enter()?;
        let setup = match (self.shared.is_active(), live.state.setup) {
            (true, Some(setup)) => setup,
            _ => return Err(Vst3Error::NotInitialized),
        };
        let mut data = ProcessData {
            processMode: io.mode.to_vst3(),
            symbolicSampleSize: setup.symbolic_size,
            numSamples: 0,
            numInputs: 0,
            numOutputs: 0,
            inputs: std::ptr::null_mut(),
            outputs: std::ptr::null_mut(),
            inputParameterChanges: param_changes_ptr(io.input_params),
            outputParameterChanges: param_changes_ptr(io.output_params),
            inputEvents: std::ptr::null_mut(),
            outputEvents: std::ptr::null_mut(),
            processContext: std::ptr::null_mut(),
        };
        // SAFETY: `data` holds no bus, event list or context, and the two
        // parameter lists are `io`'s borrows, alive for the call.
        let result = check(unsafe { live.processor.process(&mut data) });
        drop(live);
        result
    }
}

impl std::fmt::Debug for FlushIo<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlushIo")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ProcessIo<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessIo")
            .field("mode", &self.mode)
            .field("num_samples", &self.num_samples)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus_buffers::BusBuffers;
    use crate::types::SampleWidth;
    use std::sync::atomic::AtomicUsize;
    use tutti_buffers::ChannelLayout;
    use tutti_plugin_types::PointPool;
    use vst3::Class;
    use vst3::Steinberg::Vst::{ProcessSetup, SpeakerArrangement};
    use vst3::Steinberg::{kResultOk, tresult};

    const BLOCK: usize = 64;

    /// A processor that counts the calls that reach it.
    struct Stub {
        processed: AtomicUsize,
        processing: AtomicUsize,
    }

    impl Class for Stub {
        type Interfaces = (IAudioProcessor,);
    }

    impl IAudioProcessorTrait for Stub {
        unsafe fn setBusArrangements(
            &self,
            _: *mut SpeakerArrangement,
            _: i32,
            _: *mut SpeakerArrangement,
            _: i32,
        ) -> tresult {
            kResultOk
        }
        unsafe fn getBusArrangement(&self, _: i32, _: i32, _: *mut SpeakerArrangement) -> tresult {
            kResultOk
        }
        unsafe fn canProcessSampleSize(&self, _: i32) -> tresult {
            kResultOk
        }
        unsafe fn getLatencySamples(&self) -> u32 {
            0
        }
        unsafe fn setupProcessing(&self, _: *mut ProcessSetup) -> tresult {
            kResultOk
        }
        unsafe fn setProcessing(&self, _: u8) -> tresult {
            self.processing.fetch_add(1, Ordering::SeqCst);
            kResultOk
        }
        unsafe fn process(&self, _: *mut ProcessData) -> tresult {
            self.processed.fetch_add(1, Ordering::SeqCst);
            kResultOk
        }
        unsafe fn getTailSamples(&self) -> u32 {
            0
        }
    }

    /// A shared state over a stub processor, set up for `size` and `max_block`
    /// and active, with its audio half taken.
    struct Rig {
        stub: ComWrapper<Stub>,
        shared: Arc<Shared>,
        side: AudioSide,
        input: BusBuffers,
        output: BusBuffers,
        in_events: ComWrapper<EventList>,
        out_events: ComWrapper<EventList>,
        in_params: ComWrapper<ParameterChangesImpl>,
        out_params: ComWrapper<ParameterChangesImpl>,
    }

    fn rig(width: SampleWidth, max_block: usize) -> Rig {
        let stub = ComWrapper::new(Stub {
            processed: AtomicUsize::new(0),
            processing: AtomicUsize::new(0),
        });
        let ptr = stub.to_com_ptr::<IAudioProcessor>().unwrap();
        let shared = Arc::new(Shared::new(&ptr));
        {
            let mut live = shared.claim().expect("live");
            shared.record_activation(&mut live, true, None);
            live.state.setup = Some(Setup {
                symbolic_size: width.symbolic_size(),
                max_block,
            });
        }
        let side = shared.take_audio_side().expect("set up and active");
        Rig {
            stub,
            shared,
            side,
            input: BusBuffers::new(width, &[2], ChannelLayout::from(2u16), BLOCK).unwrap(),
            output: BusBuffers::new(width, &[2], ChannelLayout::from(2u16), BLOCK).unwrap(),
            in_events: EventList::input(),
            out_events: EventList::output(),
            in_params: ParameterChangesImpl::with_queues(1, 16, PointPool::default()),
            out_params: ParameterChangesImpl::with_queues(1, 16, PointPool::default()),
        }
    }

    impl Rig {
        /// One block of `n` frames at 32-bit width, through the audio half.
        fn run_f32(&mut self, n: usize) -> Result<(), Vst3Error> {
            let (a, b) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
            let (mut c, mut d) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
            let ins: [&[f32]; 2] = [&a, &b];
            let mut outs: [&mut [f32]; 2] = [&mut c, &mut d];
            let mut context = crate::sdk::zeroed();
            let setup: ProcessSetup = ProcessSetup {
                processMode: 0,
                symbolicSampleSize: 0,
                maxSamplesPerBlock: BLOCK as i32,
                sampleRate: 48_000.0,
            };
            let io = ProcessIo {
                mode: ProcessMode::Realtime,
                num_samples: n,
                inputs: self.input.bind_inputs_f32(&ins)?,
                outputs: self.output.bind_outputs_f32(&mut outs)?,
                input_events: &self.in_events,
                output_events: &self.out_events,
                input_params: &self.in_params,
                output_params: &self.out_params,
                context: &mut context,
                setup: &setup,
            };
            self.side.process(io)
        }

        fn set_active(&self, active: bool) {
            let mut live = self.shared.claim().expect("live");
            let buses = live.state.buses.take();
            self.shared.record_activation(&mut live, active, buses);
        }

        fn processed(&self) -> usize {
            self.stub.processed.load(Ordering::SeqCst)
        }
    }

    /// A block that fits is delivered exactly once.
    #[test]
    fn a_block_that_fits_reaches_the_plugin() {
        let mut rig = rig(SampleWidth::F32, BLOCK);
        assert_eq!(rig.run_f32(BLOCK), Ok(()));
        assert_eq!(rig.processed(), 1);
    }

    /// A block longer than the setup's `maxSamplesPerBlock` is refused without
    /// reaching the plugin.
    #[test]
    fn a_block_longer_than_the_setup_is_refused() {
        let mut rig = rig(SampleWidth::F32, 32);
        assert_eq!(rig.run_f32(33), Err(Vst3Error::InvalidArgument));
        assert_eq!(rig.processed(), 0);
        assert_eq!(rig.run_f32(32), Ok(()));
    }

    /// A block longer than a bound channel is refused (the channels here are
    /// `BLOCK` long, the setup allows more).
    #[test]
    fn a_block_longer_than_a_channel_is_refused() {
        let mut rig = rig(SampleWidth::F32, 1000);
        assert_eq!(rig.run_f32(BLOCK + 36), Err(Vst3Error::InvalidArgument));
        assert_eq!(rig.processed(), 0);
    }

    /// Buses bound at 32 bits on a plugin set up for `kSample64` are refused:
    /// the plugin would read 8-byte samples from 4-byte data.
    #[test]
    fn a_width_other_than_the_setups_is_refused() {
        let mut rig = rig(SampleWidth::F64, BLOCK);
        // The scratch is built for 64-bit, so binding f32 is refused outright:
        assert_eq!(rig.run_f32(8), Err(Vst3Error::InvalidArgument));
        // and a scratch built for 32-bit on that 64-bit setup is refused by the
        // call.
        rig.input =
            BusBuffers::new(SampleWidth::F32, &[2], ChannelLayout::from(2u16), BLOCK).unwrap();
        rig.output =
            BusBuffers::new(SampleWidth::F32, &[2], ChannelLayout::from(2u16), BLOCK).unwrap();
        assert_eq!(rig.run_f32(8), Err(Vst3Error::InvalidArgument));
        assert_eq!(rig.processed(), 0);
    }

    /// Input and output buses of different widths are refused.
    #[test]
    fn unequal_input_and_output_widths_are_refused() {
        let mut rig = rig(SampleWidth::F32, BLOCK);
        rig.output =
            BusBuffers::new(SampleWidth::F64, &[2], ChannelLayout::from(2u16), BLOCK).unwrap();
        let (a, b) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
        let (mut c, mut d) = (vec![0.0f64; BLOCK], vec![0.0f64; BLOCK]);
        let ins: [&[f32]; 2] = [&a, &b];
        let mut outs: [&mut [f64]; 2] = [&mut c, &mut d];
        let mut context = crate::sdk::zeroed();
        let setup = ProcessSetup {
            processMode: 0,
            symbolicSampleSize: 0,
            maxSamplesPerBlock: BLOCK as i32,
            sampleRate: 48_000.0,
        };
        let io = ProcessIo {
            mode: ProcessMode::Realtime,
            num_samples: 8,
            inputs: rig.input.bind_inputs_f32(&ins).unwrap(),
            outputs: rig.output.bind_outputs_f64(&mut outs).unwrap(),
            input_events: &rig.in_events,
            output_events: &rig.out_events,
            input_params: &rig.in_params,
            output_params: &rig.out_params,
            context: &mut context,
            setup: &setup,
        };
        assert_eq!(rig.side.process(io), Err(Vst3Error::InvalidArgument));
        assert_eq!(rig.processed(), 0);
    }

    /// Once torn down, the half answers `NotInitialized` and nothing reaches the
    /// plugin; a deactivated plugin answers the same.
    #[test]
    fn a_torn_down_or_deactivated_plugin_is_not_called() {
        let mut rig = rig(SampleWidth::F32, BLOCK);
        rig.set_active(false);
        assert_eq!(rig.run_f32(8), Err(Vst3Error::NotInitialized));
        rig.set_active(true);
        assert_eq!(rig.run_f32(8), Ok(()));
        assert!(rig.shared.teardown());
        assert_eq!(rig.run_f32(8), Err(Vst3Error::NotInitialized));
        assert_eq!(
            rig.side.set_processing(true),
            Err(Vst3Error::NotInitialized)
        );
        assert_eq!(rig.processed(), 1);
        assert_eq!(rig.stub.processing.load(Ordering::SeqCst), 0);
    }

    /// The audio half is handed out once, only for a set-up, active plugin; the
    /// control side is refused while it lives and free again after.
    #[test]
    fn the_audio_half_is_taken_once_and_blocks_control_changes() {
        let rig = rig(SampleWidth::F32, BLOCK);
        assert!(rig.shared.take_audio_side().is_none());
        assert_eq!(
            rig.shared.lock_for_control().map(drop),
            Err(Vst3Error::Busy)
        );
        // A state that is not ready never yields a half.
        let stub = ComWrapper::new(Stub {
            processed: AtomicUsize::new(0),
            processing: AtomicUsize::new(0),
        });
        let fresh = Arc::new(Shared::new(&stub.to_com_ptr::<IAudioProcessor>().unwrap()));
        assert!(fresh.take_audio_side().is_none(), "not set up, not active");
        let Rig { shared, side, .. } = rig;
        drop(side);
        assert!(
            shared.lock_for_control().is_ok(),
            "free once the half drops"
        );
        assert!(shared.take_audio_side().is_some(), "and may be taken again");
    }

    /// While a control thread holds the claim, the audio half does not wait: it
    /// answers `NotInitialized`.
    #[test]
    fn the_audio_half_never_waits_on_a_control_thread() {
        let mut rig = rig(SampleWidth::F32, BLOCK);
        let shared = rig.shared.clone();
        let held = shared.claim().expect("live");
        assert_eq!(rig.run_f32(8), Err(Vst3Error::NotInitialized));
        drop(held);
        assert_eq!(rig.run_f32(8), Ok(()));
    }

    /// Teardown says whether the plugin was active and ends the claim for good:
    /// no control or audio entry runs after it, and a second teardown finds
    /// nothing.
    #[test]
    fn teardown_ends_the_claim_and_says_whether_it_was_active() {
        let mut rig = rig(SampleWidth::F32, BLOCK);
        assert!(rig.shared.teardown());
        assert!(rig.shared.claim().is_none(), "gone for good");
        assert!(!rig.shared.is_active());
        assert_eq!(
            rig.shared.lock_for_control().map(drop),
            Err(Vst3Error::NotInitialized)
        );
        assert_eq!(rig.run_f32(8), Err(Vst3Error::NotInitialized));
        assert!(!rig.shared.teardown(), "nothing left to tear down");
    }

    /// A panic under the claim poisons it: both sides answer `Poisoned`,
    /// `is_active` still answers, the teardown reports the activation once, and
    /// afterwards the answer is `NotInitialized`.
    #[test]
    fn a_dead_claim_answers_poisoned_until_the_teardown_ends_it() {
        let mut rig = rig(SampleWidth::F32, BLOCK);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _live = rig.shared.claim().expect("live");
            panic!("host code panics under the claim");
        }));
        assert!(caught.is_err());
        assert_eq!(rig.run_f32(8), Err(Vst3Error::Poisoned));
        assert_eq!(rig.side.set_processing(true), Err(Vst3Error::Poisoned));
        assert_eq!(
            rig.shared.lock_for_control().map(drop),
            Err(Vst3Error::Poisoned)
        );
        assert!(rig.shared.is_active(), "active reads right on a dead claim");
        assert!(rig.shared.teardown(), "was active, reported once");
        assert!(!rig.shared.is_active());
        assert!(!rig.shared.teardown());
        assert_eq!(rig.run_f32(8), Err(Vst3Error::NotInitialized));
        assert_eq!(
            rig.shared.lock_for_control().map(drop),
            Err(Vst3Error::NotInitialized)
        );
        assert_eq!(rig.processed(), 0);
    }

    /// A block racing a teardown either runs whole or is refused: the plugin
    /// is never called once the teardown returned, and a call that began
    /// finishes before it does.
    #[test]
    fn a_block_racing_a_teardown_runs_whole_or_is_refused() {
        let mut rig = rig(SampleWidth::F32, BLOCK);
        let shared = rig.shared.clone();
        let blocks = std::thread::scope(|s| {
            let audio = s.spawn(|| {
                let mut ran = 0;
                loop {
                    match rig.run_f32(8) {
                        Ok(()) => ran += 1,
                        Err(Vst3Error::NotInitialized) => {
                            if rig.shared.claim().is_none() {
                                return ran;
                            }
                        }
                        Err(other) => panic!("a block is whole or refused: {other:?}"),
                    }
                }
            });
            assert!(shared.teardown());
            audio.join().expect("audio thread")
        });
        assert_eq!(
            rig.processed(),
            blocks,
            "every admitted block reached the plugin once"
        );
        assert_eq!(rig.run_f32(8), Err(Vst3Error::NotInitialized));
        assert_eq!(
            rig.processed(),
            blocks,
            "nothing is called after the teardown"
        );
    }

    /// A binding whose buses are not the plugin's arrangement is refused: a
    /// scratch with fewer channels than the plugin has would let it read past
    /// the pointer table.
    #[test]
    fn buses_that_are_not_the_plugins_arrangement_are_refused() {
        let mut rig = rig(SampleWidth::F32, BLOCK);
        rig.shared.claim().expect("live").state.buses = Some(Arrangement {
            inputs: vec![3].into(),
            outputs: vec![3].into(),
        });
        assert_eq!(rig.run_f32(8), Err(Vst3Error::InvalidArgument));
        assert_eq!(rig.processed(), 0);
        rig.shared.claim().expect("live").state.buses = Some(Arrangement {
            inputs: vec![2].into(),
            outputs: vec![2].into(),
        });
        assert_eq!(rig.run_f32(8), Ok(()));
        // A direction in which the plugin has no audio bus accepts any binding.
        rig.shared.claim().expect("live").state.buses = Some(Arrangement {
            inputs: Box::new([]),
            outputs: vec![2].into(),
        });
        assert_eq!(rig.run_f32(8), Ok(()));
    }
}
