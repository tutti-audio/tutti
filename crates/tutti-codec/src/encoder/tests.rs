use super::*;
use crate::test_sink::Full;
use crate::{EncodeError, SpecError};
use std::io::Cursor;
#[cfg(any(
    all(feature = "encode-wav", feature = "decode-wav"),
    feature = "encode-aiff"
))]
use std::io::{Seek, Write};
use tutti_buffers::ChannelSet;
use tutti_buffers::FloatWidth;
use tutti_units::SampleRate;

/// Every format this build writes, at 16 bits where it has a width, with the
/// name its file would carry.
#[allow(clippy::vec_init_then_push)]
fn formats() -> Vec<(&'static str, Format)> {
    #[allow(unused_mut)]
    let mut all: Vec<(&'static str, Format)> = Vec::new();
    #[cfg(feature = "encode-wav")]
    all.push((
        "wav",
        Format::Wav(crate::Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither())),
    ));
    #[cfg(feature = "encode-aiff")]
    all.push((
        "aiff",
        Format::Aiff(crate::Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither())),
    ));
    #[cfg(feature = "encode-flac")]
    all.push((
        "flac",
        Format::Flac {
            pcm: crate::IntPcm::new(crate::IntDepth::Int16).no_dither(),
            level: crate::FlacLevel::default(),
        },
    ));
    all
}

fn spec(format: Format, channels: u16) -> Spec {
    Spec::of_channels(
        format,
        ChannelSet::default_for(ChannelLayout::from(channels)),
        SampleRate::SR_44K1,
    )
}

fn at_rate(format: Format, rate: f64) -> Spec {
    Spec::of_channels(
        format,
        ChannelSet::default_for(ChannelLayout::STEREO),
        SampleRate::of(rate),
    )
}

/// `frames` frames of a sawtooth under a hash, exact in binary and not
/// compressible to nothing.
fn saw(channels: u16, frames: usize) -> Vec<f32> {
    (0..frames * usize::from(channels))
        .map(|i| {
            let noise = ((i as u64 * i as u64 * 2_654_435_761) >> 11) % 32_768;
            (i % 128) as f32 / 128.0 - 0.5 + noise as f32 / 32_768.0 / 4.0
        })
        .collect()
}

/// The names in `dir`, sorted.
fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A staged file appears at its path only when `finish` moves it there, and
/// `replace` puts it over what was there.
#[test]
fn a_file_appears_at_its_path_only_when_finished() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        std::fs::write(&path, b"previous").unwrap();
        let spec = spec(format, 2);
        let mut encoder = EncoderOptions::new(&spec)
            .file(&path)
            .replace()
            .create()
            .unwrap();
        encoder
            .write(Interleaved::new(&saw(2, 5_000), spec.layout()).unwrap())
            .unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"previous",
            "{ext}: the path is untouched while the encoder is open"
        );
        encoder.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > 100, "{ext}: the finished file replaced it");
        assert_eq!(
            listing(dir.path()),
            [format!("take.{ext}")],
            "{ext}: no temporary left"
        );
        std::fs::remove_file(&path).unwrap();
    }
}

/// An encoder dropped before `finish` removes its file and leaves what was at
/// the path alone.
#[test]
fn a_dropped_encoder_removes_its_file_and_leaves_the_path_alone() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        std::fs::write(&path, b"previous").unwrap();
        let spec = spec(format, 2);

        let mut dropped = EncoderOptions::new(&spec)
            .file(&path)
            .replace()
            .create()
            .unwrap();
        dropped
            .write(Interleaved::new(&saw(2, 5_000), spec.layout()).unwrap())
            .unwrap();
        drop(dropped);

        assert_eq!(std::fs::read(&path).unwrap(), b"previous", "{ext}");
        assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");
        std::fs::remove_file(&path).unwrap();
    }
}

/// A write that fails mid-take, and the drop that follows, leave the path as
/// it was and no temporary. The failure is the width check, which every format
/// shares; the full-disk case is below.
#[test]
fn a_refused_block_leaves_nothing_behind_once_dropped() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let spec = spec(format, 2);
        let mut encoder = EncoderOptions::new(&spec).file(&path).create().unwrap();
        let err = encoder
            .write(Interleaved::new(&saw(1, 100), ChannelLayout::MONO).unwrap())
            .expect_err("a mono block is not stereo");
        assert!(
            matches!(
                err,
                EncodeError::BlockWidth { expected, got }
                    if expected == ChannelLayout::STEREO && got == ChannelLayout::MONO
            ),
            "{ext}: {err}"
        );
        drop(encoder);
        assert!(
            listing(dir.path()).is_empty(),
            "{ext}: {:?}",
            listing(dir.path())
        );
    }
}

/// A spec the format refuses fails at `create`, before a file exists.
#[test]
fn a_spec_that_cannot_be_written_is_refused_without_a_file() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let zero_width = spec(format, 0);
        let err = EncoderOptions::new(&zero_width)
            .file(&path)
            .create()
            .expect_err("no channels");
        assert!(
            matches!(err, EncodeError::Spec(SpecError::ZeroChannels)),
            "{ext}: {err}"
        );
        let zero_rate = at_rate(format, 0.4);
        let err = EncoderOptions::new(&zero_rate)
            .file(&path)
            .create()
            .expect_err("no rate");
        assert!(
            matches!(err, EncodeError::Spec(SpecError::SampleRate(_))),
            "{ext}: {err}"
        );
        assert!(
            listing(dir.path()).is_empty(),
            "{ext}: a temporary was left"
        );
    }
}

