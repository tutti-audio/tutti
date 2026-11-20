use super::*;
use crate::test_sink::Full;
use crate::{Error, SpecError};
use std::io::{Cursor, Seek, Write};
use tutti_units::SampleRate;

/// Every format this build writes, at 16 bits where it has a width, with the
/// name its file would carry.
#[allow(clippy::vec_init_then_push)]
fn formats() -> Vec<(&'static str, Format)> {
    #[allow(unused_mut)]
    let mut all: Vec<(&'static str, Format)> = Vec::new();
    #[cfg(feature = "encode-wav")]
    all.push(("wav", Format::Wav(crate::Pcm::Int16)));
    #[cfg(feature = "encode-aiff")]
    all.push(("aiff", Format::Aiff(crate::Pcm::Int16)));
    #[cfg(feature = "encode-flac")]
    all.push((
        "flac",
        Format::Flac {
            depth: crate::IntDepth::Int16,
            level: crate::FlacLevel::default(),
        },
    ));
    #[cfg(feature = "encode-vorbis")]
    all.push((
        "ogg",
        Format::Vorbis {
            quality: crate::Quality::default(),
        },
    ));
    all
}

fn spec(format: Format, channels: u16) -> Spec {
    Spec::of_width(format, ChannelLayout::from(channels), SampleRate::SR_44K1)
}

fn at_rate(format: Format, rate: f64) -> Spec {
    Spec::of_width(format, ChannelLayout::STEREO, SampleRate::of(rate))
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

/// A file written with `create_file` appears at its path only when `finish`
/// moves it there, and replaces what was there.
#[test]
fn a_file_appears_at_its_path_only_when_finished() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        std::fs::write(&path, b"previous").unwrap();
        let spec = spec(format, 2);
        let mut encoder = FileEncoder::create(&spec, &path).unwrap();
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

/// An encoder dropped before `finish`, by `abort` or after an error, removes
/// its file and leaves what was at the path alone.
#[test]
fn an_aborted_encoder_removes_its_file_and_leaves_the_path_alone() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        std::fs::write(&path, b"previous").unwrap();
        let spec = spec(format, 2);

        let mut encoder = FileEncoder::create(&spec, &path).unwrap();
        encoder
            .write(Interleaved::new(&saw(2, 5_000), spec.layout()).unwrap())
            .unwrap();
        encoder.abort();

        let mut dropped = FileEncoder::create(&spec, &path).unwrap();
        dropped
            .write(Interleaved::new(&saw(2, 5_000), spec.layout()).unwrap())
            .unwrap();
        drop(dropped);

        assert_eq!(std::fs::read(&path).unwrap(), b"previous", "{ext}");
        assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");
        std::fs::remove_file(&path).unwrap();
    }
}

/// A write that fails mid-take, and the abort that follows, leave the path as
/// it was and no temporary. The failure is the width check, which every format
/// shares; the full-disk case is below.
#[test]
fn a_refused_block_leaves_nothing_behind_once_dropped() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let spec = spec(format, 2);
        let mut encoder = FileEncoder::create(&spec, &path).unwrap();
        let err = encoder
            .write(Interleaved::new(&saw(1, 100), ChannelLayout::MONO).unwrap())
            .expect_err("a mono block is not stereo");
        assert!(
            matches!(
                err,
                Error::BlockWidth { expected, got }
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
        let err = FileEncoder::create(&zero_width, &path).expect_err("no channels");
        assert!(
            matches!(err, Error::Spec(SpecError::ZeroChannels)),
            "{ext}: {err}"
        );
        let zero_rate = at_rate(format, 0.4);
        let err = FileEncoder::create(&zero_rate, &path).expect_err("no rate");
        assert!(
            matches!(err, Error::Spec(SpecError::SampleRate(_))),
            "{ext}: {err}"
        );
        assert!(
            listing(dir.path()).is_empty(),
            "{ext}: a temporary was left"
        );
    }
}

