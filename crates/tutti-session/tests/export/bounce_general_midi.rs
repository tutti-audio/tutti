//! A bounce of the General MIDI setup (`Session::add_general_midi`) that ends on a note-off:
//! its length is the tail the nodes report, not the ceiling the reverb declares.

use crate::bounced::bounced;
use std::sync::Arc;
use tutti_buffers::ChannelLayout;
use tutti_dsp::FreeverbBank;
use tutti_fixtures::midi;
use tutti_fixtures::sf2::{Inst, Preset, bytes, generator};
use tutti_graph::NodeKey;
use tutti_midi_runtime::TimedMidiEvent;
use tutti_nodes::FreeverbControls;
use tutti_session::{
    Bounce, BounceLatency, BounceTail, ExportClock, MainOutput, OutputPort, Range, Session,
};
use tutti_sf2::SoundFont;
use tutti_soundfont::{SoundFontConfig, SoundFontUnit};
use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Voice2};
use tutti_units::{Beat, Bpm, SampleRate, Seconds, Tail};

const RATE: f64 = 48_000.0;
/// The export's block (the executor's maximum block): the tail is asked again once a block.
const BLOCK: usize = 1_024;

/// **A General MIDI bounce that ends on a note-off ends at the reverb's room tail after the
/// instrument's release, far short of the reverb's declared ceiling.** One note from beat 0
/// to beat 1 (90 BPM: 32,000 frames), the programme 0.7 s (33,600 frames) so the release
/// (0.25 s, the font's `releaseVolEnv` of -2400 timecents) runs 1,600 frames into the tail.
/// The reverb is fed while the voice sounds, then owes the room's `tail_of`: the bounce is
/// at least the programme and that figure, and at most the programme, the instrument's declared
/// release, that figure and two blocks (the asks come once a block); and under half the
/// ceiling the reverb declares, which a bounce that did not narrow it would spend.
#[test]
fn a_general_midi_bounce_ends_at_the_rooms_tail_after_the_release() {
    let font = Arc::new(
        SoundFont::open(bytes(
            &[Preset {
                name: "p",
                bank: 0,
                program: 0,
                zones: vec![(vec![], 0)],
            }],
            &[Inst {
                name: "i",
                zones: vec![vec![(generator::RELEASE_VOL_ENV, -2400)]],
            }],
        ))
        .expect("the built font parses"),
    );
    let unit = SoundFontUnit::try_from(SoundFontConfig::new(font)).expect("builds");
    let mut session = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    session
        .graph()
        .set_sample_rate(SampleRate::of(RATE))
        .unwrap();
    let gm = session.add_general_midi(unit).expect("a SoundFont unit");
    for c in 0..2 {
        session
            .graph()
            .set_sources(MainOutput::channel(c), [OutputPort::new(gm.output(), c)])
            .unwrap();
    }
    let key = NoteNumber::of(69);
    let clip = vec![
        TimedMidiEvent::new(
            Beat::new(0.0),
            midi::note_on(MidiChannel::FIRST, 69, 0xC800),
        )
        .unwrap(),
        TimedMidiEvent::new(
            Beat::new(1.0),
            Message::midi2(MidiChannel::FIRST, Voice2::note_off(key, NoteVelocity::MIN)),
        )
        .unwrap(),
    ];
    session
        .midi()
        .play_clip(gm.instrument().node(), Beat::new(0.0), clip)
        .expect("the unit takes a clip");
    let _ = session.update();

    let graph = session.graph_ref();
    let declared = |node: NodeKey| match graph.node_tail(node).unwrap() {
        Tail::Finite(n) => n.get(),
        other => panic!("{node:?}: {other:?}"),
    };
    let release = declared(gm.instrument().node().erase());
    let ceiling = declared(gm.reverb().erase());
    assert!(
        (11_000..13_000).contains(&release),
        "about 0.25 s: {release}"
    );

    let Tail::Finite(room) =
        FreeverbBank::<f32>::new(SampleRate::of(RATE)).tail_of(&FreeverbControls::GENERAL_MIDI)
    else {
        panic!("a finite room tail")
    };
    let room = room.get();

    let programme = (0.7 * RATE) as usize;
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat::new(0.0),
        tempo_map: Arc::new(tutti_tempo::TempoMap::constant(Bpm::of(90.0)).expect("a tempo")),
        sample_rate: SampleRate::of(RATE),
        loop_range: None,
    }));
    let planes = bounced(
        &mut session,
        Bounce::new()
            .main_output_buffers()
            .channels(ChannelLayout::STEREO)
            .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.7)))
            .sample_rate(SampleRate::of(RATE))
            .clock(ExportClock::timeline(timeline))
            .latency(BounceLatency::Keep)
            .tail(BounceTail::default()),
    )
    .expect("the bounce renders")
    .into_main_output()
    .into_planes();
    let length = planes[0].len();
    assert!(
        length >= programme + room,
        "{length} frames, the programme {programme} and the room's {room}"
    );
    assert!(
        length <= programme + release + room + 2 * BLOCK,
        "{length} frames, the programme {programme}, the release {release}, the room's {room}"
    );
    assert!(
        length < programme + ceiling / 2,
        "{length} frames, the reverb declares {ceiling}"
    );
}