/// A WAV or FLAC header holds whole Hz, so a fractional rate is refused rather
/// than rounded; AIFF's extended float states it exactly.
#[test]
fn a_fractional_rate_is_refused_where_the_header_is_whole_hz_and_exact_in_aiff() {
    for (ext, format) in formats() {
        let s = at_rate(format, 44_100.5);
        let result = EncoderOptions::new(&s).wrap(Cursor::new(Vec::new()));
        if ext == "aiff" {
            let bytes = result
                .expect("aiff states it")
                .finish()
                .unwrap()
                .into_inner();
            let comm = bytes
                .windows(4)
                .position(|w| w == b"COMM")
                .expect("COMM chunk");
            // COMM: id, size, channels (2), frames (4), bits (2), then the rate.
            let rate = &bytes[comm + 8 + 8..comm + 8 + 18];
            let exponent = i32::from(u16::from_be_bytes([rate[0], rate[1]]) & 0x7fff);
            let mantissa = u64::from_be_bytes(rate[2..10].try_into().unwrap());
            let hz = mantissa as f64 * 2f64.powi(exponent - 16_383 - 63);
            assert_eq!(hz, 44_100.5);
        } else {
            assert!(
                matches!(result, Err(EncodeError::Spec(SpecError::SampleRate(_)))),
                "{ext}: 44100.5 Hz was accepted"
            );
            let whole =
                EncoderOptions::new(&at_rate(format, 44_100.0)).wrap(Cursor::new(Vec::new()));
            assert!(whole.is_ok(), "{ext}: 44100 Hz refused");
        }
    }
}

/// Widths past a container's header are refused as spec errors where the
/// container counts them: AIFF in an `i16`.
#[test]
fn a_width_past_the_containers_header_is_refused() {
    #[cfg(feature = "encode-aiff")]
    {
        let s = spec(
            Format::Aiff(crate::Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither())),
            40_000,
        );
        let err = EncoderOptions::new(&s)
            .wrap(Cursor::new(Vec::new()))
            .expect_err("past i16");
        assert!(
            matches!(
                err,
                EncodeError::Spec(SpecError::TooManyChannels {
                    channels: 40_000,
                    max: 32_767
                })
            ),
            "{err}"
        );
    }
}

/// A take that outgrows its sink never reports success: some write, or at
/// the latest `finish`, fails, and the failure is the sink's own.
///
/// The sink holds the headers and little more. Every format fails on the write
/// that reaches the sink.
#[test]
fn a_take_that_outgrows_its_sink_fails_by_finish_at_the_latest() {
    for (ext, format) in formats() {
        let s = spec(format, 2);
        let mut encoder = EncoderOptions::new(&s).wrap(Full::holding(6_000)).unwrap();
        let mut failed: Option<EncodeError> = None;
        for _ in 0..40 {
            if let Err(e) = encoder.write(Interleaved::new(&saw(2, 4_096), s.layout()).unwrap()) {
                failed = Some(e);
                break;
            }
        }
        let outcome = match failed {
            Some(e) => Err(e),
            None => encoder.finish().map(drop),
        };
        let err = outcome.expect_err(&format!("{ext}: a 6000-byte sink cannot hold the take"));
        match err {
            EncodeError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::StorageFull, "{ext}"),
            // Codecs that wrap the sink's error in their own report.
            EncodeError::Encoding { detail, .. } => {
                assert!(!detail.is_empty(), "{ext}: the report names the cause")
            }
            other => panic!("{ext}: expected the sink's failure, got {other}"),
        }
    }
}

/// An encoder is `Send`, so a recording can move it to its pump thread.
///
/// The property is a compile-time one: holding a value that is not `Send` in a
/// writer makes this stop compiling.
#[test]
fn an_encoder_can_move_to_another_thread() {
    fn is_send<T: Send>() {}
    is_send::<Encoder<Cursor<Vec<u8>>>>();
    is_send::<Encoder<std::io::BufWriter<std::fs::File>>>();
    is_send::<FileEncoder>();
}

/// `finish` after a failed write removes the staged file and returns the first
/// error instead of moving a partial take into place.
#[test]
fn finish_after_a_failed_write_returns_the_first_error_and_puts_nothing_in_place() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        std::fs::write(&path, b"previous").unwrap();
        let spec = spec(format, 2);
        let mut encoder = EncoderOptions::new(&spec)
            .file(&path)
            .replace()
            .create()
            .unwrap();
        let first = encoder
            .write(Interleaved::new(&saw(1, 100), ChannelLayout::MONO).unwrap())
            .expect_err("a mono block is not stereo");
        let _ = encoder.write(Interleaved::new(&saw(2, 1), spec.layout()).unwrap());
        let err = encoder.finish().expect_err("a partial take is not a file");
        assert_eq!(err.to_string(), first.to_string(), "{ext}");
        assert_eq!(std::fs::read(&path).unwrap(), b"previous", "{ext}");
        assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");
        std::fs::remove_file(&path).unwrap();
    }
}

