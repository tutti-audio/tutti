//! A stream the app reads (`open_stream(..).await`) is applied to the session:
//! two opens are two streams, a missing file fails its open with the typed error,
//! another session's read is refused as stale, and a clip over a `load_audio`
//! handle plays the file's own samples from either tier.

use crate::offline::offline_48k;
use std::path::Path;
use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_sampler::{OpenError, StreamPhase, StreamStart};
use tutti_session::{AudioData, LoadError, MainOutput, OpenStreamError, OutputPort, Session, Tier};
use tutti_units::{Beat, SampleRate};

const RATE: f64 = 48_000.0;
/// File frames.
const LEN: usize = 4_800;
const BLOCK: usize = 512;

/// Frame `i` of the ramp file: distinct per frame, exact in `f32`.
fn value(i: usize) -> f32 {
    (i as f32 + 1.0) * 1e-5
}

fn write_ramp(path: &Path) {
    let samples: Vec<f32> = (0..LEN).map(value).collect();
    tutti_fixtures::signal::write_wav_f32(path, SampleRate::of(RATE), 1, &samples);
}

/// **Two opens are two streams.** Each read file applies to a stream of its own,
/// whose control side and reader name one stream, and the two differ.
#[test]
fn two_opens_apply_to_two_streams() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = offline_48k();
    let first = s.open_stream(&path, StreamStart::HEAD).expect("a streamer");
    let second = s.open_stream(&path, StreamStart::HEAD).expect("a streamer");
    let (first, second) = (
        tutti_fixtures::block_on(first).expect("reads"),
        tutti_fixtures::block_on(second).expect("reads"),
    );
    let (stream_a, reader_a) = s.apply(first).expect("applies");
    let (stream_b, reader_b) = s.apply(second).expect("applies");
    assert_eq!(stream_a.id(), reader_a.id(), "the two halves of one stream");
    assert_eq!(stream_b.id(), reader_b.id(), "the two halves of one stream");
    assert_ne!(stream_a.id(), stream_b.id());
}

/// **A missing file fails its run with the typed error**
/// (`OpenError::Io(NotFound)`), not a stream that never starts.
#[test]
fn a_missing_file_fails_its_open_with_the_typed_error() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let s = offline_48k();
    let opened = tutti_fixtures::block_on(
        s.open_stream(dir.path().join("absent.wav"), StreamStart::HEAD)
            .expect("a streamer"),
    );
    assert!(
        matches!(
            &opened,
            Err(LoadError::Stream(error))
                if **error == OpenError::Io(std::io::ErrorKind::NotFound)
        ),
        "the typed error names the missing file"
    );
}

/// **An open of a dropped session is `ShutDown`**: the file did not fail.
#[test]
fn an_open_of_a_dropped_session_is_shut_down() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let open = {
        let s = offline_48k();
        s.open_stream(&path, StreamStart::HEAD).expect("a streamer")
    };
    assert!(matches!(
        tutti_fixtures::block_on(open),
        Err(LoadError::ShutDown)
    ));
}

/// **An open and its future are `Send`.**
const fn send<T: Send>() {}
const _: () = {
    send::<tutti_session::OpenStream<f32>>();
    send::<tutti_session::OpeningStream<f32>>();
};

/// **A file read for one session is refused by another** as `Stale`, and the
/// refusal serves nothing there.
#[test]
fn a_file_read_for_another_session_is_stale() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let (a, mut b) = (offline_48k(), offline_48k());
    let opened =
        tutti_fixtures::block_on(a.open_stream(&path, StreamStart::HEAD).expect("a streamer"))
            .expect("reads");
    assert!(matches!(
        b.apply(opened).err(),
        Some(OpenStreamError::Stale)
    ));
}

/// The clip over the file's `load_audio` handle, loaded in the tier `streamed`
/// names: the wave of a resident load, the stream a streamed load's path opens.
/// The session is returned with the clip: it owns the streamer that feeds a
/// streamed clip's ring, which ends with it.
fn clip_of(path: &Path, streamed: bool) -> (Session, tutti_sampler::Clip<f32>) {
    let mut s = offline_48k();
    let audio = tutti_fixtures::block_on(s.load_audio(path).tier(if streamed {
        Tier::Streamed
    } else {
        Tier::Resident
    }))
    .expect("loads");
    let clip = match audio.data() {
        AudioData::Resident(wave) => {
            tutti_sampler::Clip::new(Arc::clone(wave)).expect("a placeable wave")
        }
        AudioData::Streamed(_) => {
            let opened = tutti_fixtures::block_on(
                s.open_stream(audio.asset().clone(), StreamStart::HEAD)
                    .expect("a streamer"),
            )
            .expect("the file reads");
            let (stream, reader) = s.apply(opened).expect("the session serves it");
            tutti_sampler::Clip::streaming(reader, stream)
        }
        other => panic!("unexpected load {other:?}"),
    };
    (s, clip)
}

