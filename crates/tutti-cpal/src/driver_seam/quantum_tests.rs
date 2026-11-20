use super::*;
use tutti_buffers::SampleFormat;

/// A device reporting a buffer range is opened with a fixed callback size:
/// the preferred 512 when it fits, clamped to the range when not, and
/// never past the callback's own buffers; a device reporting none gets
/// its default buffer, and the spec says the size is unknown.
#[test]
fn a_device_range_gives_a_fixed_quantum() {
    use cpal::SupportedBufferSize::{Range, Unknown};
    assert_eq!(
        quantum_for(&Range { min: 64, max: 4096 }),
        Some(MaxBlock::of(Samples::new(512)))
    );
    assert_eq!(
        quantum_for(&Range {
            min: 1024,
            max: 4096
        }),
        Some(MaxBlock::of(Samples::new(1024)))
    );
    assert_eq!(
        quantum_for(&Range { min: 16, max: 256 }),
        Some(MaxBlock::of(Samples::new(256)))
    );
    assert_eq!(quantum_for(&Unknown), None);
    // A range with no usable size has no quantum, and does not panic.
    assert_eq!(quantum_for(&Range { min: 0, max: 0 }), None);
    assert_eq!(
        quantum_for(&Range { min: 0, max: 4096 }),
        Some(MaxBlock::of(Samples::new(512)))
    );
    let spec = OutputSpec::new(SampleRate::SR_48K, ChannelLayout::STEREO, SampleFormat::F32)
        .with_quantum(MaxBlock::of(Samples::new(480)));
    assert!(matches!(
        stream_config(&spec).buffer_size,
        cpal::BufferSize::Fixed(480)
    ));
    let spec = OutputSpec::new(SampleRate::SR_48K, ChannelLayout::STEREO, SampleFormat::F32);
    assert!(matches!(
        stream_config(&spec).buffer_size,
        cpal::BufferSize::Default
    ));
}

/// A device of `channels` channels, as its supported config.
fn device(channels: u16) -> cpal::SupportedStreamConfig {
    cpal::SupportedStreamConfigRange::new(
        channels,
        48_000,
        48_000,
        cpal::SupportedBufferSize::Unknown,
        cpal::SampleFormat::F32,
    )
    .with_sample_rate(48_000)
}

/// A device at the cap opens at its own width; one channel past it is refused
/// by name, never truncated.
#[test]
fn a_device_wider_than_the_cap_is_refused() {
    let (at, _) = spec_from_supported(&device(MAX_CHANNELS as u16)).unwrap();
    assert_eq!(at.channels.count() as usize, MAX_CHANNELS);
    let past = spec_from_supported(&device(MAX_CHANNELS as u16 + 1));
    assert!(
        matches!(past, Err(DeviceError::InvalidConfig(_))),
        "{past:?}"
    );
}