/// A planned length past the format's limit is refused by the codec's own
/// error before the temporary exists, and a length at the limit is not.
#[cfg(feature = "encode-wav")]
#[test]
fn a_planned_length_past_the_limit_is_refused_before_the_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take.wav");
    std::fs::write(&path, b"previous").unwrap();
    let format = Format::Wav(crate::Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither()));
    let spec = spec(format, 2);
    let limit = format.max_frames(spec.layout()).unwrap();

    let err = EncoderOptions::new(&spec)
        .planned(Samples::new(limit as usize + 1))
        .file(&path)
        .replace()
        .create()
        .map(drop)
        .expect_err("one frame past the limit");
    assert!(
        matches!(err, EncodeError::WavTooLarge { limit_frames } if limit_frames == limit),
        "{err}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"previous");
    assert_eq!(listing(dir.path()), ["take.wav"], "no temporary was made");

    EncoderOptions::new(&spec)
        .planned(Samples::new(limit as usize))
        .file(&path)
        .replace()
        .create()
        .expect("the limit itself fits");
}

/// A plain sink keeps today's behaviour: `finish` after the limit closes what
/// was written, a readable prefix, and hands the sink back.
#[cfg(feature = "encode-wav")]
#[test]
fn a_plain_sink_keeps_its_prefix_after_a_failed_write() {
    let spec = spec(
        Format::Wav(crate::Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither())),
        2,
    );
    let mut encoder = EncoderOptions::new(&spec)
        .limit_frames(Samples::new(6))
        .wrap(Cursor::new(Vec::new()))
        .unwrap();
    assert!(
        encoder
            .write(Interleaved::new(&saw(2, 8), spec.layout()).unwrap())
            .is_err()
    );
    let bytes = encoder
        .finish()
        .expect("closes what was written")
        .into_inner();
    assert_eq!(bytes.len(), 44 + 6 * 4);
    assert_eq!(hound::WavReader::new(Cursor::new(bytes)).unwrap().len(), 12);
}

/// A buffered sink on a full device never reports success, whichever format:
/// the flush is the encoder's, not the sink's `Drop`'s.
///
/// The device is the kernel's `/dev/full`, which refuses every write with
/// `ENOSPC`; the take fits the buffer, so only the final flush meets it.
#[cfg(target_os = "linux")]
mod full_device {
    use super::*;

    #[test]
    fn a_buffered_full_device_fails_every_format() {
        for (ext, format) in formats() {
            let s = spec(format, 2);
            let device = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap();
            let mut encoder = EncoderOptions::new(&s)
                .wrap(std::io::BufWriter::new(device))
                .unwrap();
            let mut failed = encoder
                .write(Interleaved::new(&saw(2, 100), s.layout()).unwrap())
                .is_err();
            failed |= encoder.finish().is_err();
            assert!(failed, "{ext}: the take reported success on a full device");
        }
    }
}

/// Rates no header can hold are refused for every format.
#[test]
fn a_rate_no_header_holds_is_refused_by_every_format() {
    for (ext, format) in formats() {
        for rate in [1e10, 4_294_967_296.0] {
            let s = at_rate(format, rate);
            let err = EncoderOptions::new(&s)
                .wrap(Cursor::new(Vec::new()))
                .expect_err("no such rate");
            assert!(
                matches!(err, EncodeError::Spec(SpecError::SampleRate(_))),
                "{ext} {rate}: {err}"
            );
        }
    }
}

/// `finalize` closes a file and leaves it beside its path: the path holds what
/// it held, the closed file is a complete one under its temporary name, and
/// `commit` is what moves it. Dropping the `FinalizedFile` instead removes the
/// temporary and leaves the path alone.
#[test]
fn a_finalized_file_waits_for_its_commit() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        std::fs::write(&path, b"previous").unwrap();
        let spec = spec(format, 2);

        let write = |path: &std::path::Path| {
            let mut encoder = EncoderOptions::new(&spec)
                .file(path)
                .replace()
                .create()
                .unwrap();
            encoder
                .write(Interleaved::new(&saw(2, 5_000), spec.layout()).unwrap())
                .unwrap();
            encoder.finalize().unwrap()
        };

        let finalized = write(&path);
        assert_eq!(std::fs::read(&path).unwrap(), b"previous", "{ext}");
        let temp = listing(dir.path())
            .into_iter()
            .find(|n| n != &format!("take.{ext}"))
            .unwrap_or_else(|| panic!("{ext}: no temporary beside the path"));
        assert!(
            std::fs::metadata(dir.path().join(&temp)).unwrap().len() > 100,
            "{ext}: the closed file is complete under its temporary name"
        );
        finalized.commit().unwrap();
        assert!(std::fs::read(&path).unwrap().len() > 100, "{ext}");
        assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");

        std::fs::write(&path, b"previous").unwrap();
        drop(write(&path));
        assert_eq!(std::fs::read(&path).unwrap(), b"previous", "{ext}");
        assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");
        std::fs::remove_file(&path).unwrap();
    }
}

