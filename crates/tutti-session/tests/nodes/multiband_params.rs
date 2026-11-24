//! The multiband compressor's indexed params through the session (doc 013,
//! "Param addresses"): `Session::set`, a modulation route and a fork of
//! the graph reach one band's address and leave the others alone.
//!
//! The rig is a stereo node at unity ratio with a 100 Hz tone in the left
//! channel and a 6 kHz tone in the right, across a 1 kHz crossover. Each
//! channel is then one band's: the bands are phase-aligned, so a tone of
//! amplitude `A` leaves at `A * (lp * g0 + hp * g1)`, with `lp = 1 / (1 + x^4)`
//! and `hp = x^4 / (1 + x^4)` for `x` the warped ratio of the tone to the
//! split, and `g0`, `g1` the bands' make-up gains. The left tone is band 0's
//! (`hp` is 1e-4 of it) and the right tone band 1's (`lp` is 6e-4).

use std::f64::consts::{PI, TAU};
use tutti_buffers::PlanesMut;

use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_graph::ForkByClone;
use tutti_modulation::Lfo;
use tutti_nodes::testing::Osc;
use tutti_nodes::{MultibandBands, MultibandCompressorConfig, MultibandCompressorNode};
use tutti_session::{InputPort, Key, MainOutput, OutputPort, Session};
use tutti_session::{RouteSpec, ValueErrorKind};
use tutti_units::{Amplitude, SampleRate};
use tutti_units::{BeatsPerCycle, Bipolar};
use tutti_units::{CompressionRatio, Db, Hz, Seconds};

const RATE: f64 = 48_000.0;
/// The tones' amplitude.
const A: f64 = 0.4;
const LOW: f64 = 100.0;
const HIGH: f64 = 6_000.0;
const SPLIT: f64 = 1_000.0;

fn warp(f: f64) -> f64 {
    (PI * f / RATE).tan() / (PI * SPLIT / RATE).tan()
}

/// Band 0's and band 1's share of a tone at `hz`.
fn shares(hz: f64) -> (f64, f64) {
    let x4 = warp(hz).powi(4);
    (1.0 / (1.0 + x4), x4 / (1.0 + x4))
}

