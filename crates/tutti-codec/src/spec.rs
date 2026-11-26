//! What a file is written as: the [`Spec`].

use crate::error::{Result, SpecError};
use crate::format::Format;
use std::num::NonZeroU32;
use tutti_buffers::{ChannelLayout, ChannelSet, ChannelTopology};
use tutti_units::SampleRate;

/// Everything an [`Encoder`](crate::Encoder) needs to start a file.
///
/// The file's channels are one [`ChannelSet`]: its width is
/// [`layout`](Self::layout), and its [`topology`](Self::topology) says which
/// speaker each file channel feeds when the width has one (at most
/// [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) channels); a wider file has none.
/// The fields are private (a [`new`](Self::new), [`of_channels`](Self::of_channels)
/// and getters), and the format
/// carries its own settings, so nothing here is read by a format that cannot use
/// it, except the topology, which WAV (the speaker mask) reads; the other formats
/// ignore it.
///
/// The fields are not written outside the crate:
///
/// ```compile_fail,E0451
/// # use tutti_codec::{Format, Pcm, Spec};
/// # use tutti_units::SampleRate;
/// # use tutti_buffers::ChannelTopology;
/// let _ = Spec {
///     format: Format::Wav(Pcm::default()),
///     topology: tutti_buffers::Bed::Quad.topology(),
///     rate: SampleRate::SR_48K,
/// };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    format: Format,
    channels: ChannelSet,
    rate: SampleRate,
}

impl Spec {
    /// A file of `format` whose channels feed the speakers of `topology`, at
    /// `rate`. The encoder converts nothing: the samples handed to it are
    /// already at this rate.
    #[must_use]
    pub fn new(format: Format, topology: ChannelTopology, rate: SampleRate) -> Self {
        Self::of_channels(format, ChannelSet::from(topology), rate)
    }

    /// A file of `format` whose channels are `channels`, at `rate`.
    #[must_use]
    pub fn of_channels(format: Format, channels: ChannelSet, rate: SampleRate) -> Self {
        Self {
            format,
            channels,
            rate,
        }
    }

    /// The container and its settings.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// Which speaker each file channel feeds, for the WAV speaker mask; `None`
    /// for a file wider than a topology holds.
    #[must_use]
    pub fn topology(&self) -> Option<&ChannelTopology> {
        self.channels.topology()
    }

    /// The file's channels: its width and, when it has one, its topology.
    #[must_use]
    pub fn channel_set(&self) -> &ChannelSet {
        &self.channels
    }

    /// The width of the file. Blocks written to the encoder have this width.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.channels.width()
    }

    /// The rate the file declares.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The rate as the integer a WAV or FLAC header holds.
    ///
    /// A header of whole Hz cannot state a fractional rate, and rounding it
    /// would play the file at another pitch, so it is refused. (AIFF states an
    /// extended float and takes the rate as it is.)
    ///
    /// # Errors
    ///
    /// [`SpecError::SampleRate`] when the rate is below 1 Hz, exceeds
    /// `u32::MAX` Hz, or is not a whole number of Hz.
    pub fn header_rate(&self) -> Result<NonZeroU32> {
        let hz = self.header_hz()?;
        if hz.fract() == 0.0 {
            // In range and whole: the cast is exact.
            if let Some(rate) = NonZeroU32::new(hz as u32) {
                return Ok(rate);
            }
        }
        Err(SpecError::SampleRate(hz).into())
    }

    /// The rate as the header states it, in Hz, when it fits a header at all.
    ///
    /// # Errors
    ///
    /// [`SpecError::SampleRate`] when the rate is below 1 Hz or exceeds
    /// `u32::MAX` Hz.
    pub(crate) fn header_hz(&self) -> Result<f64> {
        let hz = self.rate.get();
        if (1.0..=f64::from(u32::MAX)).contains(&hz) {
            Ok(hz)
        } else {
            Err(SpecError::SampleRate(hz).into())
        }
    }

    /// The width as a non-zero channel count.
    ///
    /// # Errors
    ///
    /// [`SpecError::ZeroChannels`] for a zero width.
    pub fn channels(&self) -> Result<u16> {
        match self.layout().count() {
            0 => Err(SpecError::ZeroChannels.into()),
            n => Ok(n),
        }
    }
}
