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
