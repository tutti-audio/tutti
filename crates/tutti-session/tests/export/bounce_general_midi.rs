//! A bounce of the General MIDI setup (`Session::add_general_midi`) that ends on a note-off:
//! its length is the tail the nodes report, not the ceiling the reverb declares.

use crate::bounced::bounced;
use std::sync::Arc;
use tutti_buffers::ChannelLayout;
use tutti_dsp::FreeverbBank;
use tutti_export::Produced;
use tutti_fixtures::midi;
use tutti_fixtures::sf2::{Inst, Preset, bytes, generator};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_nodes::FreeverbControls;
use tutti_session::{
    Bounce, BounceLatency, BounceTail, ExportClock, LIVE_MAX_BLOCK, MainOutput, OutputPort, Range,
    Session, Stem,
};
use tutti_sf2::SoundFont;
use tutti_soundfont::{SoundFontConfig, SoundFontUnit};
use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Voice2};
use tutti_units::{Beat, Bpm, SampleRate, Seconds, Tail};

const RATE: f64 = 48_000.0;
/// The unit's output that feeds the reverb (dry pair, chorus pair, then the reverb send).
const REVERB_SEND: u16 = 4;

/// **A General MIDI bounce that ends on a note-off ends at the room's tail after the reverb
/// stops being fed, far short of the reverb's declared ceiling (about 13 s).** One note from
/// beat 0 to beat 1 (90 BPM: 32,000 frames) on a font whose release is 0.25 s; the programme is
/// 0.7 s (33,600 frames), so the note-off is in it and the release runs into the tail.
///
/// The reverb's input is the unit's reverb send (output 4), tapped as a stem. Its last non-zero
/// frame `s` is past the programme's end (the voice's send is cut once its level is under the
/// floor, before the dry pair's). The export asks the graph after every block of the tail, in
/// blocks of [`LIVE_MAX_BLOCK`] from the programme's end, and the reverb counts whole blocks of
/// silent input: it is fed through the block that holds `s`, then owes `FreeverbBank::tail_of`
/// for the General MIDI room. So the bounce is the programme, the blocks up to and including the
/// one holding `s`, and that room tail.
#[test]
fn a_general_midi_bounce_ends_at_the_rooms_tail_after_the_reverb_is_last_fed() {
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
    let (main, mut stems) = bounced(
        &mut session,
        Bounce::new()
            .main_output_buffers()
            .channels(ChannelLayout::STEREO)
            .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.7)))
            .sample_rate(SampleRate::of(RATE))
            .clock(ExportClock::timeline(timeline))
            .latency(BounceLatency::Keep)
            .tail(BounceTail::default())
            .stem_with(Stem::new(gm.instrument().node().erase()).channels([REVERB_SEND])),
    )
    .expect("the bounce renders")
    .into_parts();
    let length = main.into_planes()[0].len();
    let Some(Produced::Buffers(send)) = stems.pop() else {
        panic!("the send stem is held in memory")
    };
    let send = send.into_planes();
    let fed_until = send[0]
        .iter()
        .rposition(|&x| x != 0.0)
        .expect("the reverb is fed");
    assert!(fed_until >= programme, "the send outlives the programme");
    let block = LIVE_MAX_BLOCK.samples().get();
    let blocks_fed = (fed_until - programme) / block + 1;
    assert_eq!(length, programme + blocks_fed * block + room);
}