/// Renders `clip` at beat 0 on a rolling session for `frames` frames,
/// waiting first for a streamed clip's ring to prime.
fn play(clip: tutti_sampler::Clip<f32>, frames: usize) -> Vec<f32> {
    play_in(&mut offline_48k(), clip, frames)
}

/// [`play`] in `s`.
fn play_in(s: &mut Session, clip: tutti_sampler::Clip<f32>, frames: usize) -> Vec<f32> {
    let key = s.graph().add(clip);
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(key, 0)])
        .expect("the clip's channel into the main output's");
    let _ = s.update();
    // A streamed clip's ring is filled on the streamer's thread, and an
    // offline render does not wait for it: wait for the stream to say it is
    // primed.
    tutti_fixtures::wait_until(
        "the stream to be primed",
        tutti_fixtures::HANG_GUARD,
        || {
            s.graph_ref()
                .handle(&key)
                .and_then(|h| h.status())
                .is_none_or(|status| status.phase != StreamPhase::Opening)
        },
    );
    s.play().expect("room");
    let mut out = Vec::new();
    let mut block = vec![0.0f32; BLOCK * 2];
    while out.len() < frames {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        out.extend(block.chunks(2).map(|frame| frame[0]));
    }
    out.truncate(frames);
    out
}

/// **A clip of either tier plays the file's own samples.** The same ramp file
/// declared resident and streamed: each is checked against the ramp the test
/// wrote (frame `i` is `value(i)`), neither against the other.
#[test]
fn a_clip_of_either_tier_plays_the_files_samples() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    for residency in [false, true] {
        let (_origin, clip) = clip_of(&path, residency);
        assert_eq!(clip.is_streamed(), residency);
        let heard = play(clip.at(Beat::new(0.0)), 3_000);
        // The transport starts on the render's next block, so the file begins
        // a block or so in, under the engine's start fade: over
        // `n = declick_frames(rate)` frames, frame `k` scales the output by the
        // smootherstep at `x = (k + 1) / (n + 1)`. A clip at beat 0 plays the
        // file's frame 0 on the fade's frame 0, and no fade gain and no file
        // frame is zero, so the first sound is the file's frame 0 times
        // `s(1 / (n + 1))`. Past the fade the output is the file's frames, one
        // per frame.
        let n = tutti_graph::declick_frames(SampleRate::of(RATE)).get();
        let gain = |k: usize| {
            let x = (k + 1) as f64 / (n + 1) as f64;
            x * x * x * (6.0 * x * x - 15.0 * x + 10.0)
        };
        // The file's frame 0 is output frame `first`, the first sound.
        let first = heard
            .iter()
            .position(|&x| x != 0.0)
            .unwrap_or_else(|| panic!("{residency}: silent"));
        let want = f64::from(value(0)) * gain(0);
        assert!(
            (f64::from(heard[first]) - want).abs() <= want * 1e-3,
            "{residency}: the first sound {} is not the file's frame 0 under the fade's frame 0, {want}",
            heard[first]
        );
        assert!(first <= 2 * BLOCK, "{residency}: began at frame {first}");
        for k in 0..n {
            let want = f64::from(value(k)) * gain(k);
            assert!(
                (f64::from(heard[first + k]) - want).abs() <= 1e-8,
                "{residency}: fade frame {k}: {}, want {want}",
                heard[first + k]
            );
        }
        for k in 1_100..2_000 {
            assert_eq!(
                heard[first + k],
                value(k),
                "{residency}: file frame {k} at output frame {}",
                first + k
            );
        }
    }
}

