use super::MonotonicMinDeque;

/// A rising run (a limiter's release) keeps every push: the deque fills
/// to the window and must not grow past the allocation `new` made.
#[test]
fn a_rising_run_never_grows_the_deque() {
    let window = 240;
    let mut d = MonotonicMinDeque::new(window);
    let cap = d.entries.capacity();
    for i in 0..10 * window as u64 {
        d.push(i, i as f32);
        assert_eq!(d.entries.capacity(), cap, "grew at push {i}");
        // The window minimum of a rising run is its oldest live entry.
        let oldest = i.saturating_sub(window as u64 - 1);
        assert_eq!(d.min(), Some(oldest as f32));
    }
}

/// A clone of a deque that holds fewer entries than its window keeps
/// the window's allocation, so a rising run on the clone does not grow.
#[test]
fn a_clone_keeps_the_window_allocation() {
    let window = 240;
    let mut d = MonotonicMinDeque::new(window);
    d.push(0, 1.0);
    let mut c = d.clone();
    assert!(
        c.entries.capacity() >= window,
        "clone capacity {}",
        c.entries.capacity()
    );
    let cap = c.entries.capacity();
    for i in 1..4 * window as u64 {
        c.push(i, i as f32);
    }
    assert_eq!(c.entries.capacity(), cap);
    assert_eq!(c.window, window);
}

/// The minimum is over exactly the last `window` pushes.
#[test]
fn the_minimum_spans_exactly_the_window() {
    let mut d = MonotonicMinDeque::new(3);
    d.push(0, 0.0);
    d.push(1, 5.0);
    d.push(2, 6.0);
    assert_eq!(d.min(), Some(0.0));
    d.push(3, 7.0);
    assert_eq!(d.min(), Some(5.0), "index 0 left the window of 3");
}
