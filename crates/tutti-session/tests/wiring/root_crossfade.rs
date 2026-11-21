//! A root declaration crossfades through the public API: the old root's
//! output fades out and the new root's fades in over the declick length,
//! unless the declaration is made `.unfaded()`.
//!
//! Every figure is analytic: constants of exactly representable values and
//! the equal-amplitude gains `(1 - s, s)` with the smootherstep
//! `s(x) = x^3 (6x^2 - 15x + 10)` at `x = (k + 1) / (n + 1)`, `n` the 240
//! frames of 5 ms at 48 kHz.

use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, ChannelTopology, MaxBlock};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::{OutPort, Root, Session};
use tutti_units::{SampleRate, Samples};

const FADE: usize = 240;

/// A stereo session whose root is fed (0.25, 0.75), the plan installed.
fn session() -> Session {
    let mut s = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples::new(64)))
        .build()
        .expect("builds");
    let node = s.graph().add(ForkByClone(Const::frame(&[0.25, 0.75])));
    for c in 0..2 {
        s.graph()
            .set_sources(Root::output(c), [OutPort::new(node, c)])
            .unwrap()
            .unfaded();
    }
    let _ = block(&mut s);
    s
}

/// One 64-frame block after the session commits: its right channel.
fn block(s: &mut Session) -> Vec<f32> {
    let _ = s.update();
    let mut out = vec![f32::NAN; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
    assert!(
        out.as_chunks::<2>().0.iter().all(|f| f[0] == 0.25),
        "the left is the same in both roots"
    );
    out.as_chunks::<2>().0.iter().map(|f| f[1]).collect()
}

/// `s(x)`, the smootherstep.
fn smootherstep(x: f64) -> f64 {
    x * x * x * (6.0 * x * x - 15.0 * x + 10.0)
}

/// **A declaration that changes the root crossfades**: the stereo root heard
/// as (0.25, 0.75) is declared mono, which keeps channel 0 (0.25) for both
/// device channels. The right is `0.75 (1 - s) + 0.25 s` frame by frame over
/// the 240 frames, and 0.25 from then on.
#[test]
fn a_declared_root_change_crossfades_over_the_declick() {
    let mut s = session();
    s.declare_root_topology(Some(ChannelTopology::default_for(ChannelLayout::MONO)))
        .expect("a mono root");
    let mut right = Vec::new();
    for _ in 0..5 {
        right.extend(block(&mut s));
    }
    for (k, &r) in right.iter().enumerate() {
        let want = if k < FADE {
            let s = smootherstep((k + 1) as f64 / (FADE + 1) as f64);
            (0.75 * (1.0 - s) + 0.25 * s) as f32
        } else {
            0.25
        };
        assert!((r - want).abs() < 1e-6, "frame {k}: {r} against {want}");
    }
}

/// **`.unfaded()` cuts**: the new root is the whole mix from the first frame.
#[test]
fn a_hard_declaration_cuts_at_the_block() {
    let mut s = session();
    s.declare_root_topology(Some(ChannelTopology::default_for(ChannelLayout::MONO)))
        .expect("a mono root")
        .unfaded();
    assert!(block(&mut s).iter().all(|&r| r == 0.25));
}
