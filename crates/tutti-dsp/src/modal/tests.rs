use super::*;
use core::f64::consts::TAU;
use tutti_buffers::{InputOutput, Planes, PlanesMut};

const LN_1000: f64 = 3.0 * core::f64::consts::LN_10;
const RATE: f64 = 48_000.0;

fn modes() -> [Mode; 3] {
    [
        Mode {
            frequency: Hz::new(440.0),
            decay: Seconds::new(0.20),
            gain: Gain::new(1.0),
        },
        Mode {
            frequency: Hz::new(1_130.0),
            decay: Seconds::new(0.12),
            gain: Gain::new(-0.5),
        },
        Mode {
            frequency: Hz::new(3_070.0),
            decay: Seconds::new(0.05),
            gain: Gain::new(0.25),
        },
    ]
}

fn modes_of(modes: &[Mode]) -> Modes {
    Modes::try_from(modes).unwrap()
}

fn bank(modes: &[Mode]) -> ModalBank<f64> {
    ModalBank::new(&modes_of(modes), ChannelLayout::MONO, SampleRate::of(RATE))
}

fn impulse(b: &mut ModalBank<f64>, len: usize) -> Vec<f64> {
    let mut x = vec![0.0; len];
    x[0] = 1.0;
    let mut y = vec![0.0; len];
    b.process(
        &mut InputOutput::paired(
            Planes::new(&[&x[..]]).unwrap(),
            PlanesMut::new(&mut [&mut y[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    y
}

/// `sum g exp(-t ln(1000) / T60) sin(2 pi f t)` at `t = n / fs`: the modes' damped sinusoids.
fn damped_sinusoids(modes: &[Mode], len: usize, f_scale: f64, t_scale: f64) -> Vec<f64> {
    (0..len)
        .map(|n| {
            let t = n as f64 / RATE;
            modes
                .iter()
                .map(|m| {
                    m.gain.get()
                        * (-t * LN_1000 / (m.decay.get() * t_scale)).exp()
                        * (TAU * m.frequency.get() * f_scale * t).sin()
                })
                .sum()
        })
        .collect()
}

/// Round-off bound of the recursion over `len` frames: each step adds at most `4 eps` of the
/// state's size, and a state of size `S` fed by `g r sin w` reaches at most `sum |g| / (1 - r)`
/// with `1 - r` the slowest mode's decay per frame.
fn bound(modes: &[Mode], len: usize) -> f64 {
    let slowest = modes.iter().map(|m| m.decay.get()).fold(0.0, f64::max);
    let one_minus_r = 1.0 - (-LN_1000 / (slowest * RATE)).exp();
    let gain: f64 = modes.iter().map(|m| m.gain.get().abs()).sum();
    4.0 * f64::EPSILON * len as f64 * gain / one_minus_r
}

/// The impulse response is the sum of the modes' damped sinusoids
/// `g exp(-t ln(1000) / T60) sin(2 pi f t)`, sampled: the impulse-invariant design. It is zero at
/// frame 0 (no direct term).
#[test]
fn the_impulse_response_is_the_sum_of_the_damped_sinusoids() {
    let m = modes();
    let h = impulse(&mut bank(&m), 4096);
    let want = damped_sinusoids(&m, 4096, 1.0, 1.0);
    assert_eq!(h[0], 0.0);
    let worst = h
        .iter()
        .zip(&want)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max);
    assert!(
        worst < bound(&m, 4096),
        "{worst} over the bound {}",
        bound(&m, 4096)
    );
    assert!(
        want.iter().any(|v| v.abs() > 0.5),
        "the response is not silent"
    );
}

/// The first mode's frequency and decay are the controls; the other modes keep their ratio to
/// it: doubling the frequency and halving the decay is every mode at twice its frequency with
/// half its decay time.
#[test]
fn the_controls_scale_every_mode_by_its_ratio_to_the_first() {
    let m = modes();
    let mut b = bank(&m);
    b.retarget(&ModalControls {
        frequency: Hz::new(880.0),
        decay: Seconds::new(0.10),
    });
    let h = impulse(&mut b, 4096);
    let want = damped_sinusoids(&m, 4096, 2.0, 0.5);
    let worst = h
        .iter()
        .zip(&want)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max);
    assert!(worst < bound(&m, 4096), "{worst}");
}

/// A mode at or above the Nyquist frequency is silent (at three quarters of the rate it would
/// alias to a sinusoid of a quarter) and the others are not disturbed.
#[test]
fn a_mode_at_or_above_nyquist_is_silent() {
    let mut m = modes().to_vec();
    m.push(Mode {
        frequency: Hz::new(RATE / 2.0),
        decay: Seconds::new(1.0),
        gain: Gain::new(1.0),
    });
    m.push(Mode {
        frequency: Hz::new(0.75 * RATE),
        decay: Seconds::new(1.0),
        gain: Gain::new(1.0),
    });
    m.push(Mode {
        frequency: Hz::new(RATE),
        decay: Seconds::new(1.0),
        gain: Gain::new(1.0),
    });
    let h = impulse(&mut bank(&m), 1024);
    let want = damped_sinusoids(&modes(), 1024, 1.0, 1.0);
    let worst = h
        .iter()
        .zip(&want)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max);
    assert!(worst < bound(&modes(), 1024), "{worst}");
}

/// A bank holds `1..=MAX_MODES` modes.
#[test]
fn the_mode_count_is_from_one_to_the_most() {
    let one = modes()[0];
    assert!(Modes::try_from(&[][..]).is_err());
    assert!(Modes::try_from(&vec![one; MAX_MODES][..]).is_ok());
    assert!(Modes::try_from(&vec![one; MAX_MODES + 1][..]).is_err());
}

/// The input is the excitation: a unit step in at frame `k` rings `h[n - k]` on each of two
/// channels, and a channel is silent until its own input arrives.
#[test]
fn a_late_impulse_rings_the_same_response_later_on_every_channel() {
    let m = modes();
    let mut b = ModalBank::<f64>::new(
        &modes_of(&m),
        ChannelLayout::from_count(2),
        SampleRate::of(RATE),
    );
    let mut x0 = vec![0.0; 512];
    x0[100] = 1.0;
    let mut x1 = vec![0.0; 512];
    x1[200] = 1.0;
    let (mut y0, mut y1) = (vec![0.0; 512], vec![0.0; 512]);
    b.process(
        &mut InputOutput::paired(
            Planes::new(&[&x0[..], &x1[..]]).unwrap(),
            PlanesMut::new(&mut [&mut y0[..], &mut y1[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let h = impulse(&mut bank(&m), 412);
    assert!(y1[..=200].iter().all(|&v| v == 0.0));
    assert!(y0[..=100].iter().all(|&v| v == 0.0));
    for n in 0..312 {
        assert!((y1[200 + n] - h[n]).abs() < 1e-15, "channel 1 frame {n}");
    }
    for n in 0..412 {
        assert!((y0[100 + n] - h[n]).abs() < 1e-15, "frame {n}");
    }
}

/// The declared tail covers the ring-out: nothing past it is above `TAIL_FLOOR` (-60 dB).
#[test]
fn the_declared_tail_covers_the_ring_out() {
    let m = modes();
    let mut b = bank(&m);
    let controls = ModalControls::of_first(&modes_of(&m));
    let Tail::Finite(tail) = b.tail(&controls) else {
        panic!("a finite tail")
    };
    let h = impulse(&mut b, tail.get() + 20_000);
    let last = h.iter().rposition(|y| y.abs() > 1e-3).unwrap();
    assert!(
        last < tail.get(),
        "rings to {last}, declared {}",
        tail.get()
    );
    assert!(last > tail.get() / 10, "the tail is not absurdly long");
}
