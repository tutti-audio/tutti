use super::*;
use crate::test_support::{RATE, render_f64};
use tutti_dsp::Partial;
use tutti_units::FrequencyRatio;

fn partials() -> Partials {
    let p = |ratio: f64, amplitude: f64| {
        Partial::new(
            FrequencyRatio::new(ratio).unwrap(),
            Amplitude::new(amplitude),
        )
    };
    Partials::try_from(&[p(1.0, 0.5), p(3.0, 0.25), p(60.0, 0.9)][..]).unwrap()
}

/// At 480 Hz the partials are at 480, 1440 and 28 800 Hz: the first two are lines of their
/// amplitudes times the level, and the third is past the Nyquist, so nothing is at its alias
/// of 19 200 Hz.
#[test]
fn the_node_plays_its_partials_below_the_nyquist() {
    let node = AdditiveNode::<f64>::from(
        AdditiveConfig::new(partials())
            .frequency(Hz::new(480.0))
            .level(Amplitude::new(0.5)),
    );
    let out = render_f64(node, RATE, &[], 4800);
    let line = |hz: f64| {
        let w = core::f64::consts::TAU * hz / RATE.get();
        let (mut re, mut im) = (0.0, 0.0);
        for (n, &v) in out[0].iter().enumerate() {
            re += v * libm::cos(w * n as f64);
            im -= v * libm::sin(w * n as f64);
        }
        (re * 2.0 / 4800.0, im * 2.0 / 4800.0)
    };
    for (hz, amplitude) in [(480.0, 0.25), (1_440.0, 0.125), (19_200.0, 0.0)] {
        let (re, im) = line(hz);
        assert!(
            re.abs() < 1e-12 && (im + amplitude).abs() < 1e-12,
            "{hz} Hz"
        );
    }
}
