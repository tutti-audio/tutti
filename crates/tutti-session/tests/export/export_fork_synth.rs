//! Built-in synths (`PolySynth` and `SoundFontUnit`) inserted as graph nodes
//! and exported through a fork, playing the clip `Session::midi().play_clip`
//! wires to them: a clip node, which the export forks with the synth and
//! which reads the render's `Env`.
//!
//! Every tempo here is 90 BPM: a beat is 32 000 frames at 48 kHz (64 000 at
//! 96 kHz), placed to the frame since clips place on integer frames.

use crate::bounced::bounced;
use std::sync::Arc;
use tutti_buffers::InterleavedMut;
use tutti_session::MainOutput;
use tutti_session::ToMemory;

use tutti_buffers::ChannelLayout;
use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::contract::Direct;
use tutti_graph::{Event, Node, Offset};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_polysynth::{Adsr, OscillatorType, PolySynth, SynthConfig};
use tutti_session::{
    Bounce, BounceLatency, BounceTail, DetachedSession, ExportClock, Range, Session, Stem, StemKey,
};
use tutti_sf2::SoundFont;
use tutti_soundfont::{SoundFontConfig, SoundFontUnit};
use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, U7, Ump, Voice2};
use tutti_units::{Amplitude, Beat, Bpm, SampleRate, Samples, Seconds};

const RATE: f64 = 48_000.0;
/// A beat at 90 BPM and 48 kHz, in frames.
const BEAT_48K: usize = 32_000;
/// Frames of the note compared against a reference render.
const NOTE: usize = 4_096;
/// A preset of the test soundfont that is not the default piano, so a
/// fork that lost the live unit's preset is heard.
const PRESET: U7 = U7::of(24);

/// A bounce of the mix into memory for `seconds` at `rate`: stereo, trimming
/// nothing and keeping no tail, on `clock`.
fn bounce_of(seconds: f64, rate: f64, clock: ExportClock) -> Bounce<NodeKey, ToMemory> {
    Bounce::new()
        .main_output(
            ToMemory::new().channels(tutti_buffers::ChannelSet::default_for(
                ChannelLayout::STEREO,
            )),
        )
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(seconds)))
        .sample_rate(SampleRate::of(rate))
        .clock(clock)
        .latency(BounceLatency::Keep)
        .tail(BounceTail::None)
}

fn render(session: &mut DetachedSession, bounce: Bounce<NodeKey, ToMemory>) -> Vec<Vec<f32>> {
    bounced(session, bounce)
        .expect("the bounce renders")
        .into_main_output()
        .into_planes()
}

/// The planes of the one stem `bounce` names, in memory: the main output is
/// rendered beside it and dropped.
fn render_stem(session: &mut DetachedSession, bounce: Bounce<NodeKey, ToMemory>) -> Vec<Vec<f32>> {
    let (_, mut done, keys) = bounced(session, bounce)
        .expect("the bounce renders")
        .into_parts();
    match keys.as_slice() {
        [StemKey::Buffers(key)] => done.take_buffers(key).expect("its own key").into_planes(),
        other => panic!("expected one stem held in memory, got {other:?}"),
    }
}

/// A saw with an instant attack, built at the live graph's rate.
fn poly() -> PolySynth<f32> {
    PolySynth::<f32>::new(
        SynthConfig::new()
            .oscillator(OscillatorType::Saw)
            .envelope(Adsr {
                attack: Seconds::new(0.0),
                ..Default::default()
            }),
    )
}

/// The repo's committed test soundfont.
fn font() -> Arc<SoundFont> {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    Arc::new(SoundFont::open(path.as_path()).unwrap_or_else(|e| {
        panic!(
            "committed test soundfont at {} does not load: {e}",
            path.display()
        )
    }))
}

/// A SoundFont unit on [`PRESET`].
fn sf2(font: &Arc<SoundFont>) -> SoundFontUnit<f32> {
    let mut unit =
        SoundFontUnit::try_from(SoundFontConfig::new(Arc::clone(font))).expect("the unit builds");
    unit.program_change(MidiChannel::FIRST, PRESET);
    unit
}

