//! [`Lifecycle`] against a [`FakePlugin`]: the interpreter's contract without
//! a plugin.
//!
//! Most tests hold the Processor here (a local `Hand`). The lent-Processor
//! paths (a recall that answers `Lent`, a parked plan resumed by
//! `Event::Returned`) run with a threaded one, whose realtime end the test
//! polls itself (`Rig::threaded`).

use super::*;
use crate::Recovery;
use crate::fake::{Call, FakeLoaded, FakePlugin};
use crate::{
    AtomicRequestFlags, HostOp, MaxBlock, PluginError, PluginIo, PluginResult, PluginTail,
    ProcessContext, ProcessOutput, Processor, RenderMode, Request, Samples, Stage, taken,
};
use tutti_buffers::FloatWidth;
use tutti_buffers::InputOutput;
use tutti_units::Normalized;
use tutti_units::ParamId;

/// The test process's main thread: nextest runs each test in its own process,
/// so the claim always succeeds.
fn main() -> MainThreadMarker {
    tutti_fixtures::main_thread()
}

fn prepare(rate: f64) -> Prepare {
    Prepare::new(SampleRate::of(rate), MaxBlock::of(Samples::new(64)))
        .with_format(FloatWidth::F32)
        .with_mode(RenderMode::Realtime)
}

struct Rig {
    plugin: FakePlugin,
    lifecycle: Lifecycle<FakeLoaded>,
    state: State,
    hand: Hand<<FakeLoaded as Loaded>::Processor>,
    notices: Vec<Notice>,
}

impl Rig {
    fn new() -> Self {
        let (plugin, loaded) = FakePlugin::new();
        Self {
            plugin,
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<FakeLoaded>::initial_state(),
            hand: Hand::local(),
            notices: Vec::new(),
        }
    }

    /// A rig whose Processor goes to a realtime end the test holds.
    fn threaded() -> (Self, tutti_rt::Rt<<FakeLoaded as Loaded>::Processor>) {
        let (hand, rt) = Hand::threaded();
        (
            Self {
                hand,
                ..Self::new()
            },
            rt,
        )
    }

    fn drive(&mut self, event: Event) {
        let state = self.state.clone();
        let notices = &mut self.notices;
        self.state = self
            .lifecycle
            .drive(state, event, &mut self.hand, main(), |n| notices.push(n));
    }

    fn op(&mut self, op: HostOp) {
        self.drive(Event::Op(op));
    }

    /// Delivers what the plugin raised, then the idle pass.
    fn tick(&mut self) {
        let requests: Vec<_> = taken(self.plugin.request_flags()).collect();
        for request in requests {
            self.drive(Event::Request(request));
        }
        self.drive(Event::Tick);
    }

    fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    /// Renders one block of `frames` through the held Processor.
    fn render(&mut self, frames: usize) -> PluginResult<()> {
        let input = vec![0.0f32; frames];
        let mut left = vec![1.0f32; frames];
        let mut right = vec![1.0f32; frames];
        let inputs = [input.as_slice(), input.as_slice()];
        let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
        let ctx = ProcessContext::new();
        let mut out = ProcessOutput::default();
        let mut io = PluginIo {
            buffer: InputOutput::from_slices(Samples::new(inputs[0].len()), &inputs, &mut outputs)
                .expect("equal lengths"),
            ctx: &ctx,
            out: &mut out,
        };
        self.lifecycle
            .processor()
            .expect("the Processor is held")
            .process(&mut io)
    }
}

fn activations(calls: &[Call]) -> usize {
    calls
        .iter()
        .filter(|c| matches!(c, Call::Activate(..)))
        .count()
}

/// **An activation splits the plugin, reads latency, tail and layout, and
/// leaves the Processor in the seat.** The reads are the Controller's, after
/// `activate` and before anything renders.
#[test]
fn activation_splits_the_plugin_and_reads_its_figures() {
    let mut rig = Rig::new();
    rig.plugin.set_latency(Samples::new(32));
    rig.op(HostOp::Activate(prepare(48_000.0)));

    assert_eq!(rig.state.stage(), Stage::Active);
    assert!(matches!(rig.lifecycle, Lifecycle::Active { .. }));
    assert_eq!(
        rig.plugin.calls(),
        [
            Call::Activate(SampleRate::SR_48K, Samples::new(64)),
            Call::Latency,
            Call::Tail,
            Call::Wiring,
        ]
    );
    rig.render(64).unwrap();
    assert_eq!(rig.plugin.calls().last(), Some(&Call::Process(64)));
}

