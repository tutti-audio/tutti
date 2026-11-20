//! The meter and the count-in a block's transport carries.
//!
//! The oracle is the map's own arithmetic: a bar of `n/d` is `4n/d` quarter
//! notes from the change that put it in force.

use tutti_graph::{BlockMeter, Env, Offset, PlayClock, TransportChanges, TransportPosition};
use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_units::{Beat, Bpm, Frame, SampleRate, Samples};

fn map() -> MeterMap {
    MeterMap::new([
        MeterChange::new(Beat::new(0.0), TimeSignature::of(4, 4)),
        MeterChange::new(Beat::new(8.0), TimeSignature::of(7, 8)),
    ])
}

/// **A block meter names the signature in force, the bar line its bars are
/// counted from and the change after it.** Beat 9.0 is in the 7/8 bar from
/// 8.0 with no change after it; beat 5.0 is in the 4/4 bar from 4.0, with the
/// change at 8.0 ahead; pre-roll at beat -1.0 is in the 4/4 bar from -4.0, and
/// the map's entry at beat 0 (a change begins a bar, even to the same
/// signature) is the next change.
#[test]
fn a_block_meter_is_the_signature_bar_line_and_next_change_at_a_beat() {
    let four = TimeSignature::of(4, 4);
    let seven = TimeSignature::of(7, 8);
    let change = MeterChange::new(Beat::new(8.0), seven);
    assert_eq!(
        BlockMeter::of(&map(), Beat::new(9.0)),
        BlockMeter::new(seven, Beat::new(8.0), None)
    );
    assert_eq!(
        BlockMeter::of(&map(), Beat::new(5.0)),
        BlockMeter::new(four, Beat::new(4.0), Some(change))
    );
    assert_eq!(
        BlockMeter::of(&map(), Beat::new(-1.0)),
        BlockMeter::new(
            four,
            Beat::new(-4.0),
            Some(MeterChange::new(Beat::new(0.0), four))
        )
    );
    // At the change itself the new meter is in force and nothing is ahead.
    assert_eq!(
        BlockMeter::of(&map(), Beat::new(8.0)),
        BlockMeter::new(seven, Beat::new(8.0), None)
    );
}

/// **A transport built by a constructor has no meter and is not counting in;
/// each is set by its own builder method and read back.**
#[test]
fn a_transport_has_no_meter_and_no_count_in_until_stamped() {
    let t = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
    assert_eq!((t.meter(), t.count_in()), (None, false));
    let meter = BlockMeter::new(TimeSignature::of(3, 4), Beat::new(0.0), None);
    let stamped = t.metered(meter).counting_in(true);
    assert_eq!((stamped.meter(), stamped.count_in()), (Some(meter), true));
    assert!(stamped.playing, "the other fields are unchanged");
}

/// **A position walked on inside the block keeps the block's meter and
/// count-in**: a meter change further on is the meter's next change, not a
/// restamp.
#[test]
fn a_walked_position_keeps_the_meter_and_the_count_in() {
    let meter = BlockMeter::new(
        TimeSignature::of(4, 4),
        Beat::new(4.0),
        Some(MeterChange::new(Beat::new(8.0), TimeSignature::of(7, 8))),
    );
    let t = TransportPosition::new(true, Bpm::of(120.0), Beat::new(7.9), None)
        .metered(meter)
        .counting_in(true);
    let env = Env {
        frame: Frame::new(0),
        sample_rate: SampleRate::SR_48K,
        block_len: Samples::new(4_096),
        transport: t,
        changes: TransportChanges::NONE,
        play: PlayClock::ZERO,
    };
    let walked = env.transport_at(Offset::of(Samples::new(3_000), Samples::new(4_096)));
    assert!(walked.beat() > Beat::new(8.0), "walked past the change");
    assert_eq!((walked.meter(), walked.count_in()), (Some(meter), true));
}