/// An existing path is refused, staged and in place, with `Exists`, nothing written and the
/// file untouched; `replace` puts the new file over it.
#[test]
fn an_existing_path_is_refused_unless_replaced() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        for in_place in [false, true] {
            let path = dir.path().join(format!("take.{ext}"));
            std::fs::write(&path, b"previous").unwrap();
            let spec = spec(format, 2);
            let options = || {
                let options = EncoderOptions::new(&spec).file(&path);
                if in_place {
                    options.in_place()
                } else {
                    options
                }
            };
            let err = options().create().map(drop).expect_err("it exists");
            assert!(
                matches!(&err, EncodeError::Exists { path: p } if *p == path),
                "{ext} in_place={in_place}: {err}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"previous", "{ext}");
            assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");

            let mut encoder = options().replace().create().unwrap();
            encoder
                .write(Interleaved::new(&saw(2, 500), spec.layout()).unwrap())
                .unwrap();
            encoder.finish().unwrap();
            assert!(std::fs::read(&path).unwrap().len() > 100, "{ext}");
            assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");
            std::fs::remove_file(&path).unwrap();
        }
    }
}

/// A staged file whose path is taken after the check loses at the commit: the finished file
/// is kept and named, and the path holds what was put there.
#[test]
fn a_staged_commit_that_loses_a_race_keeps_its_file() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let spec = spec(format, 2);
        let mut encoder = EncoderOptions::new(&spec).file(&path).create().unwrap();
        encoder
            .write(Interleaved::new(&saw(2, 2_000), spec.layout()).unwrap())
            .unwrap();
        std::fs::write(&path, b"raced").unwrap();
        let Err(EncodeError::Commit(e)) = encoder.finish() else {
            panic!("{ext}: the commit refuses");
        };
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "{ext}");
        assert!(std::fs::metadata(e.kept()).unwrap().len() > 100, "{ext}");
        assert_eq!(std::fs::read(&path).unwrap(), b"raced", "{ext}");
        std::fs::remove_file(e.kept()).unwrap();
        std::fs::remove_file(&path).unwrap();
    }
}

/// An in-place file is at its path while it is written, `finish` leaves it whole, and a
/// failed take kept with `finish_keeping` is there too.
#[test]
fn an_in_place_file_is_at_its_path_during_the_write() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let spec = spec(format, 2);
        let mut encoder = EncoderOptions::new(&spec)
            .file(&path)
            .in_place()
            .create()
            .unwrap();
        assert!(path.exists(), "{ext}: there from the start");
        assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");
        encoder
            .write(Interleaved::new(&saw(2, 5_000), spec.layout()).unwrap())
            .unwrap();
        encoder.finish().unwrap();
        assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");
        assert!(std::fs::metadata(&path).unwrap().len() > 100, "{ext}");
        std::fs::remove_file(&path).unwrap();
    }
}

/// A failed in-place take finished keeping stays at its path, with a header that describes
/// the frames written; the error is the write's.
#[cfg(feature = "encode-wav")]
#[test]
fn an_in_place_failure_is_kept_at_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take.wav");
    let spec = spec(Format::Wav(crate::Pcm::Float(FloatWidth::F32)), 2);
    let mut encoder = EncoderOptions::new(&spec)
        .file(&path)
        .in_place()
        .create()
        .unwrap();
    encoder
        .write(Interleaved::new(&saw(2, 3_000), spec.layout()).unwrap())
        .unwrap();
    assert!(
        encoder
            .write(Interleaved::new(&[0.0f32; 3], ChannelLayout::from(3u8)).unwrap())
            .is_err()
    );
    let Err(Kept::Unfinished { source, unfinished }) = encoder.finish_keeping() else {
        panic!("not kept unfinished");
    };
    assert!(
        matches!(source, EncodeError::BlockWidth { .. }),
        "{source:?}"
    );
    assert_eq!(unfinished.path, path);
    assert!(unfinished.header_describes_frames);
    assert_eq!(crate::read_wave::<f32>(&path).unwrap().len().get(), 3_000);
}

/// An in-place WAV dropped without `finish` (a pump thread that panicked) is a readable file:
/// the writer back-patches the header on drop.
#[cfg(feature = "encode-wav")]
#[test]
fn a_dropped_in_place_encoder_leaves_a_wav_whose_header_describes_the_frames() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dropped.wav");
    let spec = spec(
        Format::Wav(crate::Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither())),
        2,
    );
    let mut encoder = EncoderOptions::new(&spec)
        .file(&path)
        .in_place()
        .create()
        .unwrap();
    encoder
        .write(Interleaved::new(&[0.25f32; 20], spec.layout()).unwrap())
        .unwrap();
    drop(encoder);
    assert_eq!(hound::WavReader::open(&path).unwrap().len(), 20);
}