/// **A block with no output channel reaches the Processor with its frames**: a
/// plugin with no audio output (MIDI-only, an analyzer) sees the block's length,
/// through `process_block` as the graph node calls it.
#[test]
fn a_block_with_no_output_channel_reaches_the_processor_with_its_frames() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    let processor = rig.lifecycle.processor().expect("held");
    crate::process_block::<_, f32>(processor, &[], &mut [], Samples::new(96), &ctx, &mut out)
        .expect("a block with no channels renders");
    assert_eq!(rig.plugin.calls().last(), Some(&Call::Process(96)));
}

/// **`process_block` refuses channels of unequal length with `PluginError::Block`
/// and does not enter the Processor.**
#[test]
fn process_block_refuses_ragged_channels_without_entering_the_processor() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    let (mut left, mut right) = ([0.0f32; 64], [0.0f32; 32]);
    let mut outputs = [&mut left[..], &mut right[..]];
    let processor = rig.lifecycle.processor().expect("held");
    let refused = crate::process_block::<_, f32>(
        processor,
        &[],
        &mut outputs,
        Samples::new(64),
        &ctx,
        &mut out,
    );
    assert!(matches!(refused, Err(PluginError::Block(_))), "{refused:?}");
    assert!(
        !rig.plugin
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Process(_)))
    );
}

/// **A refused activation leaves `Refused` with the plugin intact and no half
/// pair; a later rate revives it.** The host is told once.
#[test]
fn a_refused_rate_leaves_the_plugin_loaded_and_a_later_rate_revives_it() {
    let mut rig = Rig::new();
    rig.plugin.refuse_rate(Some(SampleRate::SR_22K05));
    rig.op(HostOp::Activate(prepare(22_050.0)));

    assert!(matches!(
        rig.state.stage(),
        Stage::Refused { rate } if rate == SampleRate::SR_22K05
    ));
    assert!(matches!(rig.lifecycle, Lifecycle::Refused { .. }));
    assert!(rig.lifecycle.processor().is_none(), "no half pair");
    assert!(rig.lifecycle.control().is_some(), "the plugin is intact");
    let notices = rig.take_notices();
    assert!(
        matches!(notices.as_slice(), [Notice::Refused { rate, .. }] if *rate == SampleRate::SR_22K05),
        "{notices:?}"
    );

    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert_eq!(rig.state.stage(), Stage::Active);
    rig.render(64).unwrap();
}

/// **A refusal that hands no plugin back poisons, after the host was told.**
#[test]
fn a_lost_plugin_poisons_and_the_refusal_is_still_told() {
    let mut rig = Rig::new();
    rig.plugin.refuse_rate(Some(SampleRate::SR_8K));
    rig.plugin.lose_plugin_on_refusal(true);
    rig.op(HostOp::Activate(prepare(8_000.0)));

    assert!(rig.lifecycle.is_poisoned());
    assert_eq!(rig.state.stage(), Stage::Poisoned);
    assert!(matches!(
        rig.take_notices().as_slice(),
        [Notice::Refused { .. }]
    ));
    assert!(rig.lifecycle.control().is_none());
    assert!(rig.lifecycle.control_ref().is_none());
}

/// **A panic in `activate` poisons the same way.**
#[test]
fn a_panic_in_activate_poisons() {
    let mut rig = Rig::new();
    rig.plugin.panic_in_activate(true);
    rig.op(HostOp::Activate(prepare(48_000.0)));

    assert!(rig.lifecycle.is_poisoned());
    assert_eq!(rig.state.stage(), Stage::Poisoned);
}

/// **A plugin that will not stop stays active with both halves, and the host
/// is told; the next deactivation takes.**
#[test]
fn stuck_restores_both_halves() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.stick_deactivations(1);
    rig.take_notices();

    rig.op(HostOp::Deactivate);
    assert_eq!(rig.state.stage(), Stage::Active);
    assert!(matches!(
        rig.take_notices().as_slice(),
        [Notice::Stuck { .. }]
    ));
    rig.render(64).unwrap();

    rig.op(HostOp::Deactivate);
    assert_eq!(rig.state.stage(), Stage::Loaded);
    assert!(matches!(rig.lifecycle, Lifecycle::Loaded(_)));
    assert_eq!(
        rig.plugin
            .calls()
            .iter()
            .filter(|c| **c == Call::Deactivate)
            .count(),
        1
    );
}

