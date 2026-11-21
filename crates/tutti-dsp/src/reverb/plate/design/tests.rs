use tutti_units::{Hz, Mix, SampleRate, Seconds};

use super::*;
use crate::System;
use crate::reverb::network::{Fdn, NetworkStage};

fn design(c: &PlateControls, rate: SampleRate) -> Box<Network> {
    let mut network = Box::new(Network::new(rate));
    c.design_into(rate, &mut network);
    network
}

fn fdn_of(n: &Network) -> &Fdn {
    let Some(NetworkStage::Fdn(f)) = n.stages.iter().find(|s| matches!(s, NetworkStage::Fdn(_)))
    else {
        panic!("one FDN stage")
    };
    f
}

/// Table 1's tank and input lengths at 29761 Hz, written out from the paper.
const PAPER_INPUT: [f64; 4] = [142.0, 107.0, 379.0, 277.0];
const PAPER_LINES: [f64; 4] = [4453.0, 3720.0, 4217.0, 3163.0];
const PAPER_LATTICES: [f64; 4] = [672.0, 1800.0, 908.0, 2656.0];

/// Every length at 48 kHz is the paper's times 48000 / 29761, rounded to the nearest frame.
#[test]
fn lengths_at_48k_are_the_paper_table_scaled_and_rounded() {
    let rate = SampleRate::SR_48K;
    let k = 48_000.0 / 29_761.0;
    let n = design(&PlateControls::DEFAULT, rate);
    let want = |paper: f64| (paper * k).round();

    let f = fdn_of(&n);
    let input = n.stages.iter().filter_map(|s| match s {
        NetworkStage::AllPass(a) => Some(a[0].delay.get()),
        _ => None,
    });
    let lines = f.lines.iter().map(|l| l.length.get());
    let lattices = f
        .lines
        .iter()
        .map(|l| l.diffuser.expect("a lattice").delay.get());
    assert!(input.eq(PAPER_INPUT.map(want)));
    assert!(lines.eq(PAPER_LINES.map(want)));
    assert!(lattices.eq(PAPER_LATTICES.map(want)));
    // 107 * 48000 / 29761 = 172.57: the rounding is to nearest, not down.
    assert_eq!(want(107.0), 173.0);
    // The loop is the eight delays' sum: 21589 frames of the paper's table.
    let sum: f64 = PAPER_LINES.iter().chain(&PAPER_LATTICES).sum();
    assert_eq!(sum, 21_589.0);
    assert_eq!(loop_frames(SampleRate::of(29_761.0)), sum);
}

/// The decay coefficient behind a decay of `rt60` is `c` with `-3 T / log10 c^4 = RT60`.
#[test]
fn the_decay_coefficient_gives_the_designed_rt60_by_the_closed_form() {
    for rate in [SampleRate::SR_44K1, SampleRate::SR_48K, SampleRate::SR_96K] {
        for secs in [0.5, 2.0, 6.0] {
            let c = PlateControls {
                decay: Seconds::new(secs),
                ..PlateControls::DEFAULT
            };
            let n = design(&c, rate);
            let Absorption::Gain(g) = fdn_of(&n).lines[1].absorption else {
                panic!("the decay gain")
            };
            let t = loop_frames(rate) / rate.get();
            let rt60 = -3.0 * t / g.get().powi(4).log10();
            assert!((rt60 - secs).abs() < 1e-4 * secs, "{rate:?} {secs}: {rt60}");
        }
    }
}

/// The paper's table 2 as it prints it: node, offset at 29761 Hz, sign; left output then right.
/// `node24_30`, `31_33` and `33_39` are the left half's first delay, second lattice and second
/// delay; `48_54`, `55_59` and `59_63` the right half's.
const PAPER_TAPS: [[(&str, u32, f64); 7]; 2] = [
    [
        ("48_54", 266, 1.0),
        ("48_54", 2974, 1.0),
        ("55_59", 1913, -1.0),
        ("59_63", 1996, 1.0),
        ("24_30", 1990, -1.0),
        ("31_33", 187, -1.0),
        ("33_39", 1066, -1.0),
    ],
    [
        ("24_30", 353, 1.0),
        ("24_30", 3627, 1.0),
        ("31_33", 1228, -1.0),
        ("33_39", 2673, 1.0),
        ("48_54", 2111, -1.0),
        ("55_59", 335, -1.0),
        ("59_63", 121, -1.0),
    ],
];

/// The output taps are table 2's: each tap's half, point, offset and sign, and the 0.6 gain.
#[test]
fn the_output_taps_are_the_papers_table_2() {
    let place = |node: &str| match node {
        "24_30" => (0, Point::Delay1),
        "31_33" => (0, Point::Lattice2),
        "33_39" => (0, Point::Delay2),
        "48_54" => (1, Point::Delay1),
        "55_59" => (1, Point::Lattice2),
        "59_63" => (1, Point::Delay2),
        other => panic!("no node {other}"),
    };
    for (side, paper) in OUTPUT_TAPS.iter().zip(PAPER_TAPS) {
        for (tap, (node, offset, sign)) in side.iter().zip(paper) {
            assert_eq!((tap.half, tap.point), place(node), "{node}[{offset}]");
            assert_eq!(
                (tap.offset, tap.sign.get()),
                (offset, sign),
                "{node}[{offset}]"
            );
        }
    }
    assert_eq!(OUTPUT_GAIN.get(), 0.6);
}

