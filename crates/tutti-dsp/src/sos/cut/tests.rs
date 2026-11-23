use super::*;
use crate::TAIL_FLOOR;

const RATE: SampleRate = SampleRate::SR_48K;

fn lr16(response: CutResponse) -> CutDesign {
    CutDesign::new(
        response,
        CutAlignment::LinkwitzRiley(ButterworthOrder::of(8)),
    )
}

/// The frames an `f64` impulse response (transposed direct form II) of LR16 at `cutoff` last
/// carries the tail floor of its peak.
fn ring_at(response: CutResponse, cutoff: f64) -> usize {
    let sos = LinkwitzRiley::new(response, ButterworthOrder::of(8))
        .design(Hz::new(cutoff), RATE)
        .unwrap();
    let mut z = vec![[0.0f64; 2]; sos.sections().len()];
    let h: Vec<f64> = (0..1 << 20)
        .map(|n| {
            let mut x = if n == 0 { 1.0 } else { 0.0 };
            for (b, z) in sos.sections().iter().zip(&mut z) {
                let y = b.b0 * x + z[0];
                z[0] = b.b1 * x - b.a1 * y + z[1];
                z[1] = b.b2 * x - b.a2 * y;
                x = y;
            }
            x
        })
        .collect();
    let peak = h.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let eps = TAIL_FLOOR.to_amplitude().get();
    h.iter().rposition(|v| v.abs() >= eps * peak).unwrap() + 1
}

fn frames(time: DecayTime) -> usize {
    match time {
        DecayTime::Finite(s) => (s.get() * RATE.get()).ceil() as usize,
        DecayTime::Unbounded => panic!("unbounded"),
    }
}

/// A cutoff past the range is read as its end: 1 Hz as [`MIN_CUT_CUTOFF`], one past the
/// Nyquist as 0.99 of it, and one inside the range as itself; and a design at an out-of-range
/// cutoff is the design at the end of the range, section for section.
#[test]
fn a_cutoff_out_of_range_is_held_at_the_end_of_it() {
    let held = |hz: f64| {
        CutFilterControls {
            cutoff: Hz::new(hz),
        }
        .held(RATE)
        .get()
    };
    assert_eq!(held(1.0), MIN_CUT_CUTOFF.get());
    assert_eq!(held(90_000.0), RATE.nyquist_scaled(0.99).get());
    assert_eq!(held(1_000.0), 1_000.0);

    let design = CutDesign::new(
        CutResponse::HighPass,
        CutAlignment::Butterworth(ButterworthOrder::of(4)),
    );
    let at = |hz: f64| {
        let mut sos = Sos::new(Vec::new(), RATE);
        design
            .design_into(
                &CutFilterControls {
                    cutoff: Hz::new(hz),
                },
                RATE,
                &mut sos,
            )
            .unwrap();
        sos
    };
    assert_eq!(at(1.0), at(MIN_CUT_CUTOFF.get()));
    assert_eq!(at(90_000.0), at(RATE.nyquist_scaled(0.99).get()));
    assert_ne!(at(10.0), at(100.0));
}

/// Between two octave rungs the ring-out `at` reports covers the impulse response's at that
/// exact cutoff and is under the next rung down's twice over, and the ceiling is the lowest
/// rung's. Reporting the rung above fails the cover.
#[test]
fn the_ring_out_narrows_to_the_octave_below_the_cutoff() {
    for response in [CutResponse::LowPass, CutResponse::HighPass] {
        let ladder = lr16(response).decay_ladder(RATE, TAIL_FLOOR);
        let at = |hz: f64| {
            ladder.at(
                &CutFilterControls {
                    cutoff: Hz::new(hz),
                },
                RATE,
            )
        };
        assert_eq!(ladder.ceiling(), at(10.0));
        let mut last = usize::MAX;
        for cutoff in [14.0f64, 27.0, 55.0, 110.0, 230.0, 470.0, 1500.0, 3000.0] {
            let reported = frames(at(cutoff));
            let ring = ring_at(response, cutoff);
            assert!(
                ring <= reported,
                "{response:?} {cutoff}: {reported} < {ring}"
            );
            assert!(
                reported <= 3 * ring + 64,
                "{response:?} {cutoff}: {reported} vs {ring}"
            );
            assert!(reported <= last, "{response:?} {cutoff} does not narrow");
            last = reported;
        }
        assert!(frames(ladder.ceiling()) > 10 * last);
    }
}