/// **What a `Loaded` must do with a pair from two activations: refuse it, and
/// hand both halves back.** This pins the fake's implementation of the
/// contract; each format's own `deactivate` carries its own check and test.
#[test]
fn a_pair_from_two_activations_is_refused() {
    let (plugin, loaded) = FakePlugin::new();
    let (p1, c1) = loaded.activate(main(), prepare(48_000.0)).unwrap();
    let c1 = FakeLoaded::deactivate(main(), p1, c1).unwrap();
    let (p1, c1) = c1.activate(main(), prepare(48_000.0)).unwrap();
    let (_, other) = FakePlugin::new();
    let (p2, c2) = other.activate(main(), prepare(48_000.0)).unwrap();

    let stuck = FakeLoaded::deactivate(main(), p1, c2).unwrap_err();
    assert!(stuck.reason.to_string().contains("two activations"));
    // Both halves are back and still pair with their own.
    FakeLoaded::deactivate(main(), p2, c1).expect_err("crossed again");
    drop(plugin);
}

/// **A reconfigure deactivates then activates once, and re-reads.**
#[test]
fn a_reconfigure_is_one_deactivate_and_one_activate() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.clear_calls();

    rig.op(HostOp::Reconfigure(prepare(96_000.0)));
    assert_eq!(rig.state.stage(), Stage::Active);
    let calls = rig.plugin.calls();
    assert_eq!(
        calls,
        [
            Call::Deactivate,
            Call::Activate(SampleRate::SR_96K, Samples::new(64)),
            Call::Latency,
            Call::Tail,
            Call::Wiring,
        ]
    );
    rig.render(64).unwrap();
}

/// **A latency the plugin announces restarts it once and reaches the host as a
/// notice; the restart's own re-read is not a second restart.**
#[test]
fn a_latency_change_restarts_once_and_is_announced() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();

    rig.plugin.set_latency(Samples::new(128));
    Request::LatencyChanged.raise(rig.plugin.request_flags());
    rig.tick();

    assert_eq!(activations(&rig.plugin.calls()), 2);
    let notices = rig.take_notices();
    assert!(
        matches!(notices.as_slice(), [Notice::LatencyChanged(s)] if s.get() == 128),
        "{notices:?}"
    );
    rig.tick();
    assert_eq!(activations(&rig.plugin.calls()), 2, "nothing left to do");
}

/// **A tail that changes is re-read in place, without a restart.**
#[test]
fn a_tail_change_is_read_in_place() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();

    rig.plugin.set_tail(Samples::new(500));
    Request::TailChanged.raise(rig.plugin.request_flags());
    rig.tick();

    assert_eq!(activations(&rig.plugin.calls()), 1);
    assert!(matches!(
        rig.take_notices().as_slice(),
        [Notice::TailChanged(PluginTail::Finite(s))] if s.get() == 500
    ));
}

/// **A failed rebuild leaves `Refused`, tells the host once, and the next
/// activation rebuilds before it activates.**
#[test]
fn a_failed_rebuild_is_refused_and_the_next_activation_rebuilds_first() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();
    rig.plugin.fail_rebuild(true);
    Request::Reload.raise(rig.plugin.request_flags());
    rig.tick();

    assert!(matches!(rig.state.stage(), Stage::Refused { .. }));
    assert!(matches!(
        rig.take_notices().as_slice(),
        [Notice::Refused { .. }]
    ));
    assert!(matches!(
        rig.lifecycle,
        Lifecycle::Refused { rate: None, .. }
    ));

    rig.plugin.fail_rebuild(false);
    rig.plugin.clear_calls();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    let calls = rig.plugin.calls();
    assert_eq!(calls[0], Call::Rebuild, "{calls:?}");
    assert!(matches!(calls[1], Call::Activate(..)));
    assert_eq!(rig.state.stage(), Stage::Active);
}

