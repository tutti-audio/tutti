use super::*;
use crate::fold_uneven;

const RATE: f64 = 1000.0; // 1 sample = 1 ms keeps the arithmetic readable

fn cfg(threshold: Db, min_silence_ms: f64) -> SilenceConfig {
    SilenceConfig::new(
        FiniteLevel::new(threshold).unwrap(),
        Seconds::new(min_silence_ms / 1000.0),
    )
}

fn strip(c: &SilenceConfig, x: &[f32]) -> Vec<Region> {
    strip_silence(c, Mono::new(x), SampleRate::of(RATE))
}

fn r(start: usize, end: usize) -> Region {
    Region::new(Samples::new(start), Samples::new(end)).expect("an ascending region")
}

/// A span whose end is before its start is no region, so a region's length
/// never underflows; an equal start and end is the empty region.
#[test]
fn a_region_ends_no_earlier_than_it_starts() {
    assert_eq!(Region::new(Samples::new(10), Samples::new(4)), None);
    let empty = r(7, 7);
    assert!(empty.is_empty());
    assert_eq!(empty.len(), Samples::new(0));
    assert_eq!(r(4, 10).len(), Samples::new(6));
}

/// tone(100) silence(300) tone(100) silence(50), with nothing padded: two
/// regions with exact boundaries, the trailing silence dropped.
#[test]
fn tone_gap_tone_gives_two_exact_regions() {
    let mut x = vec![0.0f32; 550];
    x[..100].fill(0.5);
    x[400..500].fill(0.5);
    let regions = strip(&cfg(Db::new(-20.0), 50.0), &x);
    assert_eq!(regions, vec![r(0, 100), r(400, 500)]);
    assert_eq!(regions[0].len(), Samples::new(100));
    assert_eq!(&x[regions[1].range()], &[0.5f32; 100][..]);
}

/// Gaps shorter than the minimum do not split; one at the minimum does.
#[test]
fn a_gap_splits_only_at_the_minimum_length() {
    let build = |gap: usize| {
        let mut x = vec![0.0f32; 200 + gap];
        x[..100].fill(0.5);
        x[100 + gap..].fill(0.5);
        x
    };
    let c = cfg(Db::new(-20.0), 50.0);
    assert_eq!(strip(&c, &build(49)), vec![r(0, 249)]);
    assert_eq!(strip(&c, &build(50)), vec![r(0, 100), r(150, 250)]);
}

/// The least `f32` sample not under the threshold is loud; the float below it is not.
#[test]
fn the_threshold_edge_is_inclusive() {
    let c = cfg(Db::new(-20.0), 10.0);
    let floor = Db::new(-20.0).to_amplitude().get();
    let nearest = floor as f32;
    let at = if f64::from(nearest) >= floor {
        nearest
    } else {
        nearest.next_up()
    };
    let below = at.next_down();
    let mut x = vec![0.0f32; 100];
    x[10] = at;
    x[60] = below;
    assert_eq!(strip(&c, &x), vec![r(10, 11)]);
    // Sign does not matter.
    x[10] = -at;
    assert_eq!(strip(&c, &x), vec![r(10, 11)]);
}

/// Padding widens each side, clamps at the buffer and joins regions that
/// meet.
#[test]
fn padding_widens_clamps_and_joins() {
    let mut x = vec![0.0f32; 1000];
    x[5..10].fill(1.0);
    x[500..510].fill(1.0);
    x[990..995].fill(1.0);
    let c = cfg(Db::new(-20.0), 10.0)
        .pre_padding(Seconds::new(0.02))
        .post_padding(Seconds::new(0.03));
    // 5..10 -> 0..40; 500..510 -> 480..540; 990..995 -> 970..1000.
    assert_eq!(strip(&c, &x), vec![r(0, 40), r(480, 540), r(970, 1000)]);

    // Bursts 100..110 and then `gap` later: the first pads to 80..140, the
    // second starts at `90 + gap`. They meet exactly at gap 50 and join;
    // at 51 they stay one sample apart.
    let pair = |gap: usize| {
        let mut x = vec![0.0f32; 400];
        x[100..110].fill(1.0);
        x[110 + gap..120 + gap].fill(1.0);
        strip(&c, &x)
    };
    assert_eq!(pair(50), vec![r(80, 200)]);
    assert_eq!(pair(51), vec![r(80, 140), r(141, 201)]);
}

