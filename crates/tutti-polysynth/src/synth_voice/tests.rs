use super::*;
use tutti_units::Semitones;

const BASE: Hz = Hz::new(1000.0);

fn voice(lfo_rate: Option<Hz>) -> SynthVoice {
    let filter_mod = FilterModConfig::new().lfo_depth(Bipolar::MAX);
    let config = SynthConfig::new()
        .filter(FilterType::Moog {
            cutoff: BASE,
            resonance: Resonance::NONE,
        })
        .filter_mod(match lfo_rate {
            Some(rate) => filter_mod.lfo_rate(rate),
            None => filter_mod,
        });
    SynthVoice::from_config(&config, SampleRate::SR_48K, UnisonVoices::ONE)
}

/// **A filter LFO with no rate does not move the cutoff**, whatever its depth:
/// the rate is what makes the sweep exist.
#[test]
fn an_lfo_with_no_rate_leaves_the_cutoff_at_its_base() {
    let mut voice = voice(None);
    let channel = ChannelState::new(Semitones::new(2.0));
    for _ in 0..4 {
        assert_eq!(
            voice.modulated_cutoff(2400, SampleRate::SR_48K, &channel),
            BASE
        );
    }
}

/// **The LFO sweeps the cutoff by `1 + sin(phase) * depth / 2`**: after a
/// quarter period of a 5 Hz LFO (2400 frames at 48 kHz) the sine is at its
/// crest, so a full depth lifts the cutoff by 50%.
#[test]
fn an_lfo_sweeps_the_cutoff_by_half_its_depth_at_the_crest() {
    let mut voice = voice(Some(Hz::new(5.0)));
    let channel = ChannelState::new(Semitones::new(2.0));
    let cutoff = voice.modulated_cutoff(2400, SampleRate::SR_48K, &channel);
    assert!(
        (cutoff.get() - 1.5 * BASE.get()).abs() < 0.5,
        "the crest cutoff is {cutoff:?}, expected 1500 Hz"
    );
}

/// A control `x` off its centre scales the cutoff by `4^x`: two octaves per unit, so a half
/// unit up doubles it, a half unit down halves it and the centre leaves it alone.
#[test]
fn the_cutoff_control_moves_two_octaves_per_unit() {
    assert_eq!(two_octave_factor(0.0), 1.0);
    assert_eq!(two_octave_factor(0.5), 2.0);
    assert_eq!(two_octave_factor(-0.5), 0.5);
    assert_eq!(two_octave_factor(1.0), 4.0);
}

/// **A channel's pan is RP-036's gains times `sqrt(2)`**: the centre is exactly `(1, 1)`, hard
/// left is `(sqrt(2), 0)` and hard right `(0, sqrt(2))`, the dead side exactly 0 with no
/// tolerance, a pan past an end is that end, and a quarter right has the right over the left
/// at `tan(Pi/4 * 1.25)`.
#[test]
fn a_channels_pan_is_rp_036_scaled_to_unity_at_the_centre_with_exact_dead_sides() {
    let pair = |p: f64| {
        let (l, r) = unity_centre_pan(Pan::new(p));
        (l.get(), r.get())
    };
    let root = std::f64::consts::SQRT_2;
    assert_eq!(pair(0.0), (1.0, 1.0));
    assert_eq!(pair(-1.0), (root, 0.0));
    assert_eq!(pair(1.0), (0.0, root));
    assert_eq!(pair(-7.0), (root, 0.0));
    assert_eq!(pair(f64::NAN), (1.0, 1.0));
    let (l, r) = pair(0.25);
    let want = (std::f64::consts::FRAC_PI_4 * 1.25).tan();
    assert!(((r / l) - want).abs() < 1e-14, "{}", r / l);
}