/// Decay diffusion 2 is the paper's `decay + 0.15`, floor 0.25, ceiling 0.50 (table 1), at the
/// paper's diffusion: the decay coefficient `c` is found here from the closed form, `c = 10^(-3 T
/// / (4 RT60))` over the paper's loop `T = 21589 / 29761` s, and the figures are the paper's
/// rule written out: 0.05 is under the floor, 0.12, 0.20 and 0.30 are in its linear zone, 0.50 is
/// over the ceiling. Decay diffusion 1 is -0.7 whatever the decay.
#[test]
fn decay_diffusion_follows_the_papers_rule() {
    let t = 21_589.0 / 29_761.0;
    let rate = SampleRate::SR_48K;
    for (c, want) in [
        (0.05, 0.25),
        (0.12, 0.27),
        (0.20, 0.35),
        (0.30, 0.45),
        (0.50, 0.50),
    ] {
        let rt60 = 0.75 * t / -f64::log10(c);
        let n = design(
            &PlateControls {
                decay: Seconds::new(rt60),
                ..PlateControls::DEFAULT
            },
            rate,
        );
        let f = fdn_of(&n);
        let g2 = f.lines[1].diffuser.unwrap().gain.get();
        assert!(
            (g2 - want).abs() < 2e-3,
            "c = {c}: decay diffusion 2 {g2}, paper {want}"
        );
        assert_eq!(f.lines[0].diffuser.unwrap().gain.get(), -0.70);
    }
}

/// The diffusion scales every lattice coefficient (decay diffusion 2 after its rule), and at 0
/// every lattice is a plain delay.
#[test]
fn diffusion_scales_every_lattice_coefficient_and_zero_makes_them_delays() {
    let gains = |k: f64| {
        let mut n = Box::new(Network::new(SampleRate::SR_48K));
        let c = PlateControls {
            diffusion: Mix::new(k),
            ..PlateControls::DEFAULT
        };
        c.design_into(SampleRate::SR_48K, &mut n);
        let f = fdn_of(&n);
        let mut all: Vec<f64> = n
            .stages
            .iter()
            .filter_map(|s| match s {
                NetworkStage::AllPass(a) => Some(a[0].gain.get()),
                _ => None,
            })
            .collect();
        all.extend([f.lines[0].diffuser, f.lines[1].diffuser].map(|d| d.unwrap().gain.get()));
        all
    };
    let (full, half, none) = (gains(1.0), gains(0.5), gains(0.0));
    assert_eq!(full[..5], [0.75, 0.75, 0.625, 0.625, -0.70]);
    assert_eq!(half, full.iter().map(|g| 0.5 * g).collect::<Vec<_>>());
    assert_eq!(none, [0.0; 6]);
}

/// The tank is a stable ring, wet only, with a finite tail.
#[test]
fn the_network_is_stable_wet_only_and_finite() {
    let n = design(&PlateControls::DEFAULT, SampleRate::SR_48K);
    assert!(fdn_of(&n).is_stable());
    assert!(!n.has_direct_path());
    assert!(matches!(
        n.tail(crate::TAIL_FLOOR),
        crate::DecayTime::Finite(_)
    ));
}

/// A non-finite control is its default and one past its range is clamped.
#[test]
fn non_finite_controls_are_the_default_and_wild_ones_are_clamped() {
    let rate = SampleRate::SR_48K;
    let with_default_damping = |c: PlateControls| {
        design(
            &PlateControls {
                damping: Mix::new(0.0005),
                ..c
            },
            rate,
        )
    };
    let wild = PlateControls {
        decay: Seconds::new(f64::NAN),
        damping: Mix::new(f64::INFINITY),
        bandwidth: Hz::new(f64::NAN),
        pre_delay: Seconds::new(f64::NAN),
        diffusion: Mix::new(f64::NAN),
        ..PlateControls::DEFAULT
    };
    assert_eq!(
        *with_default_damping(wild),
        *with_default_damping(PlateControls::DEFAULT)
    );
    assert_eq!(wild.bandwidth_at(rate), PlateControls::DEFAULT.bandwidth);
    let huge = PlateControls {
        decay: Seconds::new(1.0e9),
        damping: Mix::new(5.0),
        ..PlateControls::DEFAULT
    };
    let n = design(&huge, rate);
    assert!(matches!(
        fdn_of(&n).lines[0].absorption,
        Absorption::OnePole { damp, .. } if damp == 0.999
    ));
}
