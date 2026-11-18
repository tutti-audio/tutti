use super::*;

/// **A voice's expression resets to its defaults**: no pressure, the slide at
/// its centre, the gain at unity.
#[test]
fn reset_returns_the_controllers_to_their_defaults() {
    let mut state = MpeVoiceState::new();
    state.apply(MpeControl::Pressure(Normalized::of(1.0)));
    state.apply(MpeControl::Slide(Normalized::of(1.0)));
    state.apply(MpeControl::Gain(Normalized::of(0.0)));
    state.reset();
    assert_eq!(state.pressure, Normalized::of(0.0));
    assert_eq!(state.slide, SLIDE_CENTER);
    assert_eq!(state.gain, Normalized::of(1.0));
}
