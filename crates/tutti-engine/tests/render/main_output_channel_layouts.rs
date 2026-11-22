//! The graph main output's channel count must not panic the audio callback, AND a
//! surround main output must reach the output correctly — either straight through to a
//! matching-width device, or folded to a narrower one via the ITU/Dolby matrix.
//!
//! `TuttiPlugin.outputs` is a public field, so a host can build a mono or wider
//! main output. A main output wider than the engine's fold scratch would index past its end
//! inside the CPAL callback, so the engine refuses such a main output on the control
//! thread instead; these run in both profiles on purpose.

use crate::support;

use support::Sine;
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_buffers::MAX_CHANNELS;
use tutti_buffers::MaxBlock;
use tutti_engine::{Engine, GraphEngineError};
use tutti_graph::NodeKey;
use tutti_graph::{CommitError, Editor, ForkByClone, Prepare};
use tutti_graph::{OutPort, PortSource};
use tutti_timeline::Transport;
use tutti_units::{Hz, SampleRate, Samples};

/// A graph whose main output has `outputs` channels, with a sine at key 1 feeding
/// the channels `wire` picks (the rest explicitly silent).
fn main_output(outputs: usize, wire: &[usize]) -> (Editor, tutti_graph::Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(256)),
    ));
    ed.insert(NodeKey(1), "sine", ForkByClone(Sine::new(Hz::new(440.0))));
    ed.spec_mut().topology.set_outputs((0..outputs).map(|ch| {
        if wire.contains(&ch) {
            PortSource::Node(OutPort {
                node: NodeKey(1),
                port: 0,
            })
        } else {
            PortSource::Zero
        }
    }));
    ed.commit().expect("commits");
    (ed, exec)
}

/// Render one block of a main output with `outputs` channels into a `target`-wide
/// interleaved buffer. `wire` feeds the source to the main output outputs it picks.
fn render_main_output_to(outputs: usize, target: usize, wire: &[usize]) -> Vec<f32> {
    let (mut ed, exec) = main_output(outputs, wire);
    let engine =
        Engine::new(&Transport::new(SampleRate::SR_48K), &mut ed, exec).expect("within the limits");
    let frames = 256;
    let mut out = vec![0.0f32; frames * target];
    // `target` stays a plain count in the assertions below — they read as
    // widths there. It becomes a layout at exactly this boundary, welded to the
    // buffer, so the engine's frame count is `out.len() / target` by
    // construction rather than a second argument that could disagree with it.
    engine.process(
        &mut InterleavedMut::new(
            &mut out,
            ChannelLayout::try_from(target).expect("a test width is small"),
        )
        .unwrap(),
    );
    out
}

/// Sum of squares of channel `c` in a `width`-interleaved buffer.
fn channel_energy(buf: &[f32], c: usize, width: usize) -> f32 {
    buf.chunks_exact(width).map(|f| f[c] * f[c]).sum()
}

#[test]
fn mono_main_output_duplicates_into_stereo() {
    // Mono main output → stereo device: channel 0 duplicated into both sides.
    let out = render_main_output_to(1, 2, &[0]);
    for frame in out.as_chunks::<2>().0 {
        assert_eq!(
            frame[0], frame[1],
            "mono main output must duplicate into L/R"
        );
    }
    assert!(out.iter().any(|s| *s != 0.0), "sine should produce signal");
}

#[test]
fn stereo_main_output_passes_through_to_stereo() {
    let out = render_main_output_to(2, 2, &[0, 1]);
    assert!(out.iter().any(|s| *s != 0.0));
}

#[test]
fn surround_main_output_reaches_matching_width_device() {
    // 5.1 main output, source wired ONLY to the SL channel (idx 4). At a 6-wide target
    // the energy stays in channel 4 — straight through, no fold.
    let out = render_main_output_to(6, 6, &[4]);
    let e: Vec<f32> = (0..6).map(|c| channel_energy(&out, c, 6)).collect();
    assert!(e[4] > 0.0, "SL source must reach output channel 4 ({e:?})");
    for (c, &en) in e.iter().enumerate() {
        if c != 4 {
            assert_eq!(en, 0.0, "only channel 4 should carry energy ({e:?})");
        }
    }
}

#[test]
fn surround_main_output_folds_to_stereo_device() {
    // Same 5.1 main output with the source in SL (idx 4). At a 2-wide target the ITU
    // matrix folds SL into Lo at −3 dB (Lo = FL + .707·C + .707·SL) — so the
    // LEFT channel carries it and the right is (near) silent.
    let out = render_main_output_to(6, 2, &[4]);
    let el = channel_energy(&out, 0, 2);
    let er = channel_energy(&out, 1, 2);
    assert!(el > 0.0, "SL must fold into the left downmix channel");
    assert!(
        el > er * 4.0,
        "SL folds to the left only (el={el}, er={er})"
    );
}

#[test]
fn center_main_output_folds_symmetrically_to_stereo() {
    // Source in the CENTER channel (idx 2 in 5.1) → both stereo channels at −3dB.
    let out = render_main_output_to(6, 2, &[2]);
    let el = channel_energy(&out, 0, 2);
    let er = channel_energy(&out, 1, 2);
    assert!(el > 0.0 && er > 0.0, "center must reach both channels");
    let ratio = el / er;
    assert!(
        (0.5..2.0).contains(&ratio),
        "center fold must be ~symmetric"
    );
}

/// A 7.1.4 main output (twelve channels) renders: a source on its last channel, the
/// top back right at index 11, reaches the same channel of a twelve-wide
/// device and no other.
#[test]
fn a_714_main_output_reaches_a_matching_width_device() {
    let out = render_main_output_to(12, 12, &[11]);
    let e: Vec<f32> = (0..12).map(|c| channel_energy(&out, c, 12)).collect();
    assert!(e[11] > 0.0, "the source must reach channel 11 ({e:?})");
    for &en in e.iter().take(11) {
        assert_eq!(en, 0.0, "only channel 11 carries energy ({e:?})");
    }
}

/// The same 7.1.4 main output folds to a stereo device by speaker: the top back right
/// (index 11) goes to the right channel alone.
#[test]
fn a_714_main_output_folds_to_a_stereo_device_by_speaker() {
    let out = render_main_output_to(12, 2, &[11]);
    let (el, er) = (channel_energy(&out, 0, 2), channel_energy(&out, 1, 2));
    assert!(er > 0.0, "the right channel carries the source");
    assert_eq!(el, 0.0, "the left channel carries none (el={el}, er={er})");
}

/// A main output wider than `MAX_CHANNELS` never reaches the callback:
/// the engine refuses it at construction, naming the widths.
///
/// A graph cannot be handed to the engine that wide, so the property is the
/// refusal (the refusal of
/// a later widening commit is `engine_graph`'s
/// `a_graph_engine_refuses_more_outputs_than_it_folds`).
#[test]
fn over_wide_main_output_is_refused_before_it_can_render() {
    let wide = MAX_CHANNELS + 1;
    let (mut ed, exec) = main_output(wide, &(0..wide).collect::<Vec<_>>());
    let refused = Engine::new(&Transport::new(SampleRate::SR_48K), &mut ed, exec).err();
    assert_eq!(
        refused,
        Some(GraphEngineError::Limits(CommitError::TooManyOutputs {
            outputs: wide,
            limit: MAX_CHANNELS,
        }))
    );
}
