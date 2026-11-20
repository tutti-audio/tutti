//! The session's own values (its tempo, meter and click) written and read
//! with the session's verbs, on analytic figures: the map a write stores, the
//! bar and beat a click accents, and what a refusal leaves.

use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_nodes::MetronomeMode;
use tutti_session::{OutPort, Root, Session, SetRefusal, ValueErrorKind};
use tutti_tempo::TempoMap;
use tutti_units::{Amplitude, Beat, Bpm, SampleRate};

fn offline() -> (Session, tutti_session::Background) {
    Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .build()
        .expect("builds")
}

/// A map with a step and a ramp: not what a constant tempo can say.
fn ramped() -> TempoMap {
    TempoMap::builder(Bpm::of(90.0))
        .ramp_time_to(Beat::new(4.0), Bpm::of(150.0))
        .step_to(Beat::new(8.0), Bpm::of(100.0))
        .build()
        .expect("a map")
}

/// A write of a map is read back whole, and a constant tempo is a one-step map.
#[test]
fn a_tempo_map_written_is_the_map_read_back() {
    let (mut s, _bg) = offline();
    let tempo = s.transport_handle().tempo();
    let map = Arc::new(ramped());
    s.set(tempo.to(Arc::clone(&map))).unwrap();
    assert_eq!(s.get(tempo).unwrap(), map);
    let _ = s.update();
    assert_eq!(s.tempo_map(), map, "the engine's map is the one written");

    s.set(tempo.to(TempoMap::constant(Bpm::of(90.0)).unwrap()))
        .unwrap();
    assert_eq!(s.get(tempo).unwrap().constant_tempo(), Some(Bpm::of(90.0)));
}

/// The click's peak over the 30 ms after each of the first `beats` beats,
/// at full level, 120 BPM (24 000 frames a beat at 48 kHz) from beat 0.
fn peaks(s: &mut Session, beats: usize) -> Vec<f32> {
    s.set((
        s.click().mode().to(MetronomeMode::Always),
        s.click().volume().to(Amplitude::new(1.0)),
    ))
    .unwrap();
    let click = s.click_node().expect("the builder inserts the click");
    for port in 0..2 {
        s.graph()
            .set_sources_unfaded(Root::output(port), [OutPort::new(click, port)])
            .unwrap();
    }
    let _ = s.update();
    s.transport_handle().play().expect("room");
    let frames = beats * 24_000;
    let mut left = Vec::with_capacity(frames);
    let mut block = vec![0.0f32; 1_024];
    while left.len() < frames {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        left.extend(block.chunks(2).map(|frame| frame[0]));
    }
    (0..beats)
        .map(|beat| {
            left[beat * 24_000..beat * 24_000 + 1_440]
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
        })
        .collect()
}

/// An accent is the 1200 Hz click at full level, a beat's the 1000 Hz one at
/// 0.7 of it: a peak past 0.9 is an accent and one under 0.75 is not.
fn accents(peaks: &[f32]) -> Vec<bool> {
    peaks
        .iter()
        .map(|&peak| {
            assert!(peak > 0.0, "every beat sounds");
            assert!(peak > 0.9 || peak < 0.75, "{peak} is neither click");
            peak > 0.9
        })
        .collect()
}

/// In 4/4 the first bar line is beat 0 and the next beat 4, so beats 1 to 3
/// are not accented; a meter of 3/4 written to the transport's meter moves the
/// next downbeat to beat 3.
#[test]
fn the_clicks_accent_follows_the_transports_meter() {
    let (mut four_four_session, _four_four_bg) = offline();
    let four_four = accents(&peaks(&mut four_four_session, 4));
    assert_eq!(four_four, [true, false, false, false]);

    let (mut s, _bg) = offline();
    s.set(
        s.transport_handle()
            .meter()
            .to(MeterMap::new([MeterChange::new(
                Beat::new(0.0),
                TimeSignature::of(3, 4),
            )])),
    )
    .unwrap();
    assert_eq!(accents(&peaks(&mut s, 4)), [true, false, false, true]);
}

/// A level outside `0.0..=1.0` is `OutOfRange` and NaN or infinity is
/// `NotFinite`; each lands nothing, and both ends are taken.
#[test]
fn a_click_volume_outside_the_unit_range_is_refused_and_lands_nothing() {
    let (s, _bg) = offline();
    let volume = s.click().volume();
    s.set(volume.to(Amplitude::new(0.25))).unwrap();
    for (bad, refusal) in [
        (1.01, SetRefusal::OutOfRange),
        (-0.01, SetRefusal::OutOfRange),
        (f64::NAN, SetRefusal::NotFinite),
        (f64::INFINITY, SetRefusal::NotFinite),
    ] {
        let refused = s.set(volume.to(Amplitude::new(bad))).unwrap_err();
        assert_eq!(refused.kind, ValueErrorKind::Refused(refusal), "{bad}");
        assert!(refused.target.expect("it names the target").is(volume));
        assert_eq!(s.get(volume).unwrap(), Amplitude::new(0.25), "{bad}");
    }
    for edge in [0.0, 1.0] {
        s.set(volume.to(Amplitude::new(edge))).unwrap();
        assert_eq!(s.get(volume).unwrap(), Amplitude::new(edge));
    }
}

/// A batch with a refused item lands none of it: the tempo before the bad
/// click level is not written, and the error names the level's position.
#[test]
fn a_batch_with_a_refused_click_volume_lands_nothing() {
    let (s, _bg) = offline();
    let tempo = s.transport_handle().tempo();
    let before = s.get(tempo).unwrap();
    let refused = s
        .set((
            tempo.to(ramped()),
            s.click().volume().to(Amplitude::new(f64::NAN)),
        ))
        .unwrap_err();
    assert_eq!(refused.at, 1);
    assert_eq!(refused.kind, ValueErrorKind::Refused(SetRefusal::NotFinite));
    assert_eq!(s.get(tempo).unwrap(), before);
    assert_eq!(s.tempo_map(), before);
}

/// The click's mode is a setting the audio thread reads: a write is read back
/// at once, each of the four modes.
#[test]
fn the_click_mode_is_read_back_for_each_mode() {
    let (s, _bg) = offline();
    let mode = s.click().mode();
    assert_eq!(s.get(mode).unwrap(), MetronomeMode::Off, "the default");
    for written in [
        MetronomeMode::PrerollOnly,
        MetronomeMode::RecordingOnly,
        MetronomeMode::Always,
        MetronomeMode::Off,
    ] {
        s.set(mode.to(written)).unwrap();
        assert_eq!(s.get(mode).unwrap(), written);
    }
}
