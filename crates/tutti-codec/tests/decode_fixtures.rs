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

use tutti_buffers::Wave;
use tutti_buffers::{Interleaved, InterleavedMut, PlanesMut, Pull, Source, deinterleave};
use tutti_codec::FloatWidth;
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_codec::{FileIn, read_wave, read_wave_metadata};
use tutti_platform::Asset;
use tutti_units::Samples;

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
        let w = read_wave::<f32>(asset(name)).expect("load");
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
        let w = read_wave::<f32>(asset(name)).expect("load");
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
            let ch = w.channel(c).samples();
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
        let w = read_wave::<f32>(asset(name)).expect("load");
        let ch = w.channels();
        let mut dec = FileIn::<f32>::open(asset(name)).expect("open");
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

/// The header agrees with the decode: `read_wave_metadata`'s width, topology, rate **and
/// frame count** are what `read_wave` actually produces, for every fixture.
///
/// Restores the fork's `probe_metadata_matches_full_load`, which the move
/// deleted along with the fork's WAV writer it depended on.
#[test]
fn the_probe_agrees_with_the_full_load() {
    for &name in FIXTURES {
        let w = read_wave::<f32>(asset(name)).expect("load");
        let meta = read_wave_metadata(asset(name)).expect("probe");
        assert_eq!(meta.channels(), w.channels(), "{name}: probed width");
        assert_eq!(meta.channel_set, w.channel_set(), "{name}: probed channels");
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
        let w = read_wave::<f32>(asset(name)).expect("load");
        let ch = w.channels();
        let len = w.len().get();
        let starts = [1, 700, len / 3, len - 100];

        let check = |dec: &mut FileIn, start: usize, how: &str| {
            dec.seek(Samples::new(start)).expect("seek");
            assert_eq!(
                dec.cursor(),
                Samples::new(start),
                "{name}: cursor after seek"
            );
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
            let mut dec = FileIn::<f32>::open(asset(name)).expect("open");
            assert!(dec.seekable(), "{name}: every fixture must be seekable");
            check(&mut dec, start, "fresh");
        }
        let mut dec = FileIn::<f32>::open(asset(name)).expect("open");
        for &start in starts.iter().rev() {
            check(&mut dec, start, "repeated");
        }
    }
}

/// A PCM WAV built byte by byte from the RIFF/WAVE layout: a `fmt ` chunk
/// (format 1, `planes.len()` channels, 48 kHz, `bits` per sample) and a `data`
/// chunk holding the planes' integers frame by frame, little-endian.
fn pcm_wav(planes: &[Vec<i32>], bits: u16) -> Vec<u8> {
    let channels = planes.len() as u16;
    let width = bits / 8;
    let mut data = Vec::new();
    for i in 0..planes[0].len() {
        for plane in planes {
            data.extend(&plane[i].to_le_bytes()[..usize::from(width)]);
        }
    }
    riff_wav(1, channels, bits, &data)
}