/// A WAV or AIFF writer back-patches its sizes every 2 s of audio: an in-place encoder that is
/// leaked, as in a crash, leaves a header describing the frames up to the first block boundary
/// at or past 2 s, and the frames written after it are not yet in the header.
#[cfg(any(feature = "encode-wav", feature = "encode-aiff"))]
#[test]
fn a_leaked_in_place_encoder_leaves_a_header_refreshed_every_two_seconds_of_audio() {
    // 2 s at 8 kHz is 16 000 frames; 1 000-frame blocks (under the encoder's 1 024-frame chunk) reach
    // it at the sixteenth, and four more follow.
    const BLOCK: usize = 1_000;
    const REFRESHED: usize = 16 * BLOCK;
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats().into_iter().filter(|(ext, _)| *ext != "flac") {
        let path = dir.path().join(format!("crash.{ext}"));
        let spec = at_rate(format, 8_000.0);
        let mut encoder = EncoderOptions::new(&spec)
            .file(&path)
            .in_place()
            .create()
            .unwrap();
        let block = saw(2, BLOCK);
        for _ in 0..20 {
            encoder
                .write(Interleaved::new(&block, spec.layout()).unwrap())
                .unwrap();
        }
        std::mem::forget(encoder);
        let bytes = std::fs::read(&path).unwrap();
        let frames = match ext {
            "wav" => {
                // The `data` size of the 44-byte header, in bytes of 16-bit stereo.
                let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
                #[cfg(feature = "encode-wav")]
                assert_eq!(
                    hound::WavReader::open(&path).unwrap().duration() as usize,
                    REFRESHED,
                    "{ext}: the frames hound reads"
                );
                data / 4
            }
            _ => {
                // COMM's frame count, and the SSND size: 8 bytes of offset and block size
                // and the frames.
                assert_eq!(&bytes[12..16], b"COMM", "{ext}");
                assert_eq!(&bytes[38..42], b"SSND", "{ext}");
                let comm = u32::from_be_bytes(bytes[22..26].try_into().unwrap()) as usize;
                let ssnd = u32::from_be_bytes(bytes[42..46].try_into().unwrap()) as usize;
                assert_eq!(ssnd, 8 + comm * 4, "{ext}: the SSND size");
                comm
            }
        };
        assert_eq!(frames, REFRESHED, "{ext}: the frames in the header");
        assert!(
            bytes.len() > 44 + REFRESHED * 4,
            "{ext}: the frames after the refresh are on disk, and not in the header"
        );
    }
}

/// A spec the format cannot write leaves no in-place file behind.
#[test]
fn a_spec_that_cannot_be_written_creates_no_in_place_file() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let zero = Spec::of_channels(
            format,
            ChannelSet::default_for(ChannelLayout::from(0u16)),
            SampleRate::SR_44K1,
        );
        EncoderOptions::new(&zero)
            .file(&path)
            .in_place()
            .create()
            .map(drop)
            .expect_err("no channels");
        assert!(listing(dir.path()).is_empty(), "{ext}");
    }
}

/// A rename that fails after `finalize` is a `Commit` error, the complete file
/// is kept where the error says and what is at the path (a directory here) is
/// untouched.
#[test]
fn a_commit_that_cannot_rename_keeps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let spec = spec(format, 2);
        let mut encoder = EncoderOptions::new(&spec).file(&path).create().unwrap();
        encoder
            .write(Interleaved::new(&saw(2, 5_000), spec.layout()).unwrap())
            .unwrap();
        let finalized = encoder.finalize().unwrap();
        std::fs::create_dir(&path).unwrap();
        let err = finalized.commit().expect_err("a directory is in the way");
        let EncodeError::Commit(e) = err else {
            panic!("{ext}: {err}");
        };
        assert!(
            std::fs::metadata(e.kept()).unwrap().len() > 100,
            "{ext}: the kept file is complete"
        );
        assert_eq!(
            listing(dir.path()).len(),
            2,
            "{ext}: the path and the kept file"
        );
        std::fs::remove_file(e.kept()).unwrap();
        std::fs::remove_dir(&path).unwrap();
    }
}

/// Writes `frames` frames of the sawtooth as 32-bit float in `format`, then a
/// block of the wrong width, which fails the encoder, and finishes keeping:
/// returns the directory, what was kept, the samples written and the path.
#[cfg(any(feature = "encode-wav", feature = "encode-aiff"))]
fn kept_unfinished(
    format: Format,
    frames: usize,
) -> (tempfile::TempDir, Unfinished, Vec<f32>, std::path::PathBuf) {
    kept_unfinished_of(format, saw(2, frames))
}

/// As [`kept_unfinished`], for the interleaved stereo `samples`.
#[cfg(any(feature = "encode-wav", feature = "encode-aiff"))]
fn kept_unfinished_of(
    format: Format,
    samples: Vec<f32>,
) -> (tempfile::TempDir, Unfinished, Vec<f32>, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take");
    let spec = spec(format, 2);
    let mut encoder = EncoderOptions::new(&spec).file(&path).create().unwrap();
    encoder
        .write(Interleaved::new(&samples, spec.layout()).unwrap())
        .unwrap();
    let wrong = [0.0f32; 3];
    assert!(
        encoder
            .write(Interleaved::new(&wrong, ChannelLayout::from(3u8)).unwrap())
            .is_err()
    );
    let Err(Kept::Unfinished { unfinished, .. }) = encoder.finish_keeping() else {
        panic!("not kept unfinished");
    };
    (dir, unfinished, samples, path)
}