/// Widths past a container's header are refused as spec errors where the
/// container counts them: AIFF in an `i16`, Ogg Vorbis in a `u8`.
#[test]
fn a_width_past_the_containers_header_is_refused() {
    #[cfg(feature = "encode-aiff")]
    {
        let s = spec(Format::Aiff(crate::Pcm::Int16), 40_000);
        let err = Encoder::create(&s, Cursor::new(Vec::new())).expect_err("past i16");
        assert!(
            matches!(
                err,
                Error::Spec(SpecError::TooManyChannels {
                    channels: 40_000,
                    max: 32_767
                })
            ),
            "{err}"
        );
    }
    #[cfg(feature = "encode-vorbis")]
    {
        let s = spec(
            Format::Vorbis {
                quality: crate::Quality::default(),
            },
            257,
        );
        let err = Encoder::create(&s, Cursor::new(Vec::new())).expect_err("past u8");
        assert!(
            matches!(
                err,
                Error::Spec(SpecError::TooManyChannels {
                    channels: 257,
                    max: 255
                })
            ),
            "{err}"
        );
    }
}

/// A take that outgrows its sink never reports success: some write, or at
/// the latest `finish`, fails, and the failure is the sink's own.
///
/// The sink holds the headers (Ogg's are a few KiB) and little more. WAV, AIFF and FLAC fail on the
/// write that reaches the sink; Ogg Vorbis encodes on a thread, so its
/// failure may come with the next write or `finish`.
#[test]
fn a_take_that_outgrows_its_sink_fails_by_finish_at_the_latest() {
    for (ext, format) in formats() {
        let s = spec(format, 2);
        let mut encoder = Encoder::create(&s, Full::holding(6_000)).unwrap();
        let mut failed: Option<Error> = None;
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
            Error::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::StorageFull, "{ext}"),
            // Codecs that wrap the sink's error in their own report.
            Error::Encoding { detail, .. } => {
                assert!(!detail.is_empty(), "{ext}: the report names the cause")
            }
            other => panic!("{ext}: expected the sink's failure, got {other}"),
        }
    }
}

/// An encoder is `Send`, so a recording can move it to its pump thread.
///
/// The property is a compile-time one:
/// holding the `VorbisEncoder` in `VorbisWriter` (built on the caller's thread)
/// makes this stop compiling.
#[test]
fn an_encoder_can_move_to_another_thread() {
    fn is_send<T: Send>() {}
    is_send::<Encoder<Cursor<Vec<u8>>>>();
    is_send::<Encoder<std::io::BufWriter<std::fs::File>>>();
    is_send::<FileEncoder>();
}

/// A WAV file that outgrows its limit mid-take (after the up-front check
/// passed) leaves no file behind and the path untouched, and `finish` after the
/// failed write removes the staged file and returns the first error instead of
/// moving a partial take into place.
#[cfg(feature = "encode-wav")]
#[test]
fn a_file_that_crosses_its_limit_is_never_put_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take.wav");
    std::fs::write(&path, b"previous").unwrap();
    let spec = spec(Format::Wav(crate::Pcm::Int16), 2);

    // Dropped after the failed write: nothing left.
    let mut encoder = FileEncoder::create(&spec, &path).unwrap().limit_frames(6);
    let err = encoder
        .write(Interleaved::new(&saw(2, 8), spec.layout()).unwrap())
        .expect_err("8 frames cross a limit of 6");
    assert!(
        matches!(err, Error::WavTooLarge { limit_frames: 6 }),
        "{err}"
    );
    drop(encoder);
    assert_eq!(std::fs::read(&path).unwrap(), b"previous");
    assert_eq!(listing(dir.path()), ["take.wav"]);

    // Finished after the failed write: the first error, nothing put in place.
    let mut encoder = FileEncoder::create(&spec, &path).unwrap().limit_frames(6);
    let first = encoder
        .write(Interleaved::new(&saw(2, 8), spec.layout()).unwrap())
        .unwrap_err();
    let _ = encoder.write(Interleaved::new(&saw(2, 1), spec.layout()).unwrap());
    let err = encoder.finish().expect_err("a partial take is not a file");
    assert_eq!(err.to_string(), first.to_string());
    assert_eq!(std::fs::read(&path).unwrap(), b"previous");
    assert_eq!(listing(dir.path()), ["take.wav"]);
}

