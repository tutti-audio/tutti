//! The leveller node: a stereo 1 kHz sine settles at the target (EBU Tech 3341 case 1 reads
//! a stereo sine at -23 dBFS peak as -23.0 LUFS, so its peak in dBFS is its loudness to within
//! 0.1), the target is the `Threshold` param at index 0, a 5.1's LFE adds no loudness (BS.1770)
//! and a bus with no placement is refused.

use tutti_buffers::{
    AmbisonicFormat, AmbisonicOrder, Bed, BusTopology, ChannelLayout, ChannelTopology, Planes,
};
use tutti_fixtures::signal::{peak, sine};
use tutti_graph::ParamNode;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::{ChannelWeightError, LevellerConfig, LevellerNode};
use tutti_units::{Amplitude, Db, Hz, SampleRate, Samples, UnitAddr, UnitParam};

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 480;

/// A 1 kHz sine of `db` peak, `seconds` long.
fn tone(db: f64, seconds: usize) -> Vec<f32> {
    sine(
        Hz::new(1_000.0),
        Db::new(db).to_amplitude(),
        SR,
        Samples::new(seconds * 48_000),
    )
}

/// `channels` through `node` in blocks of `BLOCK` frames.
fn run(node: &mut LevellerNode<f32>, channels: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(); channels.len()];
    for at in (0..channels[0].len()).step_by(BLOCK) {
        let block: Vec<&[f32]> = channels.iter().map(|c| &c[at..at + BLOCK]).collect();
        let y = drive(node, SR, Planes::new(&block).unwrap(), &[]);
        for (o, y) in out.iter_mut().zip(y) {
            o.extend(y);
        }
    }
    out
}

/// The peak of the last second of `channel`, in dBFS.
fn tail_peak_dbfs(channel: &[f32]) -> f64 {
    Amplitude::new(f64::from(peak(&channel[channel.len() - 48_000..])))
        .to_db()
        .get()
}

fn stereo(config: LevellerConfig) -> LevellerNode<f32> {
    prepared(LevellerNode::try_from(config).unwrap(), SR, BLOCK)
}

#[test]
fn a_stereo_sine_settles_at_the_target_loudness() {
    let mut n = stereo(LevellerConfig::new(BusTopology::STEREO));
    let x = tone(-29.0, 30);
    let y = run(&mut n, &[x.clone(), x]);
    let peak = tail_peak_dbfs(&y[0]);
    assert!((peak + 23.0).abs() < 0.1, "{peak} dBFS");
    assert_eq!(tail_peak_dbfs(&y[0]), tail_peak_dbfs(&y[1]));
}

/// The target is the `Threshold` param at index 0: moved to -17 LUFS (a +6 dB gain over the
/// -23 default's, inside the 12 dB boost), the output settles at -17 dBFS.
#[test]
fn the_threshold_param_at_index_0_is_the_target() {
    let mut n = stereo(LevellerConfig::new(BusTopology::STEREO).max_boost(Db::new(20.0)));
    assert!(
        n.param_set()
            .set(UnitAddr::new(UnitParam::Threshold, 0), -17.0)
    );
    let x = tone(-29.0, 30);
    let y = run(&mut n, &[x.clone(), x]);
    let peak = tail_peak_dbfs(&y[0]);
    assert!((peak + 17.0).abs() < 0.1, "{peak} dBFS");
}

/// The configured boost is the limit: -40 dBFS in, 6 dB of boost at most, -34 dBFS out.
#[test]
fn the_configured_boost_limits_the_gain() {
    let mut n = stereo(LevellerConfig::new(BusTopology::STEREO).max_boost(Db::new(6.0)));
    let x = tone(-40.0, 30);
    let y = run(&mut n, &[x.clone(), x]);
    let peak = tail_peak_dbfs(&y[0]);
    assert!((peak + 34.0).abs() < 0.1, "{peak} dBFS");
}

/// BS.1770 gives the LFE no weight: a 5.1 with a -29 dBFS sine on its centre settles at 9 dB
/// of gain (-23 LUFS target, -32 LUFS read) whatever sits on the LFE; a -3 dBFS sine there
/// would otherwise cut the gain to its limit.
#[test]
fn a_surround_bus_lfe_adds_no_loudness() {
    let mut n = prepared(
        LevellerNode::<f32>::try_from(LevellerConfig::new(Bed::Surround51.into())).unwrap(),
        SR,
        BLOCK,
    );
    let centre = tone(-29.0, 30);
    let silence = vec![0.0f32; centre.len()];
    let mut planes = vec![silence.clone(); 6];
    // FL FR FC LFE SL SR
    planes[2] = centre;
    planes[3] = tone(-3.0, 30);
    let y = run(&mut n, &planes);
    // One centre channel at unity weight reads 3.01 dB under a stereo pair of the same level,
    // -32 LUFS, so the gain is 9 dB and its peak -20 dBFS.
    let peak = tail_peak_dbfs(&y[2]);
    assert!((peak + 20.0).abs() < 0.1, "{peak} dBFS");
}

#[test]
fn a_bus_with_no_placement_is_refused() {
    let unplaced = BusTopology::unassigned(ChannelLayout::from(3u16)).unwrap();
    assert_eq!(
        LevellerNode::<f32>::try_from(LevellerConfig::new(unplaced)).unwrap_err(),
        ChannelWeightError::Unplaced {
            width: ChannelLayout::from(3u16)
        }
    );
    let ambisonic = ChannelTopology::Ambisonic(AmbisonicFormat::ambix(AmbisonicOrder::of(1)));
    assert_eq!(
        LevellerNode::<f32>::try_from(LevellerConfig::new(ambisonic)).unwrap_err(),
        ChannelWeightError::Ambisonic
    );
}
