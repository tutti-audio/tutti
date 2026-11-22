//! The allocation gate the callback module promises for [`process_audio`] and
//! `OutputBlock`.
//!
//! The docs state that the RT callback "must not allocate, lock, or block",
//! and that its buffers are sized once at stream build to `MAX_FRAMES`. This
//! file gates that.
//!
//! # Why it is here and not in `src/`
//!
//! `assert_no_alloc` observes nothing unless `#[global_allocator] =
//! RtAllocCheck` is installed, and that can only be declared at the root of a
//! *binary*. The crate's unit-test binary declares none, so an
//! `assert_no_alloc` gate written in `src/` passes whether or not the callback
//! allocated. This binary is therefore the allocation gate on
//! `process_audio`. **Add new allocation gates here, never in `src/`.**
//!
//! # The fixture has to render
//!
//! A graph with nothing wired renders silence without running a node — an
//! allocation gate around that proves nothing. The graph below is a sine
//! through a filter into both device channels, so the render
//! path runs real nodes, hands buffers between them, and folds to the device
//! width. `renders_something_to_gate` asserts that, so the gates cannot pass
//! by rendering nothing.

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_buffers::MaxBlock;
use tutti_engine::{AudioCallbackState, Engine, MAX_FRAMES, OutputBlock, process_audio};
use tutti_graph::{Edge, InPort, OutPort, PortSource};
use tutti_graph::{Editor, MeterReader, Prepare, TapReader};
use tutti_graph::{MeterId, MeterSpec, NodeKey, TapId, TapOptions};
use tutti_nodes::testing::Osc;
use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti_timeline::{MotionCommand, Transport};
use tutti_units::{Bpm, Hz, Q, SampleRate, Samples};
const SAMPLE_RATE: f64 = 48_000.0;

/// Build an engine + transport pair whose graph actually renders, rolling.
///
/// Mirrors `stream/fixtures.rs`; it is kept here so every line this binary
/// runs before the gate is auditable in one file.
fn rolling_state() -> (Transport, AudioCallbackState) {
    let (transport, state, _, _) = rolling_state_with(false);
    (transport, state)
}

/// [`rolling_state`], with a graph tap on both outputs when `tap` (the reader
/// is returned, and must be held), and the main output meter declared on the
/// outputs (off until enabled).
fn rolling_state_with(
    tap: bool,
) -> (
    Transport,
    AudioCallbackState,
    MeterReader,
    Option<TapReader>,
) {
    let transport = Transport::new(SampleRate::of(SAMPLE_RATE));

    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    let (source, filter) = (NodeKey(1), NodeKey(2));
    ed.insert(source, "sine", Osc::sine(Hz::new(220.0)));
    ed.insert(
        filter,
        "filter",
        SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                .freq(Hz::new(2_000.0))
                .q(Q::new(0.7)),
        ),
    );
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: filter,
            port: 0,
        },
        vec![Edge::Direct(PortSource::Node(OutPort {
            node: source,
            port: 0,
        }))],
    );
    ed.spec_mut().topology.set_outputs(vec![
        PortSource::Node(OutPort {
            node: filter,
            port: 0,
        });
        2
    ]);
    let reader = tap.then(|| {
        let sources: Vec<_> = ed
            .spec()
            .topology
            .outputs
            .iter()
            .flatten()
            .copied()
            .collect();
        ed.open_tap(
            TapId::of(1),
            sources,
            TapOptions::new(Samples::new(MAX_FRAMES)),
        )
        .expect("the sources exist")
    });
    let meter = ed
        .open_meter(MeterId::of(1), MeterSpec::Outputs)
        .expect("the sources exist");
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    // Every commit is sent; the editor only has to outlive the engine's
    // setup. Leaked like the fixtures in `support`, for the same reason.
    Box::leak(Box::new(ed));
    let state = AudioCallbackState::new(engine);

    transport.settings().set_tempo(Bpm::of(120.0));
    let _ = transport.motion().send(MotionCommand::Play);

    (transport, state, meter, reader)
}

/// Render `frames` once, outside any gate, to prime whatever the first call
/// sizes.
fn warm_up(state: &AudioCallbackState, buf: &mut [f32]) {
    process_audio(
        state,
        &mut InterleavedMut::new(buf, ChannelLayout::STEREO).unwrap(),
    );
}