/// **A parameter set on an active plugin reaches the Processor once, at the
/// head of its next block, in order; on a loaded plugin it is applied now.**
#[test]
fn a_parameter_set_reaches_the_processor_at_the_next_block() {
    let mut rig = Rig::new();
    let (a, b) = (ParamId::new(3), ParamId::new(9));
    rig.plugin.declare(&[(a, false), (b, false)]);
    rig.lifecycle
        .control()
        .unwrap()
        .set_parameter(main(), a, Normalized::of(0.5))
        .expect("a declared, writable parameter");
    assert_eq!(rig.plugin.calls(), [Call::SetParameter(a)]);
    rig.plugin.clear_calls();

    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert_eq!(
        rig.lifecycle
            .control_ref()
            .and_then(|c| c.parameter(main(), a))
            .map(Normalized::get),
        Some(0.5),
        "values survive the split"
    );
    rig.plugin.clear_calls();
    rig.lifecycle
        .control()
        .expect("the plugin is there")
        .set_parameter(main(), a, Normalized::of(0.25))
        .expect("a declared, writable parameter");
    rig.lifecycle
        .control()
        .expect("the plugin is there")
        .set_parameter(main(), b, Normalized::of(0.75))
        .expect("a declared, writable parameter");
    rig.render(64).unwrap();
    rig.render(64).unwrap();
    assert_eq!(
        rig.plugin.calls(),
        [
            Call::SetParameter(a),
            Call::SetParameter(b),
            Call::Edit(a, Normalized::of(0.25)),
            Call::Edit(b, Normalized::of(0.75)),
            Call::Process(64),
            Call::Process(64),
        ]
    );
}

/// **After `Died` no effect reaches the plugin.**
///
/// The property is the core's (`Poisoned` absorbs every later event, tested
/// with `step`); this pins that `drive` then applies nothing.
#[test]
fn nothing_runs_after_died() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.drive(Event::Died);
    assert_eq!(rig.state.stage(), Stage::Poisoned);
    rig.op(HostOp::Reconfigure(prepare(44_100.0)));
    assert_eq!(rig.state.stage(), Stage::Poisoned);
    assert_eq!(
        activations(&rig.plugin.calls()),
        1,
        "nothing runs after Died"
    );
}

/// **The control surface is reachable in every state that has a plugin.**
#[test]
fn control_is_reachable_loaded_active_and_refused() {
    let mut rig = Rig::new();
    assert!(rig.lifecycle.control().is_some());
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(rig.lifecycle.control().is_some());
    assert!(
        rig.lifecycle
            .control_ref()
            .map(|c| c.wiring(main()))
            .is_some()
    );
    let mut refused = Rig::new();
    refused.plugin.refuse_rate(Some(SampleRate::of(1_000.0)));
    refused.op(HostOp::Activate(prepare(1_000.0)));
    assert!(matches!(refused.lifecycle, Lifecycle::Refused { .. }));
    assert!(refused.lifecycle.control().is_some());
}

/// **A plugin lost while its refusal is rolled back is told once, by the
/// refusal.**
#[test]
fn a_plugin_lost_in_a_refused_reconfigure_is_told_by_the_refusal_alone() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();
    rig.plugin.refuse_rate(Some(SampleRate::SR_22K05));
    rig.plugin.lose_plugin_on_refusal(true);
    rig.op(HostOp::Reconfigure(prepare(22_050.0)));

    assert!(rig.lifecycle.is_poisoned());
    let notices = rig.take_notices();
    assert!(
        matches!(
            notices.as_slice(),
            [Notice::Refused {
                recovery: Recovery::Inactive,
                ..
            }]
        ),
        "{notices:?}"
    );
}

/// **State round-trips through the control surface, loaded and active.**
#[test]
fn state_round_trips_through_the_lifecycle() {
    let mut rig = Rig::new();
    rig.lifecycle
        .control()
        .expect("the plugin is there")
        .load_state(main(), b"abc", crate::StateUse::Project)
        .unwrap();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert_eq!(
        rig.lifecycle
            .control()
            .expect("the plugin is there")
            .save_state(main(), crate::StateUse::Project)
            .unwrap(),
        b"abc"
    );
}

/// **A block through the Processor, and the edit drain at its head, neither
/// allocate nor lock.**
#[test]
fn the_processor_renders_without_allocating() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    let frames = 64;
    let input = vec![0.0f32; frames];
    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    let inputs = [input.as_slice(), input.as_slice()];
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    let processor = rig.lifecycle.processor().expect("held");
    let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
    let mut io = PluginIo {
        buffer: InputOutput::from_slices(Samples::new(inputs[0].len()), &inputs, &mut outputs)
            .expect("equal lengths"),
        ctx: &ctx,
        out: &mut out,
    };
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100 {
            processor.process(&mut io).unwrap();
        }
        processor.stop();
    });
    assert_eq!(
        rig.plugin
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Process(_)))
            .count(),
        100
    );
}

