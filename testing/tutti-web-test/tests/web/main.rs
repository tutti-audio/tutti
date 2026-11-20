//! What a web session runs with no tutti thread, under node (`cargo xtask test-wasm`): an offline
//! build, a bounce to memory, a WAV in memory, and the work an app runs itself (`analyze`,
//! `load_midi`). Every input is bytes or samples in memory: the web has no file system.
#![cfg(target_arch = "wasm32")]

use std::sync::Arc;
use tutti::codec::{read_wave, write_wave};
use tutti::measure::PeakConfig;
use tutti::{
    Bounce, ChannelLayout, Clip, Format, Interleaved, MaxBlock, Pcm, Range, Root, SampleRate,
    Samples, Seconds, Session, Wave,
};
use wasm_bindgen_test::wasm_bindgen_test;

const RATE: SampleRate = SampleRate::SR_48K;

/// An offline session at [`RATE`].
fn session() -> Session {
    Session::builder()
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples::new(128)))
        .build()
        .expect("an offline session builds")
}

/// A mono wave of the given samples at [`RATE`].
fn wave_of(samples: impl IntoIterator<Item = f32>) -> Wave {
    let mut wave = Wave::new(1, RATE);
    for sample in samples {
        wave.append(Interleaved::new(&[sample], wave.layout()).unwrap())
            .unwrap();
    }
    wave
}

/// A square wave of amplitude 0.5 and a period of four frames, `frames` long.
fn square(frames: usize) -> Wave {
    wave_of((0..frames).map(|i| if i % 4 < 2 { 0.5 } else { -0.5 }))
}

/// **A session builds offline with no thread of tutti's, and holds the rate it was asked for.**
#[wasm_bindgen_test]
fn an_offline_session_builds_at_its_rate() {
    assert_eq!(session().sample_rate(), RATE);
}

/// **A clip of a constant bounces to memory as the range's frames, each at the constant.** One
/// second of a placed clip holds exactly the second's frames, and the clip's level is the
/// master's level on every frame.
#[wasm_bindgen_test]
fn a_bounce_of_a_clip_to_memory_is_the_ranges_frames_at_the_clips_level() {
    let mut session = session();
    let (clip, _) = session
        .graph()
        .add(Clip::new(Arc::new(wave_of(vec![0.25; 2 * 48_000]))).expect("a placeable wave"));
    session.graph().connect(clip, Root).unwrap().unfaded();
    let bounce = Bounce::project()
        .master_buffers()
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(1.0)))
        .sample_rate(RATE);
    let planes = session
        .bounce(bounce)
        .expect("the bounce renders")
        .into_master()
        .into_planes();
    assert_eq!(planes[0].len(), 48_000);
    assert!(
        planes[0].iter().all(|&s| s == 0.25),
        "a constant clip stays constant"
    );
}

/// **A WAV encoded to memory reads back as the wave it came from.** Float32 holds every `f32`
/// exactly, so the round trip is equality.
#[wasm_bindgen_test]
fn a_wav_in_memory_reads_back_equal() {
    let wave = square(1_001);
    let mut bytes = Vec::new();
    write_wave(&wave, &mut bytes, Format::Wav(Pcm::Float32)).expect("encodes");
    assert!(bytes.starts_with(b"RIFF"), "a WAV file starts with RIFF");
    assert_eq!(read_wave::<f32>(bytes).expect("decodes"), wave);
}

/// **An analysis run by the app reads the wave in memory.** A square of amplitude 0.5 peaks at
/// 0.5 and has a root-mean-square of 0.5: the one block over all 4 096 frames.
#[wasm_bindgen_test]
fn an_analysis_of_a_wave_in_memory_reads_its_peak_and_rms() {
    let session = session();
    let peaks = session
        .analyze(square(4_096), PeakConfig::new(Samples::new(4_096)))
        .run()
        .expect("analyzes");
    assert_eq!(peaks.layout(), ChannelLayout::MONO);
    let block = peaks.channel(0).expect("a channel")[0];
    assert_eq!(block.peak().get(), 0.5);
    assert!((block.rms.get() - 0.5).abs() < 1e-12);
}

/// **A MIDI file loaded from bytes holds the notes it was written with.** A format 0 file of two
/// quarter notes: two paired notes, each a note-on and a note-off in the track's clip.
#[wasm_bindgen_test]
fn a_midi_file_loaded_from_bytes_has_its_notes() {
    // A quarter note at the delta before it (480 ticks of a division of 480).
    let note = |key: u8| [0, 0x90, key, 100, 0x83, 0x60, 0x80, key, 64];
    let mut body: Vec<u8> = [note(60), note(64)].concat();
    body.extend([0, 0xFF, 0x2F, 0]);
    let mut bytes = b"MThd\0\0\0\x06\0\0\0\x01\x01\xE0MTrk".to_vec();
    bytes.extend((body.len() as u32).to_be_bytes());
    bytes.extend(body);

    let mut session = session();
    let loaded = session.midi().load_midi(bytes).run().expect("loads");
    assert_eq!(loaded.file().tracks().len(), 1);
    assert_eq!(loaded.file().tracks()[0].notes().len(), 2);
    assert_eq!(loaded.track(0).unwrap().events.len(), 4);
}