/// **A stream plays on at its pitch after a re-rate**: a 48 kHz ramp file
/// streams in a session at 48 kHz, which then re-rates to 44.1 kHz. Each output
/// frame advances the file by `48 000 / 44 100` of its frames, so the ramp, which
/// is linear, steps by that much of its own step on the frames after the
/// re-rate.
#[test]
fn a_stream_plays_on_at_its_pitch_after_a_rerate() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let (mut s, clip) = clip_of(&path, true);
    let key = s.graph().add(clip.at(Beat::new(0.0)));
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(key, 0)])
        .expect("the clip's channel into the main output's");
    let _ = s.update();
    // A streamed clip's ring is filled on the streamer's thread: wait for it.
    tutti_fixtures::wait_until(
        "the stream to be primed",
        tutti_fixtures::HANG_GUARD,
        || {
            s.graph_ref()
                .handle(&key)
                .and_then(|h| h.status())
                .is_none_or(|status| status.phase != StreamPhase::Opening)
        },
    );
    s.play().expect("room");
    let mut block = vec![0.0f32; BLOCK * 2];
    let mut render = |s: &mut Session, heard: &mut Vec<f32>| {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        heard.extend(block.as_chunks::<2>().0.iter().map(|frame| frame[0]));
    };
    // Sounding at 48 kHz, past the start fade.
    let mut before = Vec::new();
    while before.len() < 1_024 {
        render(&mut s, &mut before);
    }
    let first = before.iter().position(|&x| x != 0.0).expect("it sounds");
    assert_eq!(
        before[first + 900],
        value(900),
        "the file's own frames at 48 kHz"
    );

    s.set_sample_rate(SampleRate::SR_44K1).unwrap();
    // The re-prepare's second half lands once a block has run and an update
    // has collected; the blocks rendered meanwhile are not pinned.
    let mut meanwhile = Vec::new();
    for _ in 0..16 {
        render(&mut s, &mut meanwhile);
        let _ = s.update();
        if s.commit() != tutti_session::Committed::Retry {
            break;
        }
    }
    assert_eq!(s.sample_rate(), SampleRate::SR_44K1);
    // Past any fade of the re-prepare and the ring's refill: the file has
    // 4 800 frames, 4 410 output frames at the new rate.
    let mut after = Vec::new();
    while after.len() < 1_024 {
        render(&mut s, &mut after);
    }
    let step = f64::from(value(1) - value(0)) * RATE / 44_100.0;
    for k in 300..700 {
        let got = f64::from(after[k + 1] - after[k]);
        assert!(
            (got - step).abs() <= step * 1e-3,
            "output frame {k} after the re-rate: a step of {got}, want {step}"
        );
    }
}

/// **`auto` decides by the file's decoded size.** A short file reads resident,
/// holding the ramp the test wrote (frame `i` is `value(i)`); one past
/// `Tier::STREAM_ABOVE_BYTES` of decoded audio reads streamed, with the
/// frame count the test wrote. (The long file is 16-bit mono, whose decoded
/// `f32` form is twice its size on disk.)
#[test]
fn auto_streams_a_file_past_the_threshold_and_holds_a_short_one() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let short = dir.path().join("short.wav");
    write_ramp(&short);
    let long = dir.path().join("long.wav");
    let frames = (Tier::STREAM_ABOVE_BYTES / 4) as usize + 1;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&long, spec).expect("creates");
    for _ in 0..frames {
        w.write_sample(0i16).expect("writes");
    }
    w.finalize().expect("finalizes");

    let s = offline_48k();
    let short = tutti_fixtures::block_on(s.load_audio(&short)).expect("loads");
    let long = tutti_fixtures::block_on(s.load_audio(&long)).expect("loads");
    let AudioData::Resident(wave) = short.data() else {
        panic!("a short file is resident");
    };
    for i in 0..LEN {
        assert_eq!(wave.at(0, i), value(i), "frame {i}");
    }
    let AudioData::Streamed(info) = long.data() else {
        panic!("a file past the line streams");
    };
    assert_eq!(info.frames.map(|f| f.get()), Some(frames));
}

/// **`Tier::Auto { above }` compares the decoded size with `above`.** A file
/// under the default line reads streamed under a threshold one byte below its
/// decoded size, and resident at its decoded size.
#[test]
fn the_threshold_is_the_files_decoded_size() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let s = offline_48k();
    let decoded = (LEN as u64) * 4;
    let load = |above| tutti_fixtures::block_on(s.load_audio(&path).tier(Tier::Auto { above }));
    assert!(matches!(
        load(decoded).expect("loads").data(),
        AudioData::Resident(_)
    ));
    assert!(matches!(
        load(decoded - 1).expect("loads").data(),
        AudioData::Streamed(_)
    ));
}