/// **A WAV kept unfinished decodes to exactly the frames written**, with their
/// values, and the path is untouched. A torn last frame (one stray byte
/// appended) is cut off by the same patch.
#[cfg(all(feature = "encode-wav", feature = "decode-wav"))]
#[test]
fn a_wav_kept_unfinished_holds_exactly_the_frames_written() {
    let format = Format::Wav(crate::Pcm::Float(FloatWidth::F32));
    let (dir, kept, samples, path) = kept_unfinished(format, 5_000);
    assert!(kept.header_describes_frames);
    assert!(!path.exists(), "the path is untouched");
    assert_eq!(listing(dir.path()).len(), 1, "only the kept file");
    let wave = crate::read_wave::<f32>(&kept.path).unwrap();
    let decoded: Vec<f32> = (0..5_000)
        .flat_map(|i| (0..2).map(move |c| (c, i)))
        .map(|(c, i)| wave.channel(c).samples()[i])
        .collect();
    assert_eq!(wave.len().get(), 5_000);
    assert_eq!(decoded, samples);

    let spec = spec(format, 2);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&kept.path)
        .unwrap();
    file.seek(std::io::SeekFrom::End(0)).unwrap();
    file.write_all(&[0x7F]).unwrap();
    patch_header(&spec, &mut file).unwrap();
    drop(file);
    assert_eq!(
        crate::read_wave::<f32>(&kept.path).unwrap().len().get(),
        5_000
    );
}

/// **An AIFF-C kept unfinished decodes to exactly the frames written**, with
/// their values, read by the `aifc` reader (the codec has no AIFF decoder). A
/// torn last frame (three stray bytes) is cut off by the same patch.
#[cfg(feature = "encode-aiff")]
#[test]
fn an_aiff_kept_unfinished_holds_exactly_the_frames_written() {
    let format = Format::Aiff(crate::Pcm::Float(FloatWidth::F32));
    let (_dir, kept, samples, path) = kept_unfinished(format, 5_000);
    assert!(kept.header_describes_frames);
    assert!(!path.exists(), "the path is untouched");
    let read = |path: &std::path::Path| -> (u32, Vec<f32>) {
        let mut reader = aifc::AifcReader::new(std::fs::File::open(path).unwrap()).unwrap();
        let frames = reader.info().comm_num_sample_frames;
        let values = reader
            .samples()
            .unwrap()
            .map(|s| match s.unwrap() {
                aifc::Sample::F32(v) => v,
                other => panic!("not f32: {other:?}"),
            })
            .collect();
        (frames, values)
    };
    assert_eq!(read(&kept.path), (5_000, samples.clone()));

    let spec = spec(format, 2);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&kept.path)
        .unwrap();
    file.seek(std::io::SeekFrom::End(0)).unwrap();
    file.write_all(&[1, 2, 3]).unwrap();
    patch_header(&spec, &mut file).unwrap();
    drop(file);
    assert_eq!(read(&kept.path), (5_000, samples));
}

/// **A file whose encoder failed is kept by `finish_keeping`, with the frames
/// written before the failure, and the path is untouched.** The failure is a
/// block of the wrong width, which every format refuses and the encoder
/// reports at the close.
#[cfg(all(feature = "encode-wav", feature = "decode-wav"))]
#[test]
fn a_failed_encoder_finishes_keeping_the_frames_written_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take");
    let spec = spec(Format::Wav(crate::Pcm::Float(FloatWidth::F32)), 2);
    let mut encoder = EncoderOptions::new(&spec).file(&path).create().unwrap();
    encoder
        .write(Interleaved::new(&saw(2, 3_000), spec.layout()).unwrap())
        .unwrap();
    let wrong = [0.0f32; 3];
    assert!(
        encoder
            .write(Interleaved::new(&wrong, ChannelLayout::from(3u8)).unwrap())
            .is_err()
    );
    let Err(Kept::Unfinished { source, unfinished }) = encoder.finish_keeping() else {
        panic!("not kept unfinished");
    };
    assert!(
        matches!(source, EncodeError::BlockWidth { .. }),
        "{source:?}"
    );
    assert!(unfinished.header_describes_frames);
    assert!(!path.exists(), "the path is untouched");
    assert_eq!(listing(dir.path()).len(), 1, "only the kept file");
    let wave = crate::read_wave::<f32>(&unfinished.path).unwrap();
    assert_eq!(wave.len().get(), 3_000);
}

/// **`finish_keeping` of a whole file puts it in place and leaves nothing
/// beside it**: the keeper it sets is released.
#[test]
fn finish_keeping_a_whole_file_commits_it_and_leaves_no_temporary() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let spec = spec(format, 2);
        let mut encoder = EncoderOptions::new(&spec).file(&path).create().unwrap();
        encoder
            .write(Interleaved::new(&saw(2, 2_000), spec.layout()).unwrap())
            .unwrap();
        encoder.finish_keeping().unwrap();
        assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");
        std::fs::remove_file(&path).unwrap();
    }
}