/// Silence only, empty input and NaN give no regions.
#[test]
fn nothing_loud_gives_no_regions() {
    let c = cfg(Db::new(-20.0), 10.0);
    assert!(strip(&c, &[0.0; 64]).is_empty());
    assert!(strip(&c, &[]).is_empty());
    assert!(strip(&c, &[f32::NAN; 8]).is_empty());
}

/// A meter fed chunks of any size gives the one-shot result bit for bit,
/// including a chunk edge inside a gap and inside a region, and a region
/// open at the end.
#[test]
fn a_meter_fed_in_chunks_equals_the_one_shot() {
    let mut x = vec![0.0f32; 700];
    x[3..40].fill(0.9);
    x[200..260].fill(-0.9);
    x[400..401].fill(0.9);
    x[690..].fill(0.9);
    let c = cfg(Db::new(-30.0), 40.0)
        .pre_padding(Seconds::new(0.005))
        .post_padding(Seconds::new(0.01));
    let batch = strip(&c, &x);
    assert_eq!(batch.len(), 4);
    for chunk in [1usize, 7, 64, 199, 700] {
        let mut meter = c.measure(SampleRate::of(RATE)).unwrap();
        for part in x.chunks(chunk) {
            meter.push(Mono::new(part), &mut ()).unwrap();
        }
        assert_eq!(meter.finish(), batch, "chunk {chunk}");
    }
}

/// The trait fold over uneven chunks (1, 7, 64 and the rest) is the batch
/// result, and a push yields no item.
#[test]
fn the_trait_fold_over_uneven_chunks_equals_strip_silence() {
    let mut x = vec![0.0f32; 700];
    x[3..40].fill(0.9);
    x[200..260].fill(-0.9);
    x[690..].fill(0.9);
    let c = cfg(Db::new(-30.0), 40.0).post_padding(Seconds::new(0.01));
    let (items, regions) = fold_uneven(&c, SampleRate::of(RATE), x.len(), |meter, at, out| {
        meter.push(Mono::new(&x[at]), out)
    })
    .unwrap();
    assert!(items.is_empty());
    assert_eq!(regions, strip(&c, &x));
    assert_eq!(regions.len(), 3);
}

/// Any sample format scans at its nominal level: 16-bit audio at half
/// scale is as loud as an `f32` half.
#[test]
fn an_integer_buffer_scans_at_its_nominal_level() {
    let mut x = vec![0i16; 300];
    x[100..200].fill(i16::MAX / 2);
    let regions = strip_silence(
        &cfg(Db::new(-20.0), 10.0),
        Mono::new(&x),
        SampleRate::of(RATE),
    );
    assert_eq!(regions, vec![r(100, 200)]);
}

/// A reset meter reads like a fresh one, and a meter that was never fed
/// has no regions; padding comes from the config the meter was made from.
#[test]
fn a_reset_meter_is_a_fresh_one() {
    let c = cfg(Db::new(-20.0), 10.0)
        .pre_padding(Seconds::new(0.004))
        .post_padding(Seconds::new(0.006));
    assert!(
        Measure::<f32>::measure(&c, SampleRate::of(RATE))
            .unwrap()
            .finish()
            .is_empty()
    );
    let mut meter = c.measure(SampleRate::of(RATE)).unwrap();
    let mut x = vec![0.0f32; 100];
    x[50..60].fill(1.0);
    meter.push(Mono::new(&x), &mut ()).unwrap();
    meter.reset();
    assert!(meter.clone().finish().is_empty());
    meter.push(Mono::new(&x), &mut ()).unwrap();
    assert_eq!(meter.finish(), vec![r(46, 66)]);
}

/// The padding and the minimum silence resolve against the rate the
/// meter is made at: the same config at twice the rate keeps twice the
/// samples.
#[test]
fn durations_resolve_at_the_measuring_rate() {
    let c = cfg(Db::new(-20.0), 10.0).pre_padding(Seconds::new(0.004));
    let mut x = vec![0.0f32; 400];
    x[100..110].fill(1.0);
    let at = |rate: f64| strip_silence(&c, Mono::new(&x), SampleRate::of(rate));
    assert_eq!(at(1000.0), vec![r(96, 110)]);
    assert_eq!(at(2000.0), vec![r(92, 110)]);
}
