use super::DelayRing;

/// After `n + 1` pushes of the ramp `0, 1, 2, ..` the newest value is `n`, one slot
/// back, so `back(k)` is the ramp's value `n + 1 - k`, at every `k` the ring holds and
/// across several wraps.
#[test]
fn a_read_k_slots_back_is_the_value_put_k_frames_earlier_across_the_wrap() {
    const LEN: usize = 5;
    let mut ring = DelayRing::new(LEN);
    for n in 0..4 * LEN + 3 {
        ring.push(n as f32);
        for k in 1..=LEN.min(n + 1) {
            assert_eq!(
                ring.back(k),
                (n + 1 - k) as f32,
                "after {n} pushes, {k} back"
            );
        }
    }
}

/// A slot before its `put` still holds the frame `len` back: `back(0)` and `back(len)`
/// are the oldest.
#[test]
fn the_slot_at_the_cursor_holds_the_oldest_frame_until_put() {
    let mut ring = DelayRing::new(3);
    for x in [1.0, 2.0, 3.0] {
        ring.push(x);
    }
    assert_eq!(ring.oldest(), 1.0);
    assert_eq!(ring.back(0), 1.0);
    assert_eq!(ring.back(3), 1.0);
    ring.put(9.0);
    assert_eq!(ring.back(0), 9.0);
}

/// Between whole slots `d` and `d + 1` holding `a` and `b`, a read at `d + 0.5` is
/// `(a + b) / 2`, and at a whole delay it is the slot itself.
#[test]
fn a_read_halfway_between_two_slots_is_their_mean() {
    let mut ring = DelayRing::new(8);
    for n in 0..11 {
        ring.push((n * n) as f32);
    }
    // The newest value, 100, is one slot back: the cursor has advanced past it.
    for d in 1..7 {
        let (a, b) = (ring.back(d), ring.back(d + 1));
        assert_eq!(ring.at(d as f32 + 0.5), (a + b) / 2.0);
        assert_eq!(ring.at(d as f32), a);
    }
}

/// A ring of `len` holds exactly `len` frames: the frame pushed `len` frames ago is
/// the oldest it reads, and `clear` returns it to silence.
#[test]
fn the_ring_holds_its_length_and_clear_silences_it() {
    let mut ring = DelayRing::new(4);
    for x in 1..=6 {
        ring.push(x as f32);
    }
    assert_eq!(ring.len(), 4);
    assert_eq!(ring.back(4), 3.0);
    ring.clear();
    assert!((0..=4).all(|k| ring.back(k) == 0.0));
}

/// A ring in `f64` returns the 29-bit signal `len` frames later exactly, across several
/// wraps: nothing is narrowed to `f32` on the way through.
#[test]
fn an_f64_ring_returns_a_wide_signal_len_frames_later_across_the_wrap() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    const LEN: usize = 5;
    let signal = wide_signal(6 * LEN + 2);
    let mut ring = DelayRing::<f64>::new(LEN);
    let got: Vec<f64> = signal
        .iter()
        .map(|&x| {
            let out = ring.oldest();
            ring.push(x);
            out
        })
        .collect();
    let want: Vec<f64> = (0..signal.len())
        .map(|i| if i >= LEN { signal[i - LEN] } else { 0.0 })
        .collect();
    assert_close_f64(&got, &want, 0.0);
    assert_ne!(
        signal[0] as f32 as f64, signal[0],
        "the signal needs 29 bits"
    );
}

/// A fractional read halfway between two wide `f64` slots is their mean to the rounding of
/// one `f64` operation, which an `f32` lerp would miss by about `2^-25`.
#[test]
fn an_f64_fractional_read_is_the_mean_of_two_wide_slots_to_f64_rounding() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let signal = wide_signal(8);
    let mut ring = DelayRing::<f64>::new(8);
    signal.iter().for_each(|&x| ring.push(x));
    let got: Vec<f64> = (1..7).map(|d| ring.at(d as f64 + 0.5)).collect();
    let want: Vec<f64> = (1..7)
        .map(|d| (signal[8 - d] + signal[8 - d - 1]) / 2.0)
        .collect();
    assert_close_f64(&got, &want, 2f64.powi(-50));
}

/// A read at `100000.3` frames on a long `f64` ring is the closed-form lerp of its two wide
/// slots at the `f64` fraction `100000.3 - 100000`, to `2^-45`: an `f32` position (fraction
/// error near `4e-3`) or an `f32` slot (error near `2^-25`) misses it.
#[test]
fn an_f64_read_at_a_position_needing_more_than_24_bits_is_the_lerp_of_the_wide_slots() {
    use tutti_fixtures::signal::wide_signal;
    const N: usize = 200_001;
    let signal = wide_signal(N);
    let mut ring = DelayRing::<f64>::new(N);
    signal.iter().for_each(|&x| ring.push(x));
    let pos = 100_000.3_f64;
    let frac = pos - 100_000.0;
    let (a, b) = (signal[N - 100_000], signal[N - 100_001]);
    let want = a + frac * (b - a);
    let got = ring.at(pos);
    assert!(
        (got - want).abs() <= 2f64.powi(-45),
        "got {got} want {want}"
    );
}