/// **A plain integer AIFF (no FVER chunk) kept unfinished decodes to exactly
/// the frames written**, with their values, and a torn last frame (three stray
/// bytes) is cut off by the same patch. The samples are whole 16-bit codes over
/// 32768, so the read back codes are those written.
#[cfg(feature = "encode-aiff")]
#[test]
fn an_integer_aiff_kept_unfinished_holds_exactly_the_frames_written() {
    let format = Format::Aiff(crate::Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither()));
    let codes: Vec<i16> = (0..10_000)
        .map(|i| (i * 7 % 2_000 - 1_000) as i16)
        .collect();
    let samples: Vec<f32> = codes.iter().map(|&c| f32::from(c) / 32_768.0).collect();
    let (_dir, kept, _, path) = kept_unfinished_of(format, samples);
    assert!(kept.header_describes_frames);
    assert!(!path.exists(), "the path is untouched");
    let read = |path: &std::path::Path| -> (aifc::FileFormat, u32, Vec<i16>) {
        let mut reader = aifc::AifcReader::new(std::fs::File::open(path).unwrap()).unwrap();
        let info = reader.info();
        let values = reader
            .samples()
            .unwrap()
            .map(|s| match s.unwrap() {
                aifc::Sample::I16(v) => v,
                other => panic!("not i16: {other:?}"),
            })
            .collect();
        (info.file_format, info.comm_num_sample_frames, values)
    };
    assert_eq!(
        read(&kept.path),
        (aifc::FileFormat::Aiff, 5_000, codes.clone())
    );

    let spec = spec(format, 2);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&kept.path)
        .unwrap();
    file.seek(std::io::SeekFrom::End(0)).unwrap();
    file.write_all(&[1, 2, 3]).unwrap();
    patch_header(&spec, &mut file).unwrap();
    drop(file);
    assert_eq!(read(&kept.path), (aifc::FileFormat::Aiff, 5_000, codes));
}

/// The encoder's dither and rounding, through a mono 16-bit or 24-bit WAV.
#[cfg(feature = "encode-wav")]
mod quantize {
    use super::*;
    use tutti_buffers::I24;
    use tutti_units::{Amplitude, Hz};

    const LSB16: f64 = 1.0 / 32768.0;
    const N: usize = 1 << 18;

    fn mono(depth: IntDepth, int: impl FnOnce(IntPcm) -> IntPcm) -> Spec {
        Spec::of_channels(
            Format::Wav(crate::Pcm::Int(int(IntPcm::new(depth)))),
            ChannelSet::default_for(ChannelLayout::MONO),
            SampleRate::SR_48K,
        )
    }