/// **Requests raised on another thread, while the main thread takes, arrive
/// once each.**
#[test]
fn requests_raised_while_taking_arrive_once() {
    let flags = std::sync::Arc::new(AtomicRequestFlags::new());
    let raiser = {
        let flags = flags.clone();
        std::thread::spawn(move || {
            Request::Restart.raise(&flags);
            Request::TailChanged.raise(&flags);
        })
    };
    let mut got = Vec::new();
    while got.len() < 2 {
        got.extend(taken(&flags));
        std::thread::yield_now();
    }
    raiser.join().unwrap();
    got.extend(taken(&flags));
    got.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(got, [Request::Restart, Request::TailChanged]);
}

/// **A panic in `deactivate` poisons and the death is stepped.**
#[test]
fn a_panic_in_deactivate_poisons() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.panic_in_deactivate(true);
    rig.op(HostOp::Deactivate);
    assert!(rig.lifecycle.is_poisoned());
    assert_eq!(rig.state.stage(), Stage::Poisoned);
}

/// **A panic in `rebuild` poisons.**
#[test]
fn a_panic_in_rebuild_poisons() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.panic_in_rebuild(true);
    Request::Reload.raise(rig.plugin.request_flags());
    rig.tick();
    assert!(rig.lifecycle.is_poisoned());
    assert_eq!(rig.state.stage(), Stage::Poisoned);
}

/// **A panic out of a read poisons, and the `State` is not lost with the
/// unwind.** `drive` takes the state by value, so a panic that escaped would
/// drop it.
#[test]
fn a_panic_in_a_read_poisons_without_losing_the_state() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.panic_in_latency(true);
    rig.plugin.set_latency(Samples::new(8));
    Request::LatencyChanged.raise(rig.plugin.request_flags());
    rig.tick();
    assert!(rig.lifecycle.is_poisoned());
    assert_eq!(rig.state.stage(), Stage::Poisoned);
}

/// **A deactivation that is stuck, then a reconfigure, ends active at the new
/// rate once the plugin lets go.**
#[test]
fn stuck_then_reconfigure_settles_at_the_new_rate() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.stick_deactivations(1);
    rig.op(HostOp::Reconfigure(prepare(96_000.0)));
    assert_eq!(rig.state.stage(), Stage::Active);
    assert_eq!(
        rig.state.applied().map(|p| p.rate()),
        Some(SampleRate::SR_48K)
    );
    rig.op(HostOp::Reconfigure(prepare(96_000.0)));
    assert_eq!(rig.state.stage(), Stage::Active);
    assert_eq!(
        rig.state.applied().map(|p| p.rate()),
        Some(SampleRate::SR_96K)
    );
    rig.render(64).unwrap();
}

/// **A rebuild that is refused twice stays `Refused` with no rate, and the
/// attempt that takes revives it.**
#[test]
fn a_refused_rebuild_is_retried_with_no_rate() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.plugin.fail_rebuild(true);
    Request::Reload.raise(rig.plugin.request_flags());
    rig.tick();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(matches!(
        rig.lifecycle,
        Lifecycle::Refused { rate: None, .. }
    ));
    rig.plugin.fail_rebuild(false);
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert_eq!(rig.state.stage(), Stage::Active);
}

/// **Writes to one parameter before a block arrive as its latest value, once;
/// a block with nothing new sends nothing.**
#[test]
fn writes_before_a_block_arrive_as_the_latest_value_per_parameter() {
    let mut rig = Rig::new();
    let (a, b) = (ParamId::new(1), ParamId::new(2));
    rig.plugin.declare(&[(a, false), (b, false)]);
    rig.op(HostOp::Activate(prepare(48_000.0)));
    for (id, value) in [(a, 0.1), (b, 0.2), (b, 0.9)] {
        rig.lifecycle
            .control()
            .expect("the plugin is there")
            .set_parameter(main(), id, Normalized::of(value))
            .expect("a declared, writable parameter");
    }
    rig.plugin.clear_calls();
    rig.render(64).unwrap();
    assert_eq!(
        rig.plugin.calls(),
        [
            Call::Edit(a, Normalized::of(0.1)),
            Call::Edit(b, Normalized::of(0.9)),
            Call::Process(64)
        ]
    );
    assert_eq!(
        rig.lifecycle
            .control_ref()
            .and_then(|c| c.parameter(main(), b))
            .map(Normalized::get),
        Some(0.9)
    );
    rig.tick();
    rig.plugin.clear_calls();
    rig.render(64).unwrap();
    assert_eq!(
        rig.plugin.calls(),
        [Call::Process(64)],
        "nothing left to send"
    );
}