/// A WAV of format tag `tag` holding `data` as it is, `channels` wide at 48 kHz
/// with `bits` stored per sample.
fn riff_wav(tag: u16, channels: u16, bits: u16, data: &[u8]) -> Vec<u8> {
    let width = bits / 8;
    let data_len = data.len() as u32;
    // A format other than PCM states a zero-length extension (`cbSize`) after
    // the 16 bytes every `fmt ` chunk starts with.
    let extension: &[u8] = if tag == 1 { &[] } else { &[0, 0] };
    let fmt_len = 16 + extension.len() as u32;
    let mut out = b"RIFF".to_vec();
    out.extend((20 + fmt_len + data_len).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(fmt_len.to_le_bytes());
    out.extend(tag.to_le_bytes());
    out.extend(channels.to_le_bytes());
    out.extend(48_000u32.to_le_bytes());
    out.extend((48_000 * u32::from(channels) * u32::from(width)).to_le_bytes());
    out.extend((channels * width).to_le_bytes());
    out.extend(bits.to_le_bytes());
    out.extend(extension);
    out.extend(b"data");
    out.extend(data_len.to_le_bytes());
    out.extend(data);
    out
}

/// Two channels of `frames` integers in `min..=max`: the extremes, -1 and 0
/// first, then a ramp that sweeps the range with a different slope per channel.
fn integer_planes(min: i32, max: i32, frames: usize) -> Vec<Vec<i32>> {
    let head = [min, max, -1, 0, 1, min + 1, max - 1];
    let span = i64::from(max) - i64::from(min);
    (0..2)
        .map(|c| {
            (0..frames)
                .map(|i| match head.get(i) {
                    Some(&v) if c == 0 => v,
                    Some(&v) => -1 - v,
                    None => (i64::from(min) + (i as i64 * (7 + 4 * c as i64) * 977) % span) as i32,
                })
                .collect()
        })
        .collect()
}

/// A decode as the file's own format keeps every integer the file holds: the
/// bytes of a 16-bit and a 24-bit PCM WAV written from the format's layout
/// decode as `i16` and `I24` to exactly the integers put in, extremes
/// included, and the header names those formats.
#[test]
fn a_native_decode_returns_the_files_exact_integers() {
    use tutti_buffers::{I24, SampleFormat};
    let dir = tempfile::tempdir().expect("tmp dir");
    for (bits, min, max, format) in [
        (
            16u16,
            i32::from(i16::MIN),
            i32::from(i16::MAX),
            SampleFormat::I16,
        ),
        (24, I24::MIN.get(), I24::MAX.get(), SampleFormat::I24),
    ] {
        let want = integer_planes(min, max, 3000);
        let bytes = pcm_wav(&want, bits);
        let path = dir.path().join(format!("pcm{bits}.wav"));
        std::fs::write(&path, &bytes).expect("write");
        assert_eq!(read_wave_metadata(&path).expect("probe").format, format);
        let got: Vec<Vec<i32>> = if bits == 16 {
            let wave = read_wave::<i16>(bytes).expect("decode");
            (0..2)
                .map(|c| wave.channel(c).iter().map(|&s| i32::from(s)).collect())
                .collect()
        } else {
            let wave = read_wave::<I24>(bytes).expect("decode");
            (0..2)
                .map(|c| wave.channel(c).iter().map(|s| s.get()).collect())
                .collect()
        };
        assert_eq!(got, want, "{bits}-bit");
    }
}

/// `FileIn` as `i16` streams the same integers, across several pulls.
#[test]
fn a_native_stream_returns_the_files_exact_integers() {
    let want = integer_planes(i32::from(i16::MIN), i32::from(i16::MAX), 3000);
    let dir = tempfile::tempdir().expect("tmp dir");
    let path = dir.path().join("stream.wav");
    std::fs::write(&path, pcm_wav(&want, 16)).expect("write");
    let mut dec = FileIn::<i16>::open(&path).expect("open");
    let layout = dec.layout();
    let mut got = vec![Vec::new(); 2];
    let mut buf = vec![0i16; 1000 * 2];
    loop {
        let out = InterleavedMut::new(&mut buf, layout).unwrap();
        let Pull::Frames(n) = dec.pull(out) else {
            break;
        };
        let pulled = Interleaved::new(&buf[..layout.interleaved_len(n)], layout).unwrap();
        let mut planes = vec![vec![0i16; n.get()]; 2];
        deinterleave(pulled, PlanesMut::from_vecs(&mut planes).unwrap()).expect("same shape");
        for (plane, chunk) in got.iter_mut().zip(planes) {
            plane.extend(chunk.into_iter().map(i32::from));
        }
    }
    assert_eq!(got, want);
}

/// A lossy codec's samples are floats, and the header says so.
#[test]
fn a_lossy_file_is_native_float() {
    for name in ["stereo.mp3", "stereo.ogg"] {
        let meta = read_wave_metadata(asset(name)).expect("probe");
        assert_eq!(meta.format, tutti_buffers::SampleFormat::F32, "{name}");
    }
}

/// Both entry points, a path and the bytes alone, decode a RIFF-built 16-bit
/// WAV to the integers put in, and the header says `I16` and the rate and width
/// put in.
#[test]
fn both_entry_points_decode_a_riff_built_file_to_its_integers() {
    let want = integer_planes(i32::from(i16::MIN), i32::from(i16::MAX), 500);
    let bytes = pcm_wav(&want, 16);
    let dir = tempfile::tempdir().expect("tmp dir");
    let path = dir.path().join("both.wav");
    std::fs::write(&path, &bytes).expect("write");
    let ints = |wave: Wave<i16>| -> Vec<Vec<i32>> {
        (0..wave.channels())
            .map(|c| wave.channel(c).iter().map(|&s| i32::from(s)).collect())
            .collect()
    };
    let from_path = read_wave::<i16>(&path).expect("load");
    assert_eq!(from_path.sample_rate().get(), 48_000.0);
    assert_eq!(ints(from_path), want, "path");
    assert_eq!(
        ints(read_wave::<i16>(bytes).expect("decode bytes")),
        want,
        "bytes"
    );
}

/// An 8-bit WAV is unsigned with 128 as zero: the bytes 0, 128 and 255 are the
/// signed -128, 0 and 127, and the header says `I8`.
#[test]
fn an_8_bit_wav_is_unsigned_around_128() {
    let bytes = riff_wav(1, 1, 8, &[0, 128, 255, 1, 127, 129]);
    let dir = tempfile::tempdir().expect("tmp dir");
    let path = dir.path().join("u8.wav");
    std::fs::write(&path, &bytes).expect("write");
    assert_eq!(
        read_wave_metadata(&path).expect("probe").format,
        tutti_buffers::SampleFormat::I8
    );
    let wave = read_wave::<i8>(bytes).expect("decode");
    assert_eq!(wave.channel(0).samples(), &[-128i8, 0, 127, -127, -1, 1]);
}

/// A 32-bit integer WAV keeps `i32::MIN`, `i32::MAX`, -1 and 0 exactly.
#[test]
fn a_32_bit_wav_keeps_every_integer() {
    let ints = [i32::MIN, i32::MAX, -1, 0, 1, i32::MIN + 1, i32::MAX - 1];
    let bytes = pcm_wav(&[ints.to_vec()], 32);
    let wave = read_wave::<i32>(bytes).expect("decode");
    assert_eq!(wave.channel(0).samples(), &ints);
}

/// A 16-bit file asked for as another format converts by `tutti_buffers`'
/// rule: to a float it is `v / 32768`, exact, and to `I24` it is `v * 256`.
#[test]
fn a_16_bit_file_read_as_another_format_follows_the_conversion_rule() {
    let ints = [i16::MIN, i16::MAX, -1, 0, 1, 16384, -16384, 12345];
    let planes = vec![ints.iter().map(|&v| i32::from(v)).collect::<Vec<_>>()];
    let bytes = pcm_wav(&planes, 16);
    let float = read_wave::<f32>(bytes.clone()).expect("f32");
    let double = read_wave::<f64>(bytes.clone()).expect("f64");
    let wide = read_wave::<tutti_buffers::I24>(bytes).expect("i24");
    for (i, &v) in ints.iter().enumerate() {
        assert_eq!(float.at(0, i), f32::from(v) / 32768.0, "f32 of {v}");
        assert_eq!(double.at(0, i), f64::from(v) / 32768.0, "f64 of {v}");
        assert_eq!(wide.at(0, i).get(), i32::from(v) * 256, "i24 of {v}");
    }
}

/// The G.711 A-law expansion of one byte (ITU-T G.711 table 1a): the byte's
/// even bits are inverted, its low nibble is the mantissa, bits 4-6 the
/// segment, and bit 7 the sign (set is positive).
fn alaw_expand(byte: u8) -> i16 {
    let a = byte ^ 0x55;
    let mantissa = i32::from(a & 0x0F) << 4;
    let segment = (a & 0x70) >> 4;
    let magnitude = match segment {
        0 => mantissa + 8,
        s => (mantissa + 0x108) << (s - 1),
    };
    (if a & 0x80 != 0 { magnitude } else { -magnitude }) as i16
}

/// An A-law WAV (format tag 6, 8 bits stored) carries no bit depth the header
/// could map, yet it decodes to 16-bit integers: the header says `I16` and
/// every decoded integer is the G.711 expansion of its byte.
#[test]
fn an_a_law_wav_is_16_bit_integers() {
    let coded = [0xD5u8, 0x55, 0xAA, 0x2A, 0xFF, 0x7F, 0x80, 0x00, 0x95, 0x1B];
    let bytes = riff_wav(6, 1, 8, &coded);
    let dir = tempfile::tempdir().expect("tmp dir");
    let path = dir.path().join("alaw.wav");
    std::fs::write(&path, &bytes).expect("write");
    assert_eq!(
        read_wave_metadata(&path).expect("probe").format,
        tutti_buffers::SampleFormat::I16
    );
    let wave = read_wave::<i16>(bytes).expect("decode");
    let want: Vec<i16> = coded.iter().map(|&b| alaw_expand(b)).collect();
    assert_eq!(wave.channel(0).samples(), want.as_slice());
}

/// The bytes of a file give the metadata its path gives, for every fixture.
#[test]
fn the_metadata_of_a_files_bytes_is_the_metadata_of_its_path() {
    for &name in FIXTURES {
        let bytes: std::sync::Arc<[u8]> = std::fs::read(asset(name)).expect("fixture").into();
        let from_bytes = read_wave_metadata(Asset::from(bytes)).expect("probe bytes");
        let from_path = read_wave_metadata(Asset::from(asset(name))).expect("probe path");
        assert_eq!(from_bytes, from_path, "{name}");
    }
}

/// A `FileIn` over a file's bytes decodes the frames `FileIn::open`
/// decodes from the file, bit for bit, for every fixture.
#[test]
fn a_stream_over_bytes_reads_the_frames_the_path_stream_reads() {
    /// Every frame `dec` yields, interleaved.
    fn drain(dec: &mut FileIn) -> Vec<f32> {
        let mut buf = vec![0.0f32; dec.layout().interleaved_len(Samples::new(333))];
        let mut all = Vec::new();
        loop {
            let n = read(dec, &mut buf);
            if n == 0 {
                return all;
            }
            all.extend_from_slice(&buf[..dec.layout().interleaved_len(Samples::new(n))]);
        }
    }
    for &name in FIXTURES {
        let bytes = std::fs::read(asset(name)).expect("fixture");
        let mut from_bytes = FileIn::<f32>::open(Asset::from(bytes)).expect("bytes");
        let mut from_path = FileIn::<f32>::open(Asset::from(asset(name))).expect("open");
        assert_eq!(
            from_bytes.total_frames(),
            from_path.total_frames(),
            "{name}"
        );
        assert_eq!(from_bytes.channel_set(), from_path.channel_set(), "{name}");
        let (a, b) = (drain(&mut from_bytes), drain(&mut from_path));
        assert!(!a.is_empty(), "{name}: decoded nothing");
        assert!(
            a.iter()
                .map(|x| x.to_bits())
                .eq(b.iter().map(|x| x.to_bits())),
            "{name}: frames differ"
        );
    }
}

/// An AIFF the crate writes reads back sample for sample, in the sample
/// format it was written in, at every depth AIFF holds as written: the
/// integer widths and the integer extremes, and `f32` unquantized.
#[test]
fn an_aiff_written_reads_back_exactly_at_each_depth() {
    use tutti_buffers::{I24, Mono, Sample, SampleFormat};
    use tutti_codec::{Format, Pcm, write_wave};
    use tutti_units::SampleRate;

    fn round_trip<S: Sample + std::fmt::Debug>(samples: &[S], pcm: Pcm, format: SampleFormat) {
        let wave = Wave::from_mono(SampleRate::SR_48K, Mono::new(samples));
        let mut bytes = Vec::new();
        write_wave(&wave, &mut bytes, Format::Aiff(pcm)).unwrap();
        assert_eq!(read_wave_metadata(bytes.clone()).unwrap().format, format);
        assert_eq!(read_wave::<S>(bytes).unwrap(), wave, "{pcm:?}");
    }

    let int16 = [i16::MIN, -12_345, -1, 0, 1, 12_345, i16::MAX];
    round_trip(
        &int16,
        Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither()),
        SampleFormat::I16,
    );
    let int24 = [
        I24::MIN,
        I24::of(-1_234_567),
        I24::of(-1),
        I24::of(0),
        I24::of(1),
        I24::of(1_234_567),
        I24::MAX,
    ];
    round_trip(
        &int24,
        Pcm::Int(IntPcm::new(IntDepth::Int24).no_dither()),
        SampleFormat::I24,
    );
    let float = [-1.0f32, -0.5, -1.0e-7, 0.0, 1.0e-7, 0.123_456_79, 1.0];
    round_trip(&float, Pcm::Float(FloatWidth::F32), SampleFormat::F32);
}
