use tutti_buffers::{ChannelLayout, Planes};
use tutti_units::{Amplitude, Hz, Q, SampleRate, Samples};

use super::*;
use crate::SvfBank;
use tutti_fixtures::signal::sine;

const RATE: SampleRate = SampleRate::SR_48K;
const WIDTH: ChannelLayout = ChannelLayout::MONO;
const CUTOFF: Hz = Hz::new(1_000.0);

/// A loud 1 kHz tone of `frames` frames, `CHUNK` at most.
fn loud(frames: usize) -> Vec<f32> {
    sine(
        Hz::new(1_000.0),
        Amplitude::new(1.0),
        RATE,
        Samples::new(frames),
    )
}

/// The levels `path` reads from `key`, a chunk at a time.
fn levels(path: &mut KeyPath<f32>, key: &[f32]) -> Vec<f32> {
    let mut out = Vec::new();
    for chunk in key.chunks(CHUNK) {
        let mut heard = vec![0.0f32; chunk.len()];
        path.levels(&Planes::new(&[chunk]).unwrap(), 0..chunk.len(), &mut heard);
        out.extend(heard);
    }
    out
}

/// A path on `filter` at [`CUTOFF`] and Butterworth.
fn on(filter: KeyFilter) -> KeyPath<f32> {
    let mut path = KeyPath::new(WIDTH, RATE);
    path.retarget(filter, CUTOFF, Q::BUTTERWORTH);
    path
}

/// While off the stage is not run: the level is the key's own magnitude and the integrators
/// stay at rest, however loud the key. The same key through the stage while on moves them.
#[test]
fn an_off_path_does_not_run_its_stage() {
    let key = loud(4_800);
    let mut off = KeyPath::<f32>::new(WIDTH, RATE);
    off.retarget(KeyFilter::Off, CUTOFF, Q::BUTTERWORTH);
    let heard = levels(&mut off, &key);
    assert!(heard.iter().zip(&key).all(|(h, k)| *h == k.abs()));
    assert_eq!(
        off.stage.bank,
        SvfBank::new(WIDTH),
        "the integrators are at rest"
    );

    let mut running = on(KeyFilter::HighPass);
    levels(&mut running, &key);
    assert_ne!(
        running.stage.bank,
        SvfBank::new(WIDTH),
        "the stage moves when run"
    );
}

/// A switch in from off starts the stage from silence: after a loud on-period, an off-period of
/// a loud key and a switch in, a silent key reads exactly 0, where a stage that stayed on
/// through the same loud key still rings on silence. The cutoff and Q are at their design at
/// once: the coefficients do not ramp from the earlier on-period's.
#[test]
fn a_switch_in_starts_the_stage_from_silence_on_the_cutoff_at_once() {
    let (key, silence) = (loud(4_800), [0.0f32; CHUNK]);

    let mut path = on(KeyFilter::HighPass);
    levels(&mut path, &key);
    path.retarget(KeyFilter::Off, CUTOFF, Q::BUTTERWORTH);
    levels(&mut path, &key);
    path.retarget(KeyFilter::HighPass, Hz::new(2_000.0), Q::BUTTERWORTH);
    assert_eq!(
        path.stage.coefficients.now(),
        path.stage.coefficients.target()
    );
    assert!(levels(&mut path, &silence).iter().all(|&l| l == 0.0));

    let mut kept = on(KeyFilter::HighPass);
    levels(&mut kept, &key);
    let rings = levels(&mut kept, &silence);
    assert!(
        rings.iter().any(|&l| l > 1e-3),
        "a stage left on rings: {rings:?}"
    );
}

/// A switch between the two responses keeps the integrators, which both read, and ramps the
/// coefficients to the other response over the grid period.
#[test]
fn a_switch_between_responses_keeps_the_state_and_ramps_the_coefficients() {
    let mut path = on(KeyFilter::HighPass);
    levels(&mut path, &loud(4_800));
    let state = path.stage.bank.clone();
    let from = path.stage.coefficients.now();
    path.retarget(KeyFilter::BandPass, CUTOFF, Q::BUTTERWORTH);
    assert_eq!(path.stage.bank, state);
    assert_eq!(path.stage.coefficients.now(), from);
    assert_ne!(path.stage.coefficients.target(), from);
}