/// The fixture renders audible output.
///
/// Without this every gate below could pass by folding silence over an unwired
/// graph — the vacuous version of all of them.
#[test]
fn renders_something_to_gate() {
    let (_transport, state) = rolling_state();
    let mut out = vec![0.0f32; 512 * 2];

    // The filter needs a few blocks before its output is clearly non-zero.
    for _ in 0..4 {
        warm_up(&state, &mut out);
    }

    assert!(
        out.iter().any(|s| s.abs() > 1e-6),
        "the fixture graph rendered silence — the gates below would prove nothing"
    );
}

/// Rendering stays allocation-free across changing block sizes.
///
/// A device may change its buffer size mid-session, and the graph's per-vertex
/// buffers are sized from the block. Warming at the *largest* size first is
/// deliberate: warming small and then growing would attribute the growth
/// allocation to the gate, which reads as a failure of the render path rather
/// than of the warm-up — the trap `rt_no_alloc_hardware_poll.rs` documents.
#[test]
fn process_audio_across_block_sizes_is_allocation_free() {
    const SIZES: [usize; 5] = [1024, 512, 256, 128, 64];

    let (_transport, state) = rolling_state();
    let mut out = vec![0.0f32; SIZES[0] * 2];

    // Warm every size, largest first, outside the gate.
    for frames in SIZES {
        warm_up(&state, &mut out[..frames * 2]);
    }

    for _ in 0..64 {
        for frames in SIZES {
            process_audio(
                &state,
                &mut InterleavedMut::new(&mut out[..frames * 2], ChannelLayout::STEREO).unwrap(),
            );
        }
    }
}

/// The whole output callback, with the meter enabled and a graph tap open
/// (never drained, so the ring fills and the op counts its drops), must not
/// allocate at the `MAX_FRAMES` ceiling, and the meter must have read the
/// block (so the gate ran the metering pass) and the tap must hold frames.
#[test]
fn a_metered_and_tapped_callback_is_allocation_free() {
    let (_transport, state, mut meter, tap) = rolling_state_with(true);
    let tap = tap.expect("opened");
    meter.enable();
    let mut block = OutputBlock::new(std::sync::Arc::new(state), ChannelLayout::STEREO);
    let mut out = vec![0.0f32; MAX_FRAMES * 2];
    for _ in 0..4 {
        block.render(&mut out);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..16 {
            block.render(&mut out);
        }
    });
    assert!(meter.read()[0].peak.get() > 0.0, "the meter read nothing");
    assert!(tap.available().get() > 0, "the tap read nothing");
}

/// A delayed render (the timer's per-block call, with the zero latency it
/// reports) and the render-thread fault records must not allocate or free, even
/// over a fault whose message is an owned string.
#[test]
fn a_delayed_render_and_the_render_thread_fault_records_are_allocation_free() {
    let (_transport, state) = rolling_state();
    let mut block = OutputBlock::new(std::sync::Arc::new(state), ChannelLayout::STEREO);
    let mut out = vec![0.0f32; 480 * 2];
    for _ in 0..4 {
        block.render_delayed_with(&mut out, Some(std::time::Duration::ZERO), |x| x);
    }
    let disconnected = tutti_engine::StreamFaults::new();
    disconnected.record(
        tutti_engine::BackendErrorKind::DeviceGone,
        &"unplugged",
        tutti_platform::Instant::now(),
    );
    let backend = tutti_engine::StreamFaults::new();
    backend.record(
        tutti_engine::BackendErrorKind::Other,
        &"xrun",
        tutti_platform::Instant::now(),
    );
    let clean = tutti_engine::StreamFaults::new();
    clean.record_overrun("warm");

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 4..64 {
            block.render_delayed_with(&mut out, Some(std::time::Duration::ZERO), |x| x);
        }
        disconnected.record_overrun("the timer fell behind");
        backend.record_overrun("the timer fell behind");
        clean.record_overrun("the timer fell behind");
        backend.record_panic("the output callback panicked");
        disconnected.record_panic("the output callback panicked");
    });
    assert!(out.iter().any(|s| *s != 0.0), "the gate rendered silence");
    assert_eq!(disconnected.count(), 3);
    assert_eq!(backend.count(), 3);
}