/// A detached stereo session at the export's rate with `unit` inserted as a
/// graph node (the path a host takes), routed to the outputs: a unit with no
/// params of its own (`SoundFontUnit`) or one a param write reaches
/// (`PolySynth`).
fn session_with<N: tutti_graph::IntoNode<f32, Controls: Send + 'static> + Send + 'static>(
    unit: N,
) -> (DetachedSession, NodeKey) {
    let mut session = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    session.set_sample_rate(SampleRate::of(RATE)).unwrap();
    let synth = session.graph().add(unit);
    // The dry pair; the unit's other outputs are sends.
    for c in 0..2 {
        session
            .graph()
            .connect(synth.output(c), MainOutput::channel(c))
            .unwrap();
    }
    let _ = session.update();
    (session, synth.erase())
}

/// Middle C from beat 1 to beat 2.
fn clip() -> Vec<TimedMidiEvent> {
    vec![
        TimedMidiEvent::new(Beat::new(1.0), clip_note_on()).unwrap(),
        TimedMidiEvent::new(
            Beat::new(2.0),
            Message::midi2(
                MidiChannel::FIRST,
                Voice2::note_off(NoteNumber::of(60), NoteVelocity::MIN),
            ),
        )
        .unwrap(),
    ]
}

fn clip_note_on() -> Ump {
    midi::note_on(MidiChannel::FIRST, 60, 0xFFFF)
}

fn install_clip(session: &mut DetachedSession, synth: NodeKey) {
    session
        .midi()
        .play_clip(synth, Beat::new(0.0), clip())
        .expect("the synth takes a clip");
    let _ = session.update();
}

/// A 90 BPM timeline from beat 0 at `rate`.
fn timeline(rate: f64) -> Arc<OfflineTimeline> {
    Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat::new(0.0),
        tempo_map: Arc::new(tutti_tempo::TempoMap::constant(Bpm::of(90.0)).expect("a tempo")),
        sample_rate: SampleRate::of(rate),
        loop_range: None,
    }))
}

/// Export the mix for 0.8 s at `rate` against [`timeline`].
fn request(rate: f64) -> Bounce<NodeKey, ToMemory> {
    bounce_of(0.8, rate, ExportClock::timeline(timeline(rate)))
}

/// `unit` prepared at `rate` and fed a note-on at frame 0 on its event
/// input, rendered in 64-frame blocks by hand (`contract::Direct`): what
/// the note sounds like from its first frame, at `rate`.
fn reference<N: Node>(unit: N, rate: f64) -> Vec<f32> {
    let mut unit = Direct::new(unit, SampleRate::of(rate), 64);
    let unit_in = unit
        .node
        .shape()
        .event_input(0)
        .expect("the node declares an event input");
    let at = Offset::of(Samples::new(0), Samples::new(64));
    unit.events(unit_in, &[Event::midi(at, clip_note_on())]);
    let mut out = Vec::with_capacity(NOTE);
    while out.len() < NOTE {
        unit.block();
        out.extend_from_slice(unit.output().plane(0).samples());
    }
    out
}

/// Assert the render is silent until beat 1 and then, sample for sample,
/// `reference` — the note at the render's rate, from its first frame.
fn assert_note_at_beat_1(what: &str, rate: f64, planes: &[Vec<f32>], reference: &[f32]) {
    let beat = BEAT_48K * (rate / RATE) as usize;
    let lead = reference
        .iter()
        .position(|&s| s != 0.0)
        .unwrap_or_else(|| panic!("{what}: the reference note is silent"));
    let left = &planes[0];
    assert_eq!(
        left.iter().position(|&s| s != 0.0),
        Some(beat + lead),
        "{what} at {rate} Hz: the note enters on beat 1"
    );
    assert_eq!(
        &left[beat..beat + NOTE],
        reference,
        "{what} at {rate} Hz: from beat 1, the note at the render's rate"
    );
}

