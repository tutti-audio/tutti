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
