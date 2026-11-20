use super::*;
use tutti_units::SampleRate;
#[cfg(all(feature = "encode-wav", feature = "encode-flac", decode))]
use {
    crate::{FlacLevel, IntDepth},
    tutti_buffers::Interleaved,
    tutti_buffers::{ChannelLayout, ChannelTopology},
};

/// `frames` frames over `topology` whose sample at `(c, i)` is
/// `((i * 7 + c * 3) % 400 - 200) / 256`: a ramp per channel that wraps, and
/// exact at 16 bits (a whole multiple of 128 / 32768).
#[cfg(all(feature = "encode-wav", feature = "encode-flac", decode))]
fn ramp_wave(topology: ChannelTopology, rate: f64, frames: usize) -> Wave {
    let mut wave = Wave::with_topology(topology, SampleRate::of(rate));
    let width = wave.channels();
    for i in 0..frames {
        let frame: Vec<f32> = (0..width)
            .map(|c| (((i * 7 + c * 3) % 400) as f32 - 200.0) / 256.0)
            .collect();
        wave.append(Interleaved::new(&frame, wave.layout()).unwrap())
            .unwrap();
    }
    wave
}

/// Writing a wave and reading the file back gives the same samples, rate and
/// topology: exactly for WAV's float format, and to within one 16-bit step
/// (the writer scales by 32767 and the reader by 32768) for the integer ones.
#[cfg(all(feature = "encode-wav", feature = "encode-flac", decode))]
#[test]
fn a_written_wave_reads_back_as_itself() {
    use crate::{Pcm, read_wave};
    let dir = tempfile::tempdir().unwrap();
    let stereo = || ChannelTopology::assumed(ChannelLayout::STEREO);
    let cases = [
        ("stereo.wav", Format::Wav(Pcm::Int16), stereo()),
        ("float.wav", Format::Wav(Pcm::Float32), stereo()),
        (
            "quad.wav",
            Format::Wav(Pcm::Int16),
            tutti_buffers::Bed::Quad.topology(),
        ),
        (
            "stereo.flac",
            Format::Flac {
                depth: IntDepth::Int16,
                level: FlacLevel::Standard,
            },
            stereo(),
        ),
    ];
    for (name, format, topology) in cases {
        // Past one write chunk, so the chunking is exercised too.
        let wave = ramp_wave(topology, 44_100.0, 20_000);
        let path = dir.path().join(name);
        write_wave(&wave, &path, format).unwrap();
        let back = read_wave::<f32>(&path).unwrap();
        assert_eq!(back.sample_rate(), wave.sample_rate(), "{name}: rate");
        assert_eq!(back.topology(), wave.topology(), "{name}: topology");
        assert_eq!(back.len(), wave.len(), "{name}: length");
        let step = if format == Format::Wav(Pcm::Float32) {
            0.0
        } else {
            1.0 / 32_768.0
        };
        for c in 0..wave.channels() {
            let pairs = back.channel(c).iter().zip(wave.channel(c));
            for (i, (&got, &want)) in pairs.enumerate() {
                assert!(
                    (got - want).abs() <= step,
                    "{name}: channel {c}, frame {i}: {got} for {want}"
                );
            }
        }
    }
}

/// A wave of 16-bit samples is written at 16 bits and reads back as the same
/// integers to within the writer's own step (it scales by 32767 and the
/// reader by 32768), whatever the format it is held in.
#[test]
fn an_integer_wave_writes_the_integers_it_holds() {
    use crate::{Pcm, read_wave};
    let samples = [0i16, 16_384, -16_384, 32_767, -32_768, 1];
    let wave = Wave::from_mono(SampleRate::SR_48K, tutti_buffers::Mono::new(&samples));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ints.wav");
    write_wave(&wave, &path, Format::Wav(Pcm::Int16)).unwrap();
    let back = read_wave::<i16>(&path).unwrap();
    assert_eq!(back.len(), wave.len());
    for (&got, &want) in back.channel(0).iter().zip(&samples) {
        assert!(
            (i32::from(got) - i32::from(want)).abs() <= 1,
            "{got} for {want}"
        );
    }
}

