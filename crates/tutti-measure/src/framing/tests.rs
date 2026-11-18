use super::*;

const WINDOW: Samples = Samples(1024);
const HOP: Samples = Samples(256);

/// `(len - window) / hop + 1` whole frames fit: none below one window, one
/// at exactly a window, the next at a window plus a hop.
#[test]
fn whole_frames_fit_at_window_plus_a_hop_each() {
    let f = Framing::new(WINDOW, HOP);
    for (len, frames) in [
        (0, 0),
        (1023, 0),
        (1024, 1),
        (1279, 1),
        (1280, 2),
        (4096, 13),
    ] {
        assert_eq!(f.frames_for(Samples(len)), FrameCount(frames), "{len}");
    }
}

/// A zero window or hop fits no frame, rather than dividing by zero.
#[test]
fn a_zero_hop_or_window_fits_no_frame() {
    assert_eq!(
        Framing::new(WINDOW, Samples(0)).frames_for(Samples(9999)),
        FrameCount(0)
    );
    assert_eq!(
        Framing::new(Samples(0), HOP).frames_for(Samples(9999)),
        FrameCount(0)
    );
}

/// A hop wider than the window leaves the samples between frames unread:
/// frames start at `0, hop, 2 hop ..` and the last must end inside `len`.
#[test]
fn a_hop_wider_than_the_window_counts_the_frames_that_fit() {
    let f = Framing::new(Samples(100), Samples(300));
    assert_eq!(f.frames_for(Samples(99)), FrameCount(0));
    assert_eq!(f.frames_for(Samples(100)), FrameCount(1));
    assert_eq!(f.frames_for(Samples(399)), FrameCount(1));
    assert_eq!(f.frames_for(Samples(400)), FrameCount(2));
}

/// The window range includes both ends.
#[test]
fn the_window_range_includes_both_ends() {
    let accepted = Samples(64)..=Samples(256);
    assert!(check_window(Samples(64), &accepted).is_ok());
    assert!(check_window(Samples(256), &accepted).is_ok());
    for window in [63, 257] {
        assert_eq!(
            check_window(Samples(window), &accepted),
            Err(Error::WindowOutOfRange {
                window: Samples(window),
                min: Samples(64),
                max: Samples(256)
            })
        );
    }
}

/// A hop equal to the window is within it; one more sample is not.
#[test]
fn a_hop_is_within_a_window_up_to_its_length() {
    assert!(check_hop_within(WINDOW, WINDOW).is_ok());
    assert_eq!(
        check_hop_within(WINDOW, Samples(1025)),
        Err(Error::HopWiderThanWindow {
            hop: Samples(1025),
            window: WINDOW
        })
    );
}

/// Exactly `MAX_OVERLAP` hops across a window are accepted: a window of 64
/// hops is, a window of 64 hops and one sample is not.
#[test]
fn the_overlap_limit_is_exact() {
    assert!(check_overlap(Samples(4096), Samples(64)).is_ok());
    assert_eq!(
        check_overlap(Samples(4097), Samples(64)),
        Err(Error::TooManyOverlaps {
            window: Samples(4097),
            hop: Samples(64),
            max: MAX_OVERLAP
        })
    );
}

/// The frame limit is inclusive.
#[test]
fn the_frame_limit_is_inclusive() {
    let max = FrameCount(10);
    assert!(check_frame_count(FrameCount(10), max).is_ok());
    assert_eq!(
        check_frame_count(FrameCount(11), max),
        Err(Error::TooManyFrames {
            frames: FrameCount(11),
            max
        })
    );
}