/// **A VST3 mapping change reaches the plugin as one rebuild and is told to
/// the host once, and a note-expression change is one notice.**
#[test]
fn a_mapping_change_rebuilds_the_mapping_once() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();
    rig.plugin.clear_calls();
    Request::MidiCcAssignmentChanged.raise(rig.plugin.request_flags());
    Request::NoteExpressionChanged.raise(rig.plugin.request_flags());
    rig.tick();
    rig.tick();
    assert_eq!(rig.plugin.calls(), [Call::RebuildMidiMapping]);
    assert!(matches!(
        rig.take_notices().as_slice(),
        [Notice::MidiMapChanged, Notice::NoteExpressionChanged]
    ));
}

/// **A panic out of the idle flush or a read tells the host the plugin is
/// lost.** Without the notice the lifecycle poisons silently and the host
/// keeps a plugin that renders nothing.
#[test]
fn a_panic_outside_a_transition_is_told_to_the_host() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.take_notices();
    rig.plugin.panic_in_latency(true);
    Request::LatencyChanged.raise(rig.plugin.request_flags());
    rig.tick();
    assert!(rig.lifecycle.is_poisoned());
    let notices = rig.take_notices();
    assert!(
        notices.iter().any(|n| matches!(n, Notice::Lost { .. })),
        "{notices:?}"
    );
}

/// **A panic in a transition is told once, by `Refused`.**
#[test]
fn a_panic_in_a_transition_is_not_told_twice() {
    let mut rig = Rig::new();
    rig.plugin.panic_in_activate(true);
    rig.op(HostOp::Activate(prepare(48_000.0)));
    let notices = rig.take_notices();
    assert!(rig.lifecycle.is_poisoned());
    assert!(notices.iter().any(|n| matches!(n, Notice::Refused { .. })));
    assert!(!notices.iter().any(|n| matches!(n, Notice::Lost { .. })));
}

/// A new realtime end, named as the Processor's way.
fn install(rig: &mut Rig) -> tutti_rt::Rt<<FakeLoaded as Loaded>::Processor> {
    let (successor, rt) = Successor::new();
    rig.hand.install(successor);
    rt
}

/// The realtime end's poll between blocks, with `stop` as its last step.
fn poll(rt: &mut tutti_rt::Rt<<FakeLoaded as Loaded>::Processor>) -> bool {
    rt.poll(Processor::stop).is_some()
}

/// **With a threaded hand an activation lends the Processor, and a rate change
/// recalls it, parks, and resumes when the realtime thread returns it.** The
/// realtime end stops it before it hands it back; the next tick collects it,
/// rejoins, activates at the new rate and lends again.
#[test]
fn a_rate_change_recalls_the_lent_processor_and_lends_the_new_one() {
    let (mut rig, mut rt) = Rig::threaded();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(rig.lifecycle.processor().is_none(), "lent at activation");
    assert!(poll(&mut rt), "the realtime end takes it");

    rig.op(HostOp::Reconfigure(prepare(44_100.0)));
    assert!(rig.state.is_parked(), "waits for the Processor");
    assert!(!rig.plugin.calls().contains(&Call::Deactivate));

    assert!(!poll(&mut rt), "returned");
    assert_eq!(rig.plugin.calls().last(), Some(&Call::Stop));
    rig.tick();
    assert!(!rig.state.is_parked());
    let calls = rig.plugin.calls();
    assert!(calls.contains(&Call::Deactivate));
    assert!(calls.contains(&Call::Activate(SampleRate::SR_44K1, Samples::new(64))));
    assert_eq!(activations(&calls), 2);
    assert!(poll(&mut rt), "the new Processor reaches the realtime end");
}

/// **A recall before the realtime thread took the Processor withdraws it at
/// once:** the plan runs in the same drive, with no tick and no stop.
#[test]
fn a_recall_before_the_realtime_thread_took_it_is_immediate() {
    let (mut rig, mut rt) = Rig::threaded();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    rig.op(HostOp::Reconfigure(prepare(44_100.0)));
    assert!(!rig.state.is_parked());
    let calls = rig.plugin.calls();
    assert_eq!(activations(&calls), 2);
    assert!(
        !calls.contains(&Call::Stop),
        "it never reached the realtime end"
    );
    assert!(poll(&mut rt), "the new one is lent");
}