/// A path destination replaces an existing file by default, and `create_new`
/// refuses it with `AlreadyExists`, leaving the old bytes and no temporary.
#[test]
fn a_path_replaces_by_default_and_create_new_refuses() {
    use crate::{Error, Pcm};
    let wave = Wave::from_mono(SampleRate::SR_48K, tutti_buffers::Mono::new(&[0.5f32; 8]));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take.wav");
    std::fs::write(&path, b"old").unwrap();
    let refused = write_wave(
        &wave,
        Destination::from(&path).create_new(),
        Format::Wav(Pcm::Float32),
    );
    let Err(Error::Commit(e)) = refused else {
        panic!("an existing file is refused: {refused:?}");
    };
    let kept = e.kept().to_path_buf();
    let kept_bytes = std::fs::read(&kept).unwrap();
    assert_eq!(e.into_source().kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    assert_eq!(&kept_bytes[..4], b"RIFF");
    std::fs::remove_file(&kept).unwrap();
    let whole = dir.path().join("whole.wav");
    write_wave(&wave, &whole, Format::Wav(Pcm::Float32)).unwrap();
    assert_eq!(
        kept_bytes,
        std::fs::read(&whole).unwrap(),
        "the kept file is whole"
    );
    std::fs::remove_file(&whole).unwrap();

    write_wave(&wave, &path, Format::Wav(Pcm::Float32)).unwrap();
    assert_eq!(&std::fs::read(&path).unwrap()[..4], b"RIFF");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    let fresh = dir.path().join("fresh.wav");
    write_wave(
        &wave,
        Destination::from(&fresh).create_new(),
        Format::Wav(Pcm::Float32),
    )
    .unwrap();
    assert_eq!(&std::fs::read(&fresh).unwrap()[..4], b"RIFF");
}

/// A writer that only appends: no `Seek`, and not a `Vec`.
struct AppendOnly(Vec<u8>);

impl std::io::Write for AppendOnly {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A wave of 20 001 16-bit samples: past one write chunk, an odd length.
#[cfg(all(feature = "encode-wav", decode))]
fn long_int_wave() -> (Vec<i16>, Wave<i16>) {
    let samples: Vec<i16> = (0..20_001i32)
        .map(|i| ((i * 7919) % 65_536 - 32_768) as i16)
        .collect();
    let wave = Wave::from_mono(SampleRate::SR_48K, tutti_buffers::Mono::new(&samples));
    (samples, wave)
}

/// A 16-bit integer wave written to memory as 16-bit WAV reads back as the
/// same integers: no scaling is in the path, so the round trip is exact.
#[cfg(all(feature = "encode-wav", decode))]
#[test]
fn an_integer_wave_written_to_a_vec_reads_back_exactly() {
    use crate::{Pcm, read_wave};
    let (samples, wave) = long_int_wave();
    let mut bytes = vec![0xAAu8];
    write_wave(&wave, &mut bytes, Format::Wav(Pcm::Int16)).unwrap();
    assert_eq!(bytes[0], 0xAA, "a Vec is appended to");
    let back = read_wave::<i16>(bytes[1..].to_vec()).unwrap();
    assert!(back.channel(0).iter().eq(samples.iter()));
    assert_eq!(back.sample_rate(), wave.sample_rate());
    assert_eq!(back.topology(), wave.topology());
}

/// The same round trip through a writer that cannot seek.
#[cfg(all(feature = "encode-wav", decode))]
#[test]
fn an_integer_wave_written_to_a_non_seek_writer_reads_back_exactly() {
    use crate::{Pcm, read_wave};
    let (samples, wave) = long_int_wave();
    let mut sink = AppendOnly(Vec::new());
    write_wave(&wave, &mut sink, Format::Wav(Pcm::Int16)).unwrap();
    let back = read_wave::<i16>(sink.0).unwrap();
    assert!(back.channel(0).iter().eq(samples.iter()));
}
