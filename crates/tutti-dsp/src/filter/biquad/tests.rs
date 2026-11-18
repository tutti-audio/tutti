use super::*;

/// The response is the rational function and the poles are its roots.
#[test]
fn the_response_is_the_rational_function_and_the_poles_its_roots() {
    let c = Biquad {
        b0: 0.2,
        b1: 0.1,
        b2: -0.05,
        a1: -1.2,
        a2: 0.5,
    };
    let w: f64 = 0.7;
    let z = Complex64::from_polar(1.0, w);
    let want = (c.b0 + c.b1 / z + c.b2 / (z * z)) / (1.0 + c.a1 / z + c.a2 / (z * z));
    assert!((c.response(w) - want).norm() < 1e-12);
    for p in c.poles() {
        assert!((p * p + c.a1 * p + c.a2).norm() < 1e-12, "{p}");
    }
    assert!((c.spectral_radius() - 0.5f64.sqrt()).abs() < 1e-12);
}

/// `process` runs the difference equation, `hold` leaves the history of a
/// pass-through, and `reset` clears it.
#[test]
fn direct_form_i_runs_the_difference_equation_and_holds_a_pass_through() {
    let c = Biquad {
        b0: 0.5,
        b1: 0.25,
        b2: -0.125,
        a1: -0.5,
        a2: 0.25,
    };
    let mut f = DirectForm1::default();
    let mut out = [1.0f32, 0.0, 0.0, 0.0];
    f.process(&c, MonoMut::new(&mut out));
    // y0 = b0; y1 = b1 - a1 y0; y2 = b2 - a1 y1 - a2 y0; y3 = -a1 y2 - a2 y1.
    let y1 = 0.25 + 0.5 * 0.5;
    let y2 = -0.125 + 0.5 * y1 - 0.25 * 0.5;
    let y3 = 0.5 * y2 - 0.25 * y1;
    for (got, want) in out.iter().zip([0.5, y1, y2, y3]) {
        assert!((f64::from(*got) - want).abs() < 1e-7, "{got} vs {want}");
    }
    // Held over [9, 1, 2], then a section fed 0 answers as if 1 then 2 had
    // passed through it unchanged.
    let mut held = DirectForm1::default();
    held.hold(Mono::new(&[9.0f32, 1.0, 2.0]));
    let mut after = [0.0f32];
    held.process(&c, MonoMut::new(&mut after));
    let want = c.b1 * 2.0 + c.b2 * 1.0 - c.a1 * 2.0 - c.a2 * 1.0;
    assert!((f64::from(after[0]) - want).abs() < 1e-7);
    held.reset();
    assert_eq!(held, DirectForm1::default());
}

/// In `f64` a block through `process` is the input convolved with the section's impulse
/// response, to f64 rounding (an `f32` round trip would be off by 1e-8), and `hold` leaves
/// the last two inputs as both histories.
#[test]
fn direct_form_i_in_f64_is_the_input_convolved_with_the_impulse_response() {
    let c = Biquad {
        b0: 0.1,
        b1: 0.37,
        b2: -0.23,
        a1: -0.61,
        a2: 0.29,
    };
    let input = [0.9f64, -0.41, 0.13, 0.77, -0.58, 0.31, 0.07, -0.93];
    // The impulse response of the all-pole recurrence, then the numerator on top.
    let b = [c.b0, c.b1, c.b2];
    let mut h = [0.0f64; 8];
    for n in 0..h.len() {
        let ar = -c.a1 * n.checked_sub(1).map_or(0.0, |k| h[k])
            - c.a2 * n.checked_sub(2).map_or(0.0, |k| h[k]);
        h[n] = b.get(n).copied().unwrap_or(0.0) + ar;
    }
    let mut got = input;
    DirectForm1::default().process(&c, MonoMut::new(&mut got));
    for n in 0..input.len() {
        let want: f64 = (0..=n).map(|k| h[k] * input[n - k]).sum();
        assert!(
            (got[n] - want).abs() < 1e-13,
            "frame {n}: {} vs {want}",
            got[n]
        );
    }
    let mut held = DirectForm1::default();
    held.hold(Mono::new(&[9.1f64, 1.1, 2.3]));
    let mut after = [0.0f64];
    held.process(&c, MonoMut::new(&mut after));
    let want = c.b1 * 2.3 + c.b2 * 1.1 - c.a1 * 2.3 - c.a2 * 1.1;
    assert!((after[0] - want).abs() < 1e-15, "{} vs {want}", after[0]);
}
