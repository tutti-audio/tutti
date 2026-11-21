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
        Some(OutputQuantum::of(512))
    );
    assert_eq!(
        quantum_for(&Range {
            min: 1024,
            max: 4096
        }),
        Some(OutputQuantum::of(1024))
    );
    assert_eq!(
        quantum_for(&Range { min: 16, max: 256 }),
        Some(OutputQuantum::of(256))
    );
    assert_eq!(quantum_for(&Unknown), None);
    // A range with no usable size has no quantum, and does not panic.
    assert_eq!(quantum_for(&Range { min: 0, max: 0 }), None);
    assert_eq!(
        quantum_for(&Range { min: 0, max: 4096 }),
        Some(OutputQuantum::of(512))
    );
    let spec = OutputSpec::new(SampleRate::SR_48K, ChannelLayout::STEREO, SampleFormat::F32)
        .with_quantum(OutputQuantum::of(480));
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
    let (at, _) = spec_from_supported(&device(MAX_CHANNELS as u16), "test", None).unwrap();
    assert_eq!(at.channels.count() as usize, MAX_CHANNELS);
    let past = spec_from_supported(&device(MAX_CHANNELS as u16 + 1), "test", None);
    assert!(
        matches!(past, Err(DeviceError::InvalidConfig(_))),
        "{past:?}"
    );
}

/// A device whose buffer range is `min..=max`, as its supported config.
fn device_with_range(min: u32, max: u32) -> cpal::SupportedStreamConfig {
    cpal::SupportedStreamConfigRange::new(
        2,
        48_000,
        48_000,
        cpal::SupportedBufferSize::Range { min, max },
        cpal::SampleFormat::F32,
    )
    .with_sample_rate(48_000)
}

/// **A callback size asked for is opened exactly or refused by name.** Inside
/// the device's range the spec carries it, whatever the preferred size would
/// have been; one frame outside, either side, is `QuantumUnavailable` carrying
/// the range, never the nearest size the device offers.
#[test]
fn a_requested_quantum_is_opened_exactly_or_refused_by_name() {
    let device = device_with_range(64, 256);
    for frames in [64, 128, 256] {
        let (spec, _) =
            spec_from_supported(&device, "card", Some(OutputQuantum::of(frames))).unwrap();
        assert_eq!(spec.quantum, Some(OutputQuantum::of(frames)));
    }
    for frames in [63, 257, 4096] {
        let refused = spec_from_supported(&device, "card", Some(OutputQuantum::of(frames)));
        assert!(
            matches!(
                &refused,
                Err(DeviceError::QuantumUnavailable { device_name, quantum, min: 64, max: 256 })
                    if device_name == "card" && quantum.get() == frames
            ),
            "{frames}: {refused:?}"
        );
    }
}

/// A device that reports no range is asked for the size as given; with no
/// request, a device that reports one gets the preferred size.
#[test]
fn an_unknown_range_takes_the_request_as_given() {
    let (spec, _) = spec_from_supported(&device(2), "card", Some(OutputQuantum::of(100))).unwrap();
    assert_eq!(spec.quantum, Some(OutputQuantum::of(100)));
    let (spec, _) = spec_from_supported(&device_with_range(16, 4096), "card", None).unwrap();
    assert_eq!(spec.quantum, Some(PREFERRED_QUANTUM));
}