fn gain(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// What the tone at `hz` leaves at, for make-ups `g0` and `g1` in dB.
fn expected(hz: f64, g0: f64, g1: f64) -> f64 {
    let (lp, hp) = shares(hz);
    A * (lp * gain(g0) + hp * gain(g1))
}

/// An offline stereo session with the rig wired to the outputs.
fn rig() -> (Session, Key<MultibandCompressorNode<f32>>) {
    let mut s = Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    let g = s.graph();
    let low = g.add(ForkByClone(
        Osc::sine(Hz::new(LOW)).with_amplitude(Amplitude::new(A)),
    ));
    let high = g.add(ForkByClone(
        Osc::sine(Hz::new(HIGH)).with_amplitude(Amplitude::new(A)),
    ));
    let mb = g.add(MultibandCompressorNode::<f32>::from(
        MultibandCompressorConfig::new(ChannelTopology::STEREO, MultibandBands::of(2)),
    ));
    g.set_sources(InputPort::new(mb, 0), [OutputPort::new(low, 0)])
        .unwrap();
    g.set_sources(InputPort::new(mb, 1), [OutputPort::new(high, 0)])
        .unwrap();
    g.set_sources(MainOutput::channel(0), [OutputPort::new(mb, 0)])
        .unwrap();
    g.set_sources(MainOutput::channel(1), [OutputPort::new(mb, 1)])
        .unwrap();
    // Unity ratio on both bands: nothing is reduced, the make-ups are the gains.
    for band in 0..2 {
        s.set(mb.ratio_at(band).to(CompressionRatio::new(1.0)))
            .unwrap();
    }
    let _ = s.update();
    (s, mb)
}

/// Renders `frames` frames from a rolling transport, in blocks of 128.
fn render(s: &mut Session, frames: usize) -> [Vec<f32>; 2] {
    s.play().expect("room");
    let mut all = [Vec::new(), Vec::new()];
    let mut buf = vec![0.0f32; 128 * 2];
    for _ in 0..frames / 128 {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        for frame in buf.as_chunks::<2>().0 {
            all[0].push(frame[0]);
            all[1].push(frame[1]);
        }
    }
    all
}

/// The amplitude of a tone of `hz` in `x`, by lock-in over whole periods.
fn amplitude(x: &[f32], hz: f64) -> f64 {
    tutti_fixtures::signal::amplitude_at(x, tutti_units::Hz::new(hz), SampleRate::of(RATE))
}

/// Both channels' steady amplitudes after a settle, over whole periods of
/// both tones (9 600 frames hold 20 of 100 Hz and 1 200 of 6 kHz).
fn measure(s: &mut Session) -> (f64, f64) {
    render(s, 24_064);
    let [l, r] = render(s, 9_600);
    (amplitude(&l, LOW), amplitude(&r, HIGH))
}

/// **A write at index 1 reaches band 1 and leaves band 0 alone**, and
/// the other way: the right channel (band 1's tone) follows `GainDb` at
/// index 1 by the analytic figure, the left (band 0's) follows index 0.
#[test]
fn a_write_to_band_1_moves_band_1_only() {
    let (mut s, mb) = rig();
    let (l, r) = measure(&mut s);
    assert!((l - expected(LOW, 0.0, 0.0)).abs() < 1e-5, "{l}");
    assert!((r - expected(HIGH, 0.0, 0.0)).abs() < 1e-5, "{r}");

    s.set(mb.gain_db_at(1).to(Db::new(6.0))).unwrap();
    let (l, r) = measure(&mut s);
    assert!(
        (l - expected(LOW, 0.0, 6.0)).abs() < 1e-5,
        "band 1 reached the left: {l}"
    );
    assert!((r - expected(HIGH, 0.0, 6.0)).abs() < 1e-5, "{r}");

    s.set(mb.gain_db_at(0).to(Db::new(-9.0))).unwrap();
    let (l, r) = measure(&mut s);
    assert!((l - expected(LOW, -9.0, 6.0)).abs() < 1e-5, "{l}");
    assert!(
        (r - expected(HIGH, -9.0, 6.0)).abs() < 1e-5,
        "band 0 reached the right: {r}"
    );
}

/// The node declares every band's params, each writable on its own, and no
/// more: a read of a declared target is `Ok` at both bands and
/// `NoSuchTarget` at a third; the crossover (`Cutoff`) is declared at index
/// 0 only.
#[test]
fn the_node_declares_each_band_and_crossover() {
    let (s, mb) = rig();
    let no_such = ValueErrorKind::NoSuchTarget;
    macro_rules! band_declared {
        ($($at:ident),+) => {$(
            for band in 0..2u8 {
                assert!(s.get(mb.$at(band)).is_ok(), "{} {band}", stringify!($at));
            }
            assert_eq!(s.get(mb.$at(2)).unwrap_err().kind, no_such, "{} 2", stringify!($at));
        )+};
    }
    band_declared!(
        threshold_at,
        knee_at,
        ratio_at,
        attack_at,
        release_at,
        gain_db_at
    );
    assert!(s.get(mb.cutoff_at(0)).is_ok());
    assert_eq!(s.get(mb.cutoff_at(1)).unwrap_err().kind, no_such);
}

/// A write at index 1 reaches the band meter of band 1 through the
/// `Graph::gain_reduction`: a tone over the threshold is reduced by a
/// band-1 threshold written low and not by one written high, band 0 never.
///
/// The right tone (0.4, -8 dB) is over a -30 dB threshold at 2:1 (the reduction
/// is positive) and under a 0 dB one (none, exactly, with the ratio back at
/// 2).
#[test]
fn the_meter_of_band_1_follows_its_threshold() {
    let (mut s, mb) = rig();
    for band in 0..2 {
        s.set((
            mb.ratio_at(band).to(CompressionRatio::new(2.0)),
            mb.release_at(band).to(Seconds::new(0.001)),
            mb.attack_at(band).to(Seconds::new(0.001)),
        ))
        .unwrap();
    }
    s.set(mb.threshold_at(0).to(Db::new(0.0))).unwrap();
    s.set(mb.threshold_at(1).to(Db::new(-30.0))).unwrap();
    render(&mut s, 9_600);
    assert!(
        s.graph_ref().gain_reduction(&mb, 1).unwrap() > Db::new(3.0),
        "{:?}",
        s.graph_ref().gain_reduction(&mb, 1).unwrap()
    );
    assert!(
        s.graph_ref().gain_reduction(&mb, 0).unwrap() < Db::new(1e-2),
        "{:?}",
        s.graph_ref().gain_reduction(&mb, 0).unwrap()
    );
    s.set(mb.threshold_at(1).to(Db::new(0.0))).unwrap();
    render(&mut s, 9_600);
    assert!(
        s.graph_ref().gain_reduction(&mb, 1).unwrap() < Db::new(1e-4),
        "{:?}",
        s.graph_ref().gain_reduction(&mb, 1).unwrap()
    );
    assert_eq!(
        s.graph_ref().gain_reduction(&mb, 2),
        None,
        "a band past the last names nothing"
    );
}

/// **A route to `GainDb` at index 1 moves band 1's tone and leaves band 0's
/// alone.** An LFO on a `-24..=24` dB target (the cell is 0 dB) at depth 0.25
/// swings band 1's make-up by `12 * sin` dB at 4 Hz past the connect's
/// declick: the right channel's amplitude, measured over each 8-frame period
/// of the 6 kHz tone, is `A * (lp + hp * g1(t))`, while the left stays at its
/// steady figure.
///
/// The lane steps every 32 frames, so `g1` is within 0.2 dB (2.4%) of the
/// sine; the tolerance is 4%, and the swing (a 24 dB span) is asserted
/// whole. A make-up is sampled on the 16-frame grid and ramps over 5 ms
/// (`GAIN_RAMP`), re-aimed at each grid point: that is a first-order lag of
/// time constant 5 ms, so the swing is the sine through it, `1 / sqrt(1 +
/// (w tau)^2)` of its depth and `atan(w tau)` late (0.992 and 0.125 rad at 4 Hz).
#[test]
fn a_route_to_band_1_modulates_band_1_only() {
    let (mut s, mb) = rig();
    let band1 = mb.gain_db_at(1);
    let mut m = s.mods_mut();
    m.range(band1, Db::new(-24.0)..Db::new(24.0)).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(BeatsPerCycle::of(0.5)));
    m.route(RouteSpec::new(lfo, band1, Bipolar::of(0.25)))
        .unwrap();
    for _ in 0..4 {
        let _ = s.update();
    }
    assert_eq!(
        s.mod_state().problems().next(),
        None,
        "the declarations apply"
    );

    // 120 BPM: one cycle per half beat is 4 Hz. Past the declick and the
    // make-up ramp's first blocks.
    let [l, r] = render(&mut s, 24_000);
    let skip = 1_024;
    let (lp0, hp0) = shares(LOW);
    let steady = A * lp0;
    for w in l[skip..].chunks(480).filter(|w| w.len() == 480) {
        let a = amplitude(w, LOW);
        // The leak of band 1 (1e-4) swings by up to 4x; band 0 itself is flat.
        assert!(
            (a - steady).abs() < 0.01 * steady + A * hp0 * 4.0,
            "left {a} vs {steady}"
        );
    }
    let (lp1, hp1) = shares(HIGH);
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for (k, w) in r.as_chunks::<8>().0.iter().enumerate().skip(skip / 8) {
        let rms2 = w.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / 8.0;
        let a = (2.0 * rms2).sqrt();
        let t = (k * 8 + 4) as f64 / RATE;
        let (w_tau, depth) = (TAU * 4.0 * 0.005, 12.0);
        let db = depth * (TAU * 4.0 * t - w_tau.atan()).sin() / (1.0 + w_tau * w_tau).sqrt();
        let want = A * (lp1 + hp1 * gain(db));
        assert!(
            (a - want).abs() < 0.04 * want,
            "right, window {k}: {a} vs {want}"
        );
        let measured_db = 20.0 * ((a / A - lp1) / hp1).log10();
        lo = lo.min(measured_db);
        hi = hi.max(measured_db);
    }
    assert!(
        lo < -9.0 && hi > 9.0,
        "band 1 swings about 0 dB: {lo}..{hi}"
    );
}