/// The live synth after an export plays its own clip on the **live**
/// transport: rolled and seated on beat 1, the live graph sounds the
/// note. A fork that shared the live clip or synth would have left them
/// where the render did, and the live graph would stay silent.
fn assert_live_untouched(session: &mut DetachedSession) {
    let mut heard = false;
    // Rolling at 120 BPM from beat 1.
    for i in 0..4_096 {
        let t = tutti_graph::TransportPosition::new(
            true,
            Bpm::of(120.0),
            Beat::new(1.0 + f64::from(i) / (RATE / 2.0)),
            None,
        );
        let mut frame = [0.0f32; 2];
        session.render_frame_at(&t, InterleavedMut::one_frame(&mut frame).unwrap());
        heard |= frame[0] != 0.0;
    }
    assert!(heard, "the live synth plays its clip on the live transport");
}

/// **An exported `PolySynth` plays its clip**, on the render's timeline
/// and at the render's rate: a main output export at 48 and 96 kHz is silent
/// until beat 1 and then, sample for sample, what the synth renders for
/// the note re-rated to the render's rate. The live synth keeps its clip.
#[test]
fn an_exported_polysynth_plays_its_clip() {
    for rate in [RATE, 2.0 * RATE] {
        let (mut session, synth) = session_with(poly());
        install_clip(&mut session, synth);
        let planes = render(&mut session, request(rate));
        // Prepared at the render's rate, as the fork is.
        let reference = reference(poly(), rate);
        assert_note_at_beat_1("PolySynth", rate, &planes, &reference);
        assert_live_untouched(&mut session);
    }
}

/// **A param written on a synth reaches it and its fork.** The synth is a
/// `ParamNode` added as a graph node, so a write of `UnitParam::Volume` lands
/// in its master-volume cell, which the live synth reads on its next block
/// and an export's fork starts from: the exported note at a quarter volume
/// peaks at a quarter of the note at unity. (The write is
/// `Session::set`.)
#[test]
fn a_param_on_a_synth_reaches_it_and_its_fork() {
    let peak = |volume: Option<f64>| {
        let mut session = Session::builder()
            .outputs_floor(ChannelLayout::STEREO)
            .build_detached(SampleRate::DEFAULT)
            .expect("a detached session builds");
        session.set_sample_rate(SampleRate::of(RATE)).unwrap();
        let synth = session.graph().add(poly());
        session.graph().connect(synth, MainOutput).unwrap();
        let _ = session.update();
        if let Some(v) = volume {
            session.set(synth.volume().to(Amplitude::new(v))).unwrap();
            let _ = session.update();
            assert_eq!(
                session.get(synth.volume()).unwrap(),
                Amplitude::new(v),
                "the write reaches the live synth's volume cell"
            );
        }
        install_clip(&mut session, synth.erase());
        let planes = render(&mut session, request(RATE));
        planes[0].iter().fold(0.0f32, |a, s| a.max(s.abs()))
    };
    let full = peak(None);
    assert!(full > 0.0, "the note sounds in the export");
    let quarter = peak(Some(0.25));
    assert!(
        (quarter - 0.25 * full).abs() < 1e-5,
        "the fork renders the volume that was set: {quarter} vs ¼ × {full}"
    );
}

/// **An exported `SoundFontUnit` plays its clip**, on the live unit's
/// preset, over the same decoded SoundFont, at the render's rate — a
/// 48 kHz unit exported at 96 kHz renders what a unit built at 96 kHz
/// does, not the 48 kHz note an octave down at half its frame. The live
/// unit keeps its clip.
#[test]
fn an_exported_soundfont_plays_its_clip() {
    let font = font();
    for rate in [RATE, 2.0 * RATE] {
        let (mut session, synth) = session_with(sf2(&font));
        install_clip(&mut session, synth);
        let planes = render(&mut session, request(rate));
        let reference = reference(sf2(&font), rate);
        assert_note_at_beat_1("SoundFontUnit", rate, &planes, &reference);
        assert_live_untouched(&mut session);
    }
}

/// **A node export of a synth plays its clip**: silent until beat 1,
/// then the synth's own note ([`reference`]) sample for sample. The fork
/// carries the clip itself.
#[test]
fn a_synth_node_export_plays_its_clip() {
    let (mut session, synth) = session_with(poly());
    install_clip(&mut session, synth);
    let exported = render_stem(&mut session, request(RATE).stem_with(Stem::new(synth)));
    let reference = reference(poly(), RATE);
    assert_note_at_beat_1("PolySynth node export", RATE, &exported, &reference);
}
