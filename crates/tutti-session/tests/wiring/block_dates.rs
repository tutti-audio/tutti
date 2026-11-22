//! `SessionBuilder::block_dates`: a `Frames` offline session dates its blocks by the
//! frames rendered, and the playhead reads each date back.

use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_session::{BlockDates, Session};
use tutti_units::{SampleRate, Samples};

fn session(dates: BlockDates, rate: f64) -> Session {
    Session::builder()
        .sample_rate(SampleRate::of(rate))
        .max_block(MaxBlock::of(Samples::new(512)))
        .block_dates(dates)
        .build()
        .expect("builds")
}

fn render(s: &mut Session) {
    let mut block = vec![0.0f32; 512 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
}

/// **A `Frames` session's block `n` is dated `n * 512 / rate` after block 0**, to the
/// nanosecond, at 48 kHz and at 44.1 kHz; no block before the first is dated.
#[test]
fn frames_date_the_sessions_blocks_by_the_frames_rendered() {
    for rate in [48_000u64, 44_100] {
        let mut s = session(BlockDates::Frames, rate as f64);
        assert_eq!(s.playhead().at(), None, "nothing is dated before a block");
        render(&mut s);
        let origin = s.playhead().at().expect("block 0 is dated");
        for n in 1..=100u128 {
            render(&mut s);
            let at = s.playhead().at().expect("dated");
            let exact = (n * 512 * 1_000_000_000 + u128::from(rate) / 2) / u128::from(rate);
            assert_eq!(at - origin, exact as i64, "block {n} at {rate} Hz");
        }
    }
}

/// **`Os` dates never go back.**
#[test]
fn os_dates_never_go_back() {
    let mut s = session(BlockDates::Os, 48_000.0);
    let mut last = i64::MIN;
    for _ in 0..20 {
        render(&mut s);
        let at = s.playhead().at().expect("dated");
        assert!(at >= last);
        last = at;
    }
}