    /// The file `spec` holds for `blocks`, seeded with `seed`, as its 16-bit samples.
    fn written16<S: Sample>(spec: &Spec, seed: u64, blocks: &[&[S]]) -> (Vec<i16>, u64) {
        let mut encoder = EncoderOptions::new(spec)
            .dither_seed(seed)
            .wrap(Cursor::new(Vec::new()))
            .unwrap();
        for block in blocks {
            encoder
                .write(Interleaved::new(block, ChannelLayout::MONO).unwrap())
                .unwrap();
        }
        let clipped = encoder.clipped();
        let bytes = encoder.finish().unwrap().into_inner();
        let samples = bytes[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        (samples, clipped)
    }

    /// The 24-bit samples of the file `spec` holds for `block`.
    fn written24(spec: &Spec, block: &[f32]) -> Vec<i32> {
        let mut encoder = EncoderOptions::new(spec)
            .wrap(Cursor::new(Vec::new()))
            .unwrap();
        encoder
            .write(Interleaved::new(block, ChannelLayout::MONO).unwrap())
            .unwrap();
        let bytes = encoder.finish().unwrap().into_inner();
        bytes[68..]
            .as_chunks::<3>()
            .0
            .iter()
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], 0]) << 8 >> 8)
            .collect()
    }

    fn quiet_sine() -> Vec<f32> {
        tutti_fixtures::signal::sine(
            Hz::new(997.0),
            Amplitude::new(1e-5), // -100 dBFS, a third of an LSB
            SampleRate::SR_48K,
            Samples::new(N),
        )
    }

    /// A TPDF dither of one LSB adds `lsb² / 6` to the rounding's `lsb² / 12`, and the error is
    /// white and independent of a signal this small, so its variance is `lsb² / 4` (Lipshitz,
    /// Wannamaker and Vanderkooy, JAES 1992). The sample variance of `N` values of a bounded
    /// error has standard deviation under `σ² sqrt(2 / N)`; the bound is `1.5 sqrt(2 ln N)` of
    /// that.
    #[test]
    fn a_minus_100_dbfs_sine_at_16_bits_has_the_tpdf_error_variance() {
        let input = quiet_sine();
        let (got, clipped) = written16(&mono(IntDepth::Int16, |p| p), 7, &[&input]);
        assert_eq!(clipped, 0);
        let error: Vec<f64> = got
            .iter()
            .zip(&input)
            .map(|(&y, &x)| (f64::from(y) * LSB16 - f64::from(x)) / LSB16)
            .collect();
        let n = N as f64;
        let sigma2 = 1.0 / 6.0 + 1.0 / 12.0;
        let factor = 1.5 * (2.0 * n.ln()).sqrt();
        let mean = error.iter().sum::<f64>() / n;
        let var = error.iter().map(|e| (e - mean) * (e - mean)).sum::<f64>() / n;
        assert!(mean.abs() < factor * (sigma2 / n).sqrt(), "mean {mean}");
        assert!(
            (var - sigma2).abs() < factor * sigma2 * (2.0 / n).sqrt(),
            "variance {var} vs {sigma2}"
        );
    }

    /// Without dither the error is the deterministic rounding: each value is the file type's
    /// own conversion of the input, which a third of an LSB rounds to silence.
    #[test]
    fn without_dither_the_error_is_the_rounding_of_convert() {
        let input = quiet_sine();
        let (got, _) = written16(&mono(IntDepth::Int16, IntPcm::no_dither), 7, &[&input]);
        let want: Vec<i16> = input.iter().map(|&x| x.convert_to::<i16>()).collect();
        assert_eq!(got, want);
        assert!(got.iter().all(|&y| y == 0));
    }

    /// 24-bit values held as `f32` sit on the 24-bit grid: written undithered they are
    /// bit-exact, and dithering them is what loses that.
    #[test]
    fn on_grid_floats_into_24_bits_are_bit_exact_without_dither() {
        let ints: Vec<i32> = (0..4096)
            .map(|i| (i * 2039) % 16_000_000 - 8_000_000)
            .collect();
        let floats: Vec<f32> = ints
            .iter()
            .map(|&v| I24::of(v).convert_to::<f32>())
            .collect();
        let exact = written24(&mono(IntDepth::Int24, IntPcm::no_dither), &floats);
        assert_eq!(exact, ints);
        let dithered = written24(&mono(IntDepth::Int24, |p| p), &floats);
        assert_ne!(dithered, ints, "dither of one LSB moves some value");
    }

    /// An `i32` block into 16 bits drops resolution, so it is dithered: a value off the 16-bit
    /// grid is no longer its plain rounding, and stays within the dither and rounding of one and
    /// a half LSB of it. An `i16` block into 16 bits drops none: bit-exact, dither on.
    #[test]
    fn a_wider_integer_into_a_narrower_one_is_dithered_and_an_equal_one_is_exact() {
        let wide: Vec<i32> = (0..4096)
            .map(|i| (i * 40_009) % 2_000_000_000 - 1_000_000_000)
            .collect();
        let (got, _) = written16(&mono(IntDepth::Int16, |p| p), 3, &[&wide]);
        let plain: Vec<i16> = wide.iter().map(|&x| x.convert_to::<i16>()).collect();
        assert_ne!(got, plain);
        for (&g, &p) in got.iter().zip(&plain) {
            assert!((i32::from(g) - i32::from(p)).abs() <= 2, "{g} vs {p}");
        }
        let same: Vec<i16> = (0..4096)
            .map(|i| ((i * 31) % 65_536 - 32_768) as i16)
            .collect();
        let (got, _) = written16(&mono(IntDepth::Int16, |p| p), 3, &[&same]);
        assert_eq!(got, same);
    }

    /// One seed gives one file; two seeds give two noise sequences whose correlation is at the
    /// noise floor: for `n` independent values of variance `σ²` the sample correlation has
    /// standard deviation `1 / sqrt(n)`, and the bound is five of them.
    #[test]
    fn a_seed_picks_the_noise() {
        let silence = vec![0.0f32; N];
        let spec = mono(IntDepth::Int16, |p| p);
        let (a, _) = written16(&spec, 1, &[&silence]);
        let (again, _) = written16(&spec, 1, &[&silence]);
        let (b, _) = written16(&spec, 2, &[&silence]);
        assert_eq!(a, again);
        let n = N as f64;
        let dot: f64 = a
            .iter()
            .zip(&b)
            .map(|(&x, &y)| f64::from(x) * f64::from(y))
            .sum();
        let power = |s: &[i16]| s.iter().map(|&x| f64::from(x) * f64::from(x)).sum::<f64>();
        let correlation = dot / (power(&a) * power(&b)).sqrt();
        assert!(
            correlation.abs() < 5.0 / n.sqrt(),
            "correlation {correlation}"
        );
    }

    /// The noise continues across writes: blocks cut anywhere give the file of one block.
    #[test]
    fn the_noise_does_not_depend_on_how_the_blocks_are_cut() {
        let input = quiet_sine();
        let spec = mono(IntDepth::Int16, |p| p);
        let (whole, _) = written16(&spec, 5, &[&input]);
        let (cut, _) = written16(
            &spec,
            5,
            &[&input[..700], &input[700..3000], &input[3000..]],
        );
        assert_eq!(whole, cut);
    }

    /// A block at 2.0 into 16 bits clips every sample, and the encoder counts each.
    #[test]
    fn a_block_past_full_scale_counts_every_sample_it_clipped() {
        let loud = vec![2.0f32; 3000];
        let (got, clipped) = written16(&mono(IntDepth::Int16, |p| p), 1, &[&loud]);
        assert_eq!(clipped, 3000);
        assert!(got.iter().all(|&y| y == i16::MAX));
    }
}
