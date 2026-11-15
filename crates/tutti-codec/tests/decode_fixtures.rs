//! What the decode path produces for the committed fixtures in
//! `assets/audio/`.
//!
//! The lossless fixtures are sums of sines the generator wrote and quantised to
//! their sample format, so every decoded sample is held to the generator's
//! closed form; the lossy ones are held to their levels. Every container is
//! held to the invariants of the decode path: the stream is the load, the
//! probe is the load's header, a seek lands on the load's frames.
//!
//! Not feature-gated: the crate's dev-dependency on itself turns every decoder
//! on for its tests, so this fails to compile rather than silently vanishing
//! if that ever stops being true.

use std::path::PathBuf;

use tutti_buffers::{InterleavedMut, Pull, Source};
use tutti_codec::{FileIn, Wave};
use tutti_types::Samples;

/// The frames one pull produced, `0` for any verdict (these loops read until
/// the file stops, and each asserts a clean file reaches its end).
fn read(dec: &mut FileIn, buf: &mut [f32]) -> usize {
    let out = InterleavedMut::new(buf, dec.layout()).unwrap();
    match dec.pull(out) {
        Pull::Frames(n) => n.get(),
        _ => 0,
    }
}

fn asset(name: &str) -> PathBuf {
    tutti_fixtures::asset(&format!("audio/{name}"))
}
/// Every fixture.
const FIXTURES: &[&str] = &[
    "stereo_s16.wav",
    "stereo_s24.wav",
    "mono_48k.wav",
    "surround_51.wav",
    "stereo.flac",
    "stereo.mp3",
    "stereo.ogg",
];

/// A sine partial: `(frequency in hertz, amplitude)`.
type Partial = (f64, f64);

/// `(file, sample rate, frames, per channel: its partials, one least-significant bit)`
/// of the lossless fixtures: sample `i` of a channel is
/// `sum(amplitude * sin(2 pi frequency i / rate))`, quantised to the file's
/// depth, so a decoded sample is within one step of it. These figures are the
/// recipe the fixtures were written from (sums of sines), not an independent
/// oracle: they hold the decoder to the recipe.
const GENERATED: &[(&str, f64, usize, &[&[Partial]], f64)] = &[
    (
        "stereo_s16.wav",
        44_100.0,
        11025,
        &[
            &[(440.0, 0.5), (3001.0, 0.2)],
            &[(660.0, 0.4), (5003.0, -0.25)],
        ],
        1.0 / 32_768.0,
    ),
    (
        "stereo_s24.wav",
        44_100.0,
        11025,
        &[
            &[(440.0, 0.5), (3001.0, 0.2)],
            &[(660.0, 0.4), (5003.0, -0.25)],
        ],
        1.0 / 8_388_608.0,
    ),
    // The FLAC fixture is the 24-bit WAV's signal: a lossless decode lands on
    // the same samples.
    (
        "stereo.flac",
        44_100.0,
        11025,
        &[
            &[(440.0, 0.5), (3001.0, 0.2)],
            &[(660.0, 0.4), (5003.0, -0.25)],
        ],
        1.0 / 8_388_608.0,
    ),
    (
        "mono_48k.wav",
        48_000.0,
        9600,
        &[&[(220.0, 0.6)]],
        1.0 / 32_768.0,
    ),
    (
        "surround_51.wav",
        44_100.0,
        4410,
        &[
            &[(100.0, 0.1)],
            &[(200.0, 0.2)],
            &[(300.0, 0.3)],
            &[(50.0, 0.05)],
            &[(400.0, 0.4)],
            &[(500.0, 0.5)],
        ],
        1.0 / 32_768.0,
    ),
];

/// A lossless fixture decodes to its generator's closed form, within one
/// quantisation step, in every sample of every channel: the width, the rate
/// and the length are the generator's too. A dropped packet, a swapped channel,
/// a wrong scale or a shifted sample moves a sample by far more than a step.
#[test]
fn lossless_fixtures_decode_to_their_generator() {
    for &(name, rate, frames, channels, step) in GENERATED {
        let w = Wave::read(asset(name)).expect("load");
        assert_eq!(w.channels(), channels.len(), "{name}: channels");
        assert_eq!(w.sample_rate().get(), rate, "{name}: rate");
        assert_eq!(w.len().get(), frames, "{name}: frames");
        for (c, partials) in channels.iter().enumerate() {
            for i in 0..w.len().get() {
                let want: f64 = partials
                    .iter()
                    .map(|&(f, a)| a * (std::f64::consts::TAU * f * i as f64 / rate).sin())
                    .sum();
                let got = f64::from(w.at(c, i));
                assert!(
                    (got - want).abs() <= step,
                    "{name}: channel {c} frame {i}: {got}, the generator gives {want}"
                );
            }
        }
    }
}

