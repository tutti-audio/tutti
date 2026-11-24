//! What a device offers, as plain data.

use std::ops::RangeInclusive;

use tutti_buffers::{ChannelLayout, SampleFormat};
use tutti_units::SampleRate;

use crate::driver_seam::DeviceFormat;

/// One configuration a device offers for a direction: a width, a range of
/// rates and a sample format.
///
/// An offset-binary device format is listed as the signed format of its width,
/// the one an app asks for ([`MicConfig::sample_format`](crate::MicConfig),
/// [`OutputConfig::sample_format`](crate::OutputConfig::sample_format)), so a
/// device offering both `U16` and `I16` lists one row.
#[derive(Debug, Clone, PartialEq)]
pub struct SupportedConfig {
    /// How many channels the device runs.
    pub channels: ChannelLayout,
    /// The rates it runs at, both ends included.
    pub rates: RangeInclusive<SampleRate>,
    /// The sample format a stream opens at.
    pub format: SampleFormat,
}

impl SupportedConfig {
    /// The rows `ranges` make, in order, each once. A range at a format the
    /// driver does not build, or with a rate of 0 Hz, is not offered.
    pub(crate) fn list(
        ranges: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    ) -> Vec<Self> {
        let mut rows: Vec<Self> = Vec::new();
        for range in ranges {
            let (Ok(format), Some(min), Some(max)) = (
                DeviceFormat::try_from(range.sample_format()).map(DeviceFormat::spec),
                SampleRate::from_hz(range.min_sample_rate()),
                SampleRate::from_hz(range.max_sample_rate()),
            ) else {
                continue;
            };
            let row = Self {
                channels: ChannelLayout::from(range.channels()),
                rates: min..=max,
                format,
            };
            if !rows.contains(&row) {
                rows.push(row);
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
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

    /// **An offset-binary format and its signed twin are one row**: the app
    /// chooses a `SampleFormat`, and `U16` is how a device offers `I16`.
    #[test]
    fn the_listing_collapses_u16_and_i16_into_one_row() {
        let rows = SupportedConfig::list(
            [
                range(2, 44_100, 48_000, cpal::SampleFormat::U16),
                range(2, 44_100, 48_000, cpal::SampleFormat::I16),
            ]
            .into_iter(),
        );
        assert_eq!(
            rows,
            [SupportedConfig {
                channels: ChannelLayout::STEREO,
                rates: SampleRate::SR_44K1..=SampleRate::SR_48K,
                format: SampleFormat::I16,
            }]
        );
    }

    /// **Rows that differ in width, rates or format stay apart.**
    #[test]
    fn rows_that_differ_are_kept() {
        let rows = SupportedConfig::list(
            [
                range(2, 44_100, 48_000, cpal::SampleFormat::F32),
                range(1, 44_100, 48_000, cpal::SampleFormat::F32),
                range(2, 44_100, 96_000, cpal::SampleFormat::F32),
                range(2, 44_100, 48_000, cpal::SampleFormat::F64),
            ]
            .into_iter(),
        );
        assert_eq!(rows.len(), 4);
    }
}