/// A planned length past the format's limit is refused by the codec's own
/// error before the temporary exists, and a length at the limit is not.
#[cfg(feature = "encode-wav")]
#[test]
fn a_planned_length_past_the_limit_is_refused_before_the_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take.wav");
    std::fs::write(&path, b"previous").unwrap();
    let format = Format::Wav(crate::Pcm::Int16);
    let spec = spec(format, 2);
    let limit = format.max_frames(spec.layout()).unwrap();

    let err = FileEncoder::create_planned(&spec, &path, Samples::new(limit as usize + 1))
        .map(drop)
        .expect_err("one frame past the limit");
    assert!(
        matches!(err, Error::WavTooLarge { limit_frames } if limit_frames == limit),
        "{err}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"previous");
    assert_eq!(listing(dir.path()), ["take.wav"], "no temporary was made");

    FileEncoder::create_planned(&spec, &path, Samples::new(limit as usize))
        .expect("the limit itself fits");
}

/// A plain sink keeps today's behaviour: `finish` after the limit closes what
/// was written, a readable prefix, and hands the sink back.
#[cfg(feature = "encode-wav")]
#[test]
fn a_plain_sink_keeps_its_prefix_after_a_failed_write() {
    let spec = spec(Format::Wav(crate::Pcm::Int16), 2);
    let mut encoder = Encoder::create(&spec, Cursor::new(Vec::new()))
        .unwrap()
        .limit_frames(6);
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
            let mut encoder = Encoder::create(&s, std::io::BufWriter::new(device)).unwrap();
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
            let err = Encoder::create(&s, Cursor::new(Vec::new())).expect_err("no such rate");
            assert!(
                matches!(err, Error::Spec(SpecError::SampleRate(_))),
                "{ext} {rate}: {err}"
            );
        }
    }
}

/// `finalize` closes a file and leaves it beside its path: the path holds what
/// it held, the closed file is a complete one under its temporary name, and
/// `commit` is what moves it. Dropping the `Finalized` instead removes the
/// temporary and leaves the path alone.
#[test]
fn a_finalized_file_waits_for_its_commit() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        std::fs::write(&path, b"previous").unwrap();
        let spec = spec(format, 2);

        let write = |path: &std::path::Path| {
            let mut encoder = FileEncoder::create(&spec, path).unwrap();
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

/// A rename that fails after `finalize` is a `Commit` error, the complete file
/// is kept where the error says and what is at the path (a directory here) is
/// untouched.
#[test]
fn a_commit_that_cannot_rename_keeps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    for (ext, format) in formats() {
        let path = dir.path().join(format!("take.{ext}"));
        let spec = spec(format, 2);
        let mut encoder = FileEncoder::create(&spec, &path).unwrap();
        encoder
            .write(Interleaved::new(&saw(2, 5_000), spec.layout()).unwrap())
            .unwrap();
        let finalized = encoder.finalize().unwrap();
        std::fs::create_dir(&path).unwrap();
        let err = finalized.commit().expect_err("a directory is in the way");
        let Error::Commit(e) = err else {
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take");
    let spec = spec(format, 2);
    let samples = saw(2, frames);
    let mut encoder = FileEncoder::create(&spec, &path).unwrap();
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
    let format = Format::Wav(crate::Pcm::Float32);
    let (dir, kept, samples, path) = kept_unfinished(format, 5_000);
    assert!(kept.header_patched);
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
    let format = Format::Aiff(crate::Pcm::Float32);
    let (_dir, kept, samples, path) = kept_unfinished(format, 5_000);
    assert!(kept.header_patched);
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
    let spec = spec(Format::Wav(crate::Pcm::Float32), 2);
    let mut encoder = FileEncoder::create(&spec, &path).unwrap();
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
    assert!(matches!(source, Error::BlockWidth { .. }), "{source:?}");
    assert!(unfinished.header_patched);
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
        let mut encoder = FileEncoder::create(&spec, &path).unwrap();
        encoder
            .write(Interleaved::new(&saw(2, 2_000), spec.layout()).unwrap())
            .unwrap();
        encoder.finish_keeping().unwrap();
        assert_eq!(listing(dir.path()), [format!("take.{ext}")], "{ext}");
        std::fs::remove_file(&path).unwrap();
    }
}