/// **A lend the handoff refuses keeps the Processor here and is offered again
/// on a tick.** A recall raised with nothing lent refuses the next lend until
/// the realtime end clears it.
#[test]
fn a_refused_lend_is_offered_again_on_a_tick() {
    let (mut rig, mut rt) = Rig::threaded();
    // A stale recall: nothing is lent, so the realtime end clears it at its
    // next poll, and until then a lend is refused.
    assert!(rig.hand.end.as_mut().unwrap().recall().is_none());
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(rig.lifecycle.processor().is_some(), "refused, kept here");
    assert!(!poll(&mut rt), "nothing lent; the stale recall is cleared");
    rig.tick();
    assert!(rig.lifecycle.processor().is_none(), "offered again");
    assert!(poll(&mut rt));
}

/// **An `install` moves the Processor to the new realtime end at the restart
/// that follows, and leaves the old end empty.** The old end stops and returns
/// it at its poll, the tick restarts the plugin, and the new Processor is lent
/// to the end `install` named: the old end holds none, so dropping it frees
/// nothing.
#[test]
fn an_install_lends_the_restarted_processor_to_the_new_end() {
    let (mut rig, mut old) = Rig::threaded();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(poll(&mut old), "the old end holds the Processor");

    let mut new = install(&mut rig);
    assert!(rig.hand.pending());
    rig.op(HostOp::Reconfigure(prepare(44_100.0)));
    assert!(rig.state.is_parked(), "waits for the old end to return it");
    assert!(rig.hand.pending(), "the Processor has not moved yet");
    assert!(!poll(&mut new), "nothing is lent to the new end yet");

    assert!(!poll(&mut old), "the old end hands it back");
    assert_eq!(rig.plugin.calls().last(), Some(&Call::Stop));
    rig.tick();
    assert!(!rig.hand.pending(), "lent through the new end");
    assert!(old.held().is_none(), "the old end holds none");
    assert!(!poll(&mut old), "and is lent none");
    assert!(poll(&mut new), "the restarted Processor is in the new end");
    assert_eq!(activations(&rig.plugin.calls()), 2);
}

/// **A second `install` before the Processor moves replaces the first:** the
/// end the first call returned is never lent to, and the Processor goes to the
/// second.
#[test]
fn a_second_install_replaces_the_first() {
    let (mut rig, mut old) = Rig::threaded();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(poll(&mut old));
    let mut first = install(&mut rig);
    let mut second = install(&mut rig);
    rig.op(HostOp::Reconfigure(prepare(44_100.0)));
    assert!(!poll(&mut old));
    rig.tick();
    rig.tick();
    assert!(!poll(&mut first), "the replaced end is never lent to");
    assert!(first.held().is_none());
    assert!(poll(&mut second), "the Processor went to the last end");
}

/// **A Processor held on the main thread moves at the next lend:** a local
/// hand, whose Processor stays in the seat, becomes a threaded one when
/// `install` is called, and the next tick lends to the end it returned.
#[test]
fn an_install_of_a_held_processor_lends_to_the_new_end_on_the_next_tick() {
    let mut rig = Rig::new();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(rig.lifecycle.processor().is_some(), "held: a local hand");
    let mut new = install(&mut rig);
    rig.tick();
    assert!(!rig.hand.pending());
    assert!(rig.lifecycle.processor().is_none(), "lent");
    assert!(poll(&mut new));
}

/// **A relend moves the Processor with no deactivation and no activation:**
/// the plugin keeps running, the old end returns the Processor at its poll and
/// the same Processor is lent to the new end.
#[test]
fn a_relend_moves_the_processor_without_restarting_the_plugin() {
    let (mut rig, mut old) = Rig::threaded();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(poll(&mut old));
    let mut new = install(&mut rig);
    rig.op(HostOp::Relend);
    assert!(rig.state.is_parked(), "waits for the old end to return it");
    assert!(!poll(&mut old), "the old end hands it back");
    rig.tick();
    assert!(!rig.hand.pending());
    assert!(poll(&mut new), "lent to the new end");
    assert!(old.held().is_none());
    let calls = rig.plugin.calls();
    assert_eq!(activations(&calls), 1, "no second activation");
    assert!(!calls.contains(&Call::Deactivate), "no deactivation");
}