/// **A fork takes each index's value as it stands and shares no cell with
/// the live graph at any index**: after the fork, the live make-ups are
/// moved the other way (each band), and each render keeps its own figures.
#[test]
fn a_fork_keeps_each_index_to_itself() {
    let (mut s, mb) = rig();
    s.set(mb.gain_db_at(0).to(Db::new(-6.0))).unwrap();
    s.set(mb.gain_db_at(1).to(Db::new(6.0))).unwrap();
    render(&mut s, 128);
    let mut fork = s.graph_ref().testing().fork_live().expect("the node forks");
    s.set(mb.gain_db_at(0).to(Db::new(12.0))).unwrap();
    s.set(mb.gain_db_at(1).to(Db::new(-12.0))).unwrap();

    let mut out = [vec![0.0; 24_064 + 9_600], vec![0.0; 24_064 + 9_600]];
    fork.render(128, PlanesMut::from_vecs(&mut out).unwrap());
    let [l, r] = out;
    let (l, r) = (amplitude(&l[24_064..], LOW), amplitude(&r[24_064..], HIGH));
    assert!((l - expected(LOW, -6.0, 6.0)).abs() < 1e-5, "fork left {l}");
    assert!(
        (r - expected(HIGH, -6.0, 6.0)).abs() < 1e-5,
        "fork right {r}"
    );

    let (l, r) = measure(&mut s);
    assert!(
        (l - expected(LOW, 12.0, -12.0)).abs() < 1e-5,
        "live left {l}"
    );
    assert!(
        (r - expected(HIGH, 12.0, -12.0)).abs() < 1e-5,
        "live right {r}"
    );
}

/// A crossover write through the session moves the split: the left tone is
/// band 0's by `1 / (1 + x^4)` at the new split, so dropping the split from
/// 1 kHz to 150 Hz takes the 100 Hz tone through the low-pass at
/// `x = warp(100) / warp(150)` and the make-up of band 1 reaches it.
#[test]
fn a_crossover_write_moves_the_split() {
    let (mut s, mb) = rig();
    s.set(mb.gain_db_at(1).to(Db::new(-60.0))).unwrap();
    let (l, _) = measure(&mut s);
    // Band 1 muted by 60 dB: the left is band 0's share alone.
    let (lp, hp) = shares(LOW);
    assert!((l - A * (lp + hp * gain(-60.0))).abs() < 1e-5, "{l}");

    s.set(mb.cutoff_at(0).to(Hz::new(150.0))).unwrap();
    let (l, _) = measure(&mut s);
    let x4 = ((PI * LOW / RATE).tan() / (PI * 150.0 / RATE).tan()).powi(4);
    let want = A * (1.0 / (1.0 + x4) + x4 / (1.0 + x4) * gain(-60.0));
    assert!((l - want).abs() < 1e-5, "{l} vs {want}");
    assert!(l < A * lp * 0.9, "the split moved: {l}");
}
