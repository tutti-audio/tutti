//! The transient shaper node: its gain is the closed form of the two followers' step responses,
//! `attack * (1 - s / f)` while the fast envelope `f` is over the slow one `s`, with
//! `f_n = L (1 - a^n)` and `a = exp(-1 / (tau * rate))`; and its two amounts are the `GainDb`
//! params at index 0 (attack) and 1 (sustain).

use tutti_buffers::Planes;
use tutti_graph::ParamNode;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::{TransientShaperConfig, TransientShaperNode};
use tutti_types::{ChannelLayout, Db, SampleRate, Seconds, UnitAddr, UnitParam};

const SR: SampleRate = SampleRate::SR_48K;
const FAST: Seconds = Seconds(0.001);
const SLOW: Seconds = Seconds(0.02);
const LEVEL: f64 = 0.5;

fn node(width: ChannelLayout) -> TransientShaperNode {
    TransientShaperNode::from(TransientShaperConfig::new(width).fast(FAST).slow(SLOW))
}

fn pole(tau: Seconds) -> f64 {
    (-1.0 / (tau.get() * SR.get())).exp()
}

/// `x` through `node` in blocks of 100 frames.
fn run(node: &mut TransientShaperNode, x: &[f32]) -> Vec<f32> {
    x.chunks(100)
        .flat_map(|b| drive(node, SR, Planes::new(&[b]).unwrap(), &[]).remove(0))
        .collect()
}

/// The amounts set through the node's `GainDb` params steer the two branches of the closed form:
/// a step onset follows the attack amount, and a drop follows the sustain amount.
#[test]
fn the_gain_params_at_index_0_and_1_are_the_attack_and_sustain_amounts() {
    let mut n = prepared(node(ChannelLayout::MONO), SR, 100);
    let set = n.param_set();
    assert!(set.set(UnitAddr::new(UnitParam::GainDb, 0), 12.0));
    assert!(set.set(UnitAddr::new(UnitParam::GainDb, 1), 6.0));
    let (high, low) = (LEVEL, LEVEL / 4.0);
    let mut x = vec![high as f32; 48_000];
    x.extend(std::iter::repeat_n(low as f32, 1_000));
    let y = run(&mut n, &x);
    let (af, asl) = (pole(FAST), pole(SLOW));
    for i in 0..1_000 {
        let k = (i + 1) as i32;
        let (f, s) = (
            low + (high - low) * af.powi(k),
            low + (high - low) * asl.powi(k),
        );
        let want = low * 10f64.powf(6.0 * (1.0 - f / s) / 20.0);
        let got = f64::from(y[48_000 + i]);
        assert!(
            (got - want).abs() < 2e-5,
            "decay frame {i}: {got} vs {want}"
        );
    }
}

/// Zero on both amounts is the identity on a stereo signal.
#[test]
fn zero_amounts_pass_a_stereo_signal_bit_for_bit() {
    let mut n = prepared(node(ChannelLayout::STEREO), SR, 128);
    let l: Vec<f32> = (0..128)
        .map(|i| ((i * 7) % 13) as f32 / 20.0 - 0.3)
        .collect();
    let r: Vec<f32> = l.iter().rev().copied().collect();
    let y = drive(&mut n, SR, Planes::new(&[&l, &r]).unwrap(), &[]);
    assert_eq!(y[0], l);
    assert_eq!(y[1], r);
}

/// From silence into a constant level the node's gain follows the attack branch's closed form.
#[test]
fn a_step_onset_follows_the_attack_branch() {
    let attack = 9.0;
    let mut n = prepared(
        TransientShaperNode::from(
            TransientShaperConfig::new(ChannelLayout::MONO)
                .attack(Db(attack as f32))
                .fast(FAST)
                .slow(SLOW),
        ),
        SR,
        100,
    );
    let y = run(&mut n, &vec![LEVEL as f32; 1_000]);
    let (af, asl) = (pole(FAST), pole(SLOW));
    for (i, got) in y.iter().enumerate() {
        let k = (i + 1) as i32;
        let g = attack * (1.0 - (1.0 - asl.powi(k)) / (1.0 - af.powi(k)));
        let want = LEVEL * 10f64.powf(g / 20.0);
        assert!(
            (f64::from(*got) - want).abs() < 2e-5,
            "frame {i}: {got} vs {want}"
        );
    }
}
