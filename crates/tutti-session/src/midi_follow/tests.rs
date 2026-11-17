use super::*;
use tutti_types::SampleRate;

const RATE: SampleRate = SampleRate::of(48_000.0);

/// **A command is placed `CLOCK_LEAD` after its message's arrival frame.** A
/// message at frame 48 000 (1 s at 48 kHz) takes effect at
/// `48 000 + 0.020 * 48 000 = 48 960`, whatever the poll that read it, as long as
/// that frame is not behind the next block.
#[test]
fn a_command_lands_the_lead_after_its_arrival_frame() {
    let arrival = Frame::new(48_000);
    let next = Frame::new(48_512);
    assert_eq!(
        placed_frame(arrival.to_seconds(RATE), RATE, next),
        Frame::new(48_960)
    );
    // Two messages a poll apart keep their spacing: 480 frames (10 ms) on.
    let later = Frame::new(48_480);
    assert_eq!(
        placed_frame(later.to_seconds(RATE), RATE, next),
        Frame::new(49_440)
    );
}

/// **A frame the audio thread has passed is the next block's first.** An
/// arrival at frame 48 000 read by a poll whose next block starts at 50 000
/// (past `48 960`) lands at 50 000, where a command in the past would land
/// anyway.
#[test]
fn a_command_past_the_audio_thread_lands_at_the_next_block() {
    let arrival = Frame::new(48_000).to_seconds(RATE);
    assert_eq!(
        placed_frame(arrival, RATE, Frame::new(50_000)),
        Frame::new(50_000)
    );
}
