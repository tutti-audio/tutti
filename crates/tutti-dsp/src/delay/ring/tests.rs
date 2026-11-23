use super::DelayRing;
use tutti_units::{ReadPosition, SampleRate, Seconds};

/// The position `frames` frames back, to the quantum.
fn pos(frames: f64) -> ReadPosition {
    ReadPosition::from_count((frames * ReadPosition::COUNTS_PER_FRAME).round() as u64)
}

// The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
// format, so their tolerances are `f32`'s.

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
        assert_eq!(ring.at(pos(d as f64 + 0.5)), (a + b) / 2.0);
        assert_eq!(ring.at(pos(d as f64)), a);
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
    use tutti_fixtures::signal::{assert_close, wide_signal};
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
    assert_close(&got, &want, 0.0);
    assert_ne!(
        signal[0] as f32 as f64, signal[0],
        "the signal needs 29 bits"
    );
}

/// A fractional read halfway between two wide `f64` slots is their mean to the rounding of
/// one `f64` operation, which an `f32` lerp would miss by about `2^-25`.
#[test]
fn an_f64_fractional_read_is_the_mean_of_two_wide_slots_to_f64_rounding() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let signal = wide_signal(8);
    let mut ring = DelayRing::<f64>::new(8);
    signal.iter().for_each(|&x| ring.push(x));
    let got: Vec<f64> = (1..7).map(|d| ring.at(pos(d as f64 + 0.5))).collect();
    let want: Vec<f64> = (1..7)
        .map(|d| (signal[8 - d] + signal[8 - d - 1]) / 2.0)
        .collect();
    assert_close(&got, &want, 2f64.powi(-50));
}

/// A read at `100000.3` frames on a long `f64` ring is the closed-form lerp of its two wide
/// slots at the position's own fraction, to `2^-45`: an `f32` position (fraction
/// error near `4e-3`) or an `f32` slot (error near `2^-25`) misses it.
#[test]
fn an_f64_read_at_a_position_needing_more_than_24_bits_is_the_lerp_of_the_wide_slots() {
    use tutti_fixtures::signal::wide_signal;
    const N: usize = 200_001;
    let signal = wide_signal(N);
    let mut ring = DelayRing::<f64>::new(N);
    signal.iter().for_each(|&x| ring.push(x));
    let at = pos(100_000.3);
    let frac = (at.get() & 0xffff_ffff) as f64 / ReadPosition::COUNTS_PER_FRAME;
    let (a, b) = (signal[N - 100_000], signal[N - 100_001]);
    let want = a + frac * (b - a);
    let got = ring.at(at);
    assert!(
        (got - want).abs() <= 2f64.powi(-45),
        "got {got} want {want}"
    );
}

/// 9.9 s at 192 kHz is 1 900 800 frames. A read of 0.3 frames more, on an `f32` ring of the
/// ramp `0, 1, 2, ..`, is the ramp's linear interpolation at that fraction: an `f32` delay
/// has steps of 1/8 frame at that distance, so a position held in it reads 0.25 or 0.375 and
/// misses by 0.05 or more. The fraction is cut to its top 24 bits (error under `2^-24`),
/// and the interpolation of `3` and `2` rounds three `f32` operations (half-ulps of `2^-23`
/// at `3 * (1 - t)`, `2^-25` at `2 * t` and `2^-23` at the sum) plus `(b - a) * 2^-24`:
/// `12 * 2^-25` bounds them.
#[test]
fn an_f32_read_at_9_9_seconds_and_a_third_of_a_frame_interpolates_at_that_fraction() {
    const WHOLE: usize = 1_900_800;
    const N: usize = WHOLE + 3;
    let mut ring = DelayRing::<f32>::new(N);
    for n in 0..N {
        ring.push(n as f32);
    }
    let at = ReadPosition::of(
        Seconds::new((WHOLE as f64 + 0.3) / 192_000.0),
        SampleRate::SR_192K,
    );
    assert_eq!(at.frames().get(), WHOLE);
    // `back(k)` is the ramp's value `N - k`, so a read `WHOLE + 0.3` back is `3 - 0.3`.
    let want = 3.0 - 0.3;
    let got = f64::from(ring.at(at));
    assert!(
        (got - want).abs() <= 12.0 * 2f64.powi(-25),
        "got {got} want {want}"
    );
}

/// Sweeping an `f64` read through a whole-frame boundary in steps of one quantum moves the
/// value by the ramp's slope (1 a frame) times the quantum, and the interpolation's rounding
/// (a few `f64` half-ulps of values under 2048): no step jumps, at the boundary or either
/// side.
#[test]
fn an_f64_read_swept_across_a_whole_frame_moves_by_the_slope_times_the_quantum() {
    const LEN: usize = 2048;
    let mut ring = DelayRing::<f64>::new(LEN);
    for n in 0..LEN {
        ring.push(n as f64);
    }
    let boundary = 1000_u64 << 32;
    let quantum = 2f64.powi(-32);
    let mut last = ring.at(ReadPosition::from_count(boundary - 64));
    for count in boundary - 63..=boundary + 64 {
        let now = ring.at(ReadPosition::from_count(count));
        let jump = (now - last).abs();
        assert!(
            (jump - quantum).abs() <= 2f64.powi(-40),
            "count {count}: jump {jump}"
        );
        last = now;
    }
}
