//! The output format pick, over constructed device capabilities.

use super::*;

fn range(
    channels: u16,
    min: u32,
    max: u32,
    format: cpal::SampleFormat,
) -> cpal::SupportedStreamConfigRange {
    cpal::SupportedStreamConfigRange::new(
        channels,
        min,
        max,
        cpal::SupportedBufferSize::Unknown,
        format,
    )
}

/// A stereo device whose default is F32 at 48 kHz.
fn default_config() -> cpal::SupportedStreamConfig {
    range(2, 48_000, 48_000, cpal::SampleFormat::F32).with_sample_rate(48_000)
}

fn choose(
    ranges: &[cpal::SupportedStreamConfigRange],
    wanted: Option<SampleFormat>,
) -> Result<cpal::SupportedStreamConfig> {
    choose_output_config(ranges.iter().cloned(), default_config(), wanted, "Card")
}

/// **With no format chosen the answer is the device's default config**, whatever
/// else it offers.
#[test]
fn an_unset_output_format_is_the_default_config() {
    let chosen = choose(&[range(2, 44_100, 96_000, cpal::SampleFormat::I16)], None).unwrap();
    assert_eq!(chosen, default_config());
}

/// **A chosen format picks the range that offers it, at the default's rate and
/// width.**
#[test]
fn a_chosen_output_format_picks_that_format_at_the_default_rate_and_width() {
    let chosen = choose(
        &[
            range(2, 48_000, 48_000, cpal::SampleFormat::F32),
            range(6, 44_100, 96_000, cpal::SampleFormat::I16),
            range(2, 44_100, 96_000, cpal::SampleFormat::I16),
        ],
        Some(SampleFormat::I16),
    )
    .unwrap();
    assert_eq!(
        (
            chosen.channels(),
            chosen.sample_rate(),
            chosen.sample_format()
        ),
        (2, 48_000, cpal::SampleFormat::I16)
    );
}

/// **An I16 choice opens the U16 type on a device that offers only that, and the
/// signed type when both exist.**
#[test]
fn a_chosen_i16_output_opens_u16_only_when_nothing_signed_is_offered() {
    let only = choose(
        &[range(2, 44_100, 96_000, cpal::SampleFormat::U16)],
        Some(SampleFormat::I16),
    )
    .unwrap();
    assert_eq!(only.sample_format(), cpal::SampleFormat::U16);
    let both = choose(
        &[
            range(2, 44_100, 96_000, cpal::SampleFormat::U16),
            range(2, 44_100, 96_000, cpal::SampleFormat::I16),
        ],
        Some(SampleFormat::I16),
    )
    .unwrap();
    assert_eq!(both.sample_format(), cpal::SampleFormat::I16);
}

/// **A chosen format the device lacks is the named error**, with the device and
/// the format; the default config is not a fallback.
#[test]
fn a_chosen_output_format_the_device_lacks_is_a_named_error() {
    let err = choose(
        &[range(2, 44_100, 96_000, cpal::SampleFormat::F32)],
        Some(SampleFormat::I32),
    )
    .expect_err("only F32 is offered");
    assert!(
        matches!(
            &err,
            DeviceError::OutputFormatUnavailable { device_name, format: SampleFormat::I32 }
                if device_name == "Card"
        ),
        "{err:?}"
    );
}

/// **A format offered only at another width or rate is not offered**: the
/// stream keeps the default's rate and width.
#[test]
fn a_format_offered_only_at_another_rate_is_not_offered() {
    let err = choose(
        &[
            range(2, 44_100, 44_100, cpal::SampleFormat::I16),
            range(1, 48_000, 48_000, cpal::SampleFormat::I16),
        ],
        Some(SampleFormat::I16),
    )
    .expect_err("no I16 at 48 kHz stereo");
    assert!(
        matches!(err, DeviceError::OutputFormatUnavailable { .. }),
        "{err:?}"
    );
}
