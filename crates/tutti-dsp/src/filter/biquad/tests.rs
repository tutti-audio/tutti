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
