use super::*;

/// A step closes `coeff` of the gap: a tenth up, a tenth down, all of it
/// at 1, none at 0.
#[test]
fn a_step_closes_the_coefficient_of_the_gap() {
    assert!((Smoother::toward(0.0, 1.0, 0.1) - 0.1).abs() < 0.001);
    assert!((Smoother::toward(1.0, 0.0, 0.1) - 0.9).abs() < 0.001);
    assert_eq!(Smoother::toward(0.0, 1.0, 1.0), 1.0);
    assert_eq!(Smoother::toward(0.25, 1.0, 0.0), 0.25);
    let mut s = Smoother::new(0.5);
    s.seed(0.0);
    assert_eq!(s.step(1.0), 0.5);
    assert_eq!(s.step(1.0), 0.75);
    assert_eq!(s.value(), 0.75);
}