/// The lossy fixtures, encoded from the stereo generator's signal.
const LOSSY: &[&str] = &["stereo.mp3", "stereo.ogg"];

/// A lossy fixture decodes to the generator's level, within what an encoder
/// loses: per channel RMS within 1.5 dB, and peak under 1 dB above and over
/// half of the closed form (RMS `sqrt(sum(amplitude^2 / 2))`, peak the sum of the amplitudes'
/// magnitudes at most), and a length of the generator's 11 025 frames plus at
/// most two MP3 frames (2 304) of encoder delay and padding. The measured
/// figures are an RMS 0.2 to 1.1 dB low (the padding is silence in the mean)
/// and a peak within 0.4 dB; an encoder's overshoot stays inside the bound.
/// The bound is loose: it catches a missing or swapped channel, a dropped
/// packet's worth of silence, or an RMS gain error over 1.5 dB, not a small drift
/// (the peak bound is tight above and loose below, and the length is a range,
/// not an exact count), because a lossy decode has no closed form to the
/// sample.
#[test]
fn lossy_fixtures_decode_to_the_generator_level() {
    const CHANNELS: [&[Partial]; 2] = [
        &[(440.0, 0.5), (3001.0, 0.2)],
        &[(660.0, 0.4), (5003.0, -0.25)],
    ];
    let db = |got: f64, want: f64| 20.0 * (got / want).log10();
    for &name in LOSSY {
        let w = Wave::read(asset(name)).expect("load");
        assert_eq!(w.channels(), 2, "{name}: channels");
        assert!(
            (11_025..=11_025 + 2_304).contains(&w.len().get()),
            "{name}: {} frames",
            w.len().get()
        );
        for (c, partials) in CHANNELS.iter().enumerate() {
            let rms = partials
                .iter()
                .map(|&(_, a)| a * a / 2.0)
                .sum::<f64>()
                .sqrt();
            let peak: f64 = partials.iter().map(|&(_, a)| a.abs()).sum();
            let ch = w.channel(c);
            let got_rms = (ch.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>()
                / ch.len() as f64)
                .sqrt();
            let got_peak = ch.iter().fold(0.0f64, |m, &s| m.max(f64::from(s).abs()));
            assert!(
                db(got_rms, rms).abs() < 1.5,
                "{name}: channel {c} RMS {got_rms}, the generator's is {rms}"
            );
            assert!(
                db(got_peak, peak) < 1.0 && got_peak > 0.5 * peak,
                "{name}: channel {c} peak {got_peak}, the generator's is at most {peak}"
            );
        }
    }
}

/// Streaming a file from the start yields every frame the whole-file load
/// does, bit for bit, in every container.
///
/// This is the test that found `FileIn` reading **nothing** from an Ogg file:
/// Vorbis's first packet decodes to zero frames (it only primes the overlap),
/// and the streamer took a zero-frame packet for end-of-stream. A streamed Ogg
/// clip therefore played as silence from its first block, and `probe` called
/// it streamable because the container reports a frame count.
#[test]
fn the_stream_reads_every_frame_the_load_does() {
    for &name in FIXTURES {
        let w = Wave::read(asset(name)).expect("load");
        let ch = w.channels();
        let mut dec = FileIn::open(asset(name)).expect("open");
        let mut buf = vec![0.0f32; 333 * ch];
        let mut at = 0usize;
        loop {
            let n = read(&mut dec, &mut buf);
            if n == 0 {
                break;
            }
            for f in 0..n {
                for c in 0..ch {
                    assert_eq!(
                        buf[f * ch + c].to_bits(),
                        w.at(c, at + f).to_bits(),
                        "{name}: frame {} channel {c}",
                        at + f
                    );
                }
            }
            at += n;
        }
        assert_eq!(at, w.len().get(), "{name}: streamed frame count");
        assert_eq!(
            dec.error().map(ToString::to_string),
            None,
            "{name} ends cleanly"
        );
    }
}

