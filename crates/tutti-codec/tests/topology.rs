//! The speaker each decoded channel feeds, from what a container states.
//!
//! The expected topologies are the specs' own: `ksmedia.h`'s `SPEAKER_*` bits
//! for WAVE (the mask is hand-written into the header here, not produced by
//! our writer), the Vorbis I and FLAC channel tables for those.

use std::path::PathBuf;

use tutti_buffers::{ChannelLayout, ChannelTopology, Speaker::*};
use tutti_codec::{DecodeError, FileIn, read_wave};

fn asset(name: &str) -> PathBuf {
    tutti_fixtures::asset(&format!("audio/{name}"))
}

/// A float32 `WAVE_FORMAT_EXTENSIBLE` file whose `n`th channel holds the
/// constant `n + 1` (times 0.01) for 4 frames, declaring `mask`.
fn extensible(channels: u16, mask: u32) -> Vec<u8> {
    let frames = 4u32;
    let mut fmt = Vec::new();
    fmt.extend(0xFFFEu16.to_le_bytes());
    fmt.extend(channels.to_le_bytes());
    fmt.extend(48_000u32.to_le_bytes());
    fmt.extend((48_000 * 4 * u32::from(channels)).to_le_bytes());
    fmt.extend((4 * channels).to_le_bytes());
    fmt.extend(32u16.to_le_bytes());
    fmt.extend(22u16.to_le_bytes());
    fmt.extend(32u16.to_le_bytes());
    fmt.extend(mask.to_le_bytes());
    // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    fmt.extend([
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ]);
    let mut data = Vec::new();
    for _ in 0..frames {
        for c in 0..channels {
            data.extend((0.01 * f32::from(c + 1)).to_le_bytes());
        }
    }
    let mut out = b"RIFF".to_vec();
    out.extend((4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend((fmt.len() as u32).to_le_bytes());
    out.extend(&fmt);
    out.extend(b"data");
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(&data);
    out
}

fn topology_of_mask(channels: u16, mask: u32) -> ChannelTopology {
    read_wave::<f32>(extensible(channels, mask))
        .expect("decodes")
        .topology()
        .clone()
}

/// The mask a WAVE header states is the topology, speaker for speaker: quad
/// `0x33` is back, `0x603` side, 5.1 `0x3F` back and `0x60F` side (the engine's
/// own), and the channels stay in file order.
#[test]
fn a_wave_mask_names_each_channels_speaker() {
    assert_eq!(
        topology_of_mask(4, 0x33).positions().unwrap(),
        [FrontLeft, FrontRight, BackLeft, BackRight]
    );
    assert_eq!(
        topology_of_mask(4, 0x603).positions().unwrap(),
        [FrontLeft, FrontRight, SideLeft, SideRight]
    );
    assert_eq!(
        topology_of_mask(6, 0x3F).positions().unwrap(),
        [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight
        ]
    );
    assert_eq!(
        topology_of_mask(6, 0x60F),
        ChannelTopology::assumed(ChannelLayout::from(6u16))
    );
    assert_eq!(
        topology_of_mask(3, 0x7).positions().unwrap(),
        [FrontLeft, FrontRight, FrontCenter]
    );
}

/// A WAVE channel mask names the 18 standard speakers; a bit past them is a
/// mask the container refuses, as unsupported, rather than a channel the file
/// would feed to no speaker.
#[test]
fn a_mask_bit_past_the_standard_speakers_is_refused() {
    assert!(matches!(
        read_wave::<f32>(extensible(2, 0x0200_0001)),
        Err(DecodeError::Unsupported(_))
    ));
}

/// Sample channel `n` is topology entry `n`: the LFE of a 5.1 file is the
/// fourth plane, whatever order the speakers were declared in.
#[test]
fn channel_n_of_the_samples_is_entry_n_of_the_topology() {
    let wave = read_wave::<f32>(extensible(6, 0x3F)).expect("decodes");
    let at = wave.topology().index_of(LowFrequency).unwrap();
    assert_eq!(at, 3);
    assert!((wave.at(at, 0) - 0.04).abs() < 1e-6);
    let centre = wave.topology().index_of(FrontCenter).unwrap();
    assert!((wave.at(centre, 0) - 0.03).abs() < 1e-6);
}

/// The fixtures: a lone channel is the centre (symphonia calls it
/// `FRONT_LEFT`), two are left and right in every container, and the 5.1 file
/// states the back pair. A stream reports what a load does.
#[test]
fn the_fixtures_state_their_speakers() {
    for (name, expected) in [
        ("mono_48k.wav", vec![FrontCenter]),
        ("stereo.flac", vec![FrontLeft, FrontRight]),
        ("stereo.mp3", vec![FrontLeft, FrontRight]),
        ("stereo.ogg", vec![FrontLeft, FrontRight]),
        ("stereo_s16.wav", vec![FrontLeft, FrontRight]),
        (
            "surround_51.wav",
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
            ],
        ),
    ] {
        let path = asset(name);
        let wave = read_wave::<f32>(&path).expect(name);
        assert_eq!(wave.topology().positions().unwrap(), expected, "{name}");
        assert_eq!(wave.channels(), expected.len(), "{name}");
        let stream = FileIn::<f32>::open(&path).expect(name);
        assert_eq!(stream.topology(), wave.topology(), "{name}: stream");
    }
}