/// **A relend of a plugin that is not active does nothing:** nothing is
/// recalled, parked or lent, and the successor stays pending.
#[test]
fn a_relend_of_a_plugin_that_is_not_active_does_nothing() {
    let mut rig = Rig::new();
    let _new = install(&mut rig);
    rig.op(HostOp::Relend);
    assert!(!rig.state.is_parked());
    assert!(rig.plugin.calls().is_empty());
    assert!(rig.hand.pending(), "the successor stays pending");
}

/// **A relend raised while an activation is in flight does not displace
/// it:** the activation lends at its end, to the successor.
#[test]
fn a_relend_during_an_activation_is_left_to_the_activation() {
    let (mut rig, mut old) = Rig::threaded();
    rig.op(HostOp::Activate(prepare(48_000.0)));
    assert!(poll(&mut old));
    let mut new = install(&mut rig);
    rig.op(HostOp::Reconfigure(prepare(44_100.0)));
    rig.op(HostOp::Relend);
    assert!(!poll(&mut old));
    rig.tick();
    assert!(poll(&mut new), "the reconfigure's lend went to the new end");
    assert_eq!(activations(&rig.plugin.calls()), 2);
}

/// **The fake's explicit answers for the editor, render-mode, request and
/// reported-edit entries of `Control` reach the host through
/// [`Lifecycle::control`] in both states that hold a plugin**: no editor (and
/// the floating window refused by its one default), no live render mode, no
/// requests, no reported edits.
#[test]
fn the_fake_control_answers_nothing_in_either_state() {
    let mut rig = Rig::new();
    for active in [false, true] {
        if active {
            rig.op(HostOp::Activate(prepare(48_000.0)));
            assert!(rig.lifecycle.processor().is_some());
        }
        let main = main();
        let control = rig.lifecycle.control().expect("a plugin is held");

        assert!(!control.set_render_mode(main, RenderMode::Offline));
        assert!(!control.has_editor(main));
        assert!(matches!(
            control.open_floating_editor(main),
            Err(PluginError::Editor(_))
        ));
        control.close_editor(main);
        control.editor_idle(main);

        let mut requests = Vec::new();
        control.take_requests(main, &mut requests);
        assert!(requests.is_empty());
        let mut edits = Vec::new();
        control.take_reported_edits(main, &mut edits);
        assert!(edits.is_empty());
    }
}

/// **A plugin its shell activated itself is in the state a driven activation
/// leaves, with its Processor lent.** `adopt` steps the pure core with the
/// replies an activation gets, so the stage, the `Prepare`, the latency, the
/// tail and the layout are what `drive` records, and the Processor is on the
/// realtime thread's end, not in the seat.
#[test]
fn an_adopted_plugin_is_in_the_state_a_driven_activation_leaves() {
    let mut driven = Rig::new();
    driven.plugin.set_latency(Samples::new(32));
    driven.op(HostOp::Activate(prepare(48_000.0)));

    let (plugin, loaded) = FakePlugin::new();
    plugin.set_latency(Samples::new(32));
    let wiring = Control::wiring(&loaded, main());
    let (processor, controller) = match loaded.activate(main(), prepare(48_000.0)) {
        Ok(halves) => halves,
        Err(_) => panic!("the fake activates"),
    };
    let (mut hand, mut rt) = Hand::threaded();
    let (lifecycle, state) = Lifecycle::<FakeLoaded>::adopt(
        processor,
        controller,
        prepare(48_000.0),
        &wiring,
        &mut hand,
    );

    assert_eq!(state.stage(), driven.state.stage());
    assert_eq!(state.applied(), driven.state.applied());
    assert_eq!(state.latency(), Some(Samples::new(32)));
    assert_eq!(state.latency(), driven.state.latency());
    assert_eq!(state.tail(), driven.state.tail());
    assert!(state.tail().is_some());
    assert_eq!(state.layout(), driven.state.layout());
    assert!(state.layout().is_some());
    assert!(state.is_quiet(), "no plan is left under way");
    assert!(matches!(
        lifecycle,
        Lifecycle::Active {
            seat: Seat::Lent,
            ..
        }
    ));
    assert!(
        rt.poll(Processor::stop).is_some(),
        "lent to the realtime end"
    );
}