/// The header agrees with the decode: `Wave::read_metadata`'s width, topology, rate **and
/// frame count** are what `Wave::read` actually produces, for every fixture.
///
/// Restores the fork's `probe_metadata_matches_full_load`, which the move
/// deleted along with the fork's WAV writer it depended on.
#[test]
fn the_probe_agrees_with_the_full_load() {
    for &name in FIXTURES {
        let w = Wave::read(asset(name)).expect("load");
        let meta = Wave::read_metadata(asset(name)).expect("probe");
        assert_eq!(meta.channels(), w.channels(), "{name}: probed width");
        assert_eq!(meta.topology, *w.topology(), "{name}: probed topology");
        assert_eq!(meta.sample_rate, w.sample_rate(), "{name}: probed rate");
        assert_eq!(
            meta.total_frames,
            Some(w.len().get() as u64),
            "{name}: probed frame count"
        );
    }
}

/// Where a seek landed, as a lag against the whole-file load: the offset `d`
/// at which the frames read after `seek(start)` equal the load's frames at
/// `start + d`, or `None` if no offset in `-4096..=4096` matches. Used only to
/// make a failure say *how far off* a seek was.
fn lag_of(w: &Wave, start: usize, out: &[f32]) -> Option<i64> {
    let ch = w.channels();
    let n = out.len() / ch;
    (-4096i64..=4096).find(|&d| {
        let s = start as i64 + d;
        s >= 0
            && (s as usize + n) <= w.len().get()
            && (0..n).all(|f| {
                (0..ch).all(|c| out[f * ch + c].to_bits() == w.at(c, s as usize + f).to_bits())
            })
    })
}

/// After a seek, `FileIn` produces exactly the frames a whole-file load has
/// at that position — for **every** container, lossy included. Position is the
/// contract; the fixtures are also sample-identical after a seek, so that is
/// asserted, bit for bit.
///
/// Starts cover frame 1 (inside the first packet), 700 (mid-packet for every
/// codec here), a third of the way in, and 100 frames from the end. Each is
/// seeked on a fresh decoder, then again in reverse order on one decoder, so a
/// seek from a mid-stream state is covered too.
///
/// This pins the fix for an Ogg seek landing 1024 frames late (576 from frame
/// 1): after `decoder.reset()` the first Vorbis packet decodes to zero frames
/// and is skipped, but the preroll discard was counted from the seek's
/// `actual_ts` rather than from the packet that produced audio — and when that
/// primer is the packet containing `start`, no discard can recover it, so the
/// seek has to step back past it.
#[test]
fn seek_lands_on_the_whole_file_load_frames() {
    for &name in FIXTURES {
        let w = Wave::read(asset(name)).expect("load");
        let ch = w.channels();
        let len = w.len().get();
        let starts = [1, 700, len / 3, len - 100];

        let check = |dec: &mut FileIn, start: usize, how: &str| {
            dec.seek(Samples(start)).expect("seek");
            assert_eq!(dec.cursor(), Samples(start), "{name}: cursor after seek");
            let want = 257.min(len - start);
            let mut out = vec![0.0f32; want * ch];
            assert_eq!(
                read(dec, &mut out),
                want,
                "{name}: read count after {how} seek to {start}"
            );
            let exact = (0..want).all(|f| {
                (0..ch).all(|c| out[f * ch + c].to_bits() == w.at(c, start + f).to_bits())
            });
            assert!(
                exact,
                "{name}: {how} seek to {start} did not land on the load's frames; \
                 it matches the load at lag {:?}",
                lag_of(&w, start, &out)
            );
        };

        for &start in &starts {
            let mut dec = FileIn::open(asset(name)).expect("open");
            assert!(dec.seekable(), "{name}: every fixture must be seekable");
            check(&mut dec, start, "fresh");
        }
        let mut dec = FileIn::open(asset(name)).expect("open");
        for &start in starts.iter().rev() {
            check(&mut dec, start, "repeated");
        }
    }
}

/// `Wave::from_bytes`, which probes from the bytes alone with no extension
/// hint, decodes to the same samples as the path-based load, for every fixture.
#[test]
fn from_bytes_decodes_what_load_decodes() {
    for &name in FIXTURES {
        let want = Wave::read(asset(name)).expect("load");
        let bytes = std::fs::read(asset(name)).expect("read fixture");
        let decoded = Wave::from_bytes(bytes).expect("decode bytes");
        assert_eq!(decoded.channels(), want.channels(), "{name}: channels");
        assert_eq!(decoded.len(), want.len(), "{name}: frames");
        assert_eq!(decoded.sample_rate(), want.sample_rate(), "{name}: rate");
        for c in 0..want.channels() {
            for i in 0..want.len().get() {
                assert_eq!(
                    decoded.at(c, i).to_bits(),
                    want.at(c, i).to_bits(),
                    "{name}: channel {c} frame {i}"
                );
            }
        }
    }
}
