//! What a file is written as: the [`Spec`].

use crate::error::{Result, SpecError};
use crate::format::Format;
use std::num::NonZeroU32;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_units::SampleRate;

/// Everything an [`Encoder`](crate::Encoder) needs to start a file.
///
/// The speakers are the one statement of the file's width: the
/// [`topology`](Self::topology) says which speaker each file channel feeds, and
/// [`layout`](Self::layout) is its length, so the two cannot disagree. The fields
/// are private (a [`new`](Self::new) and getters), and the format carries its own
/// settings, so nothing here is read by a format that cannot use it, except the
/// topology, which WAV (the speaker mask) reads; the other formats ignore it.
///
/// A spec has no width of its own to disagree with its topology, and its
/// fields are not written outside the crate:
///
/// ```compile_fail,E0451
/// # use tutti_codec::{Format, Pcm, Spec};
/// # use tutti_units::SampleRate;
/// # use tutti_buffers::ChannelTopology;
/// let _ = Spec {
///     format: Format::Wav(Pcm::Int16),
///     topology: tutti_buffers::Bed::Quad.topology(),
///     rate: SampleRate::SR_48K,
/// };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    format: Format,
    topology: ChannelTopology,
    rate: SampleRate,
}

impl Spec {
    /// A file of `format` whose channels feed the speakers of `topology`, at
    /// `rate`. The encoder converts nothing: the samples handed to it are
    /// already at this rate.
    #[must_use]
    pub fn new(format: Format, topology: ChannelTopology, rate: SampleRate) -> Self {
        Self {
            format,
            topology,
            rate,
        }
    }

    /// A file of `layout` at `rate` whose speakers are what a bare width is
    /// taken to mean ([`ChannelTopology::default_for`]: mono, stereo, quad, 5.1, 7.1
    /// and 7.1.4 named, unassigned channels for every other width). A caller
    /// that knows better passes its own topology to [`new`](Self::new).
    #[must_use]
    pub fn of_width(format: Format, layout: ChannelLayout, rate: SampleRate) -> Self {
        Self::new(format, ChannelTopology::default_for(layout), rate)
    }

    /// The container and its settings.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// Which speaker each file channel feeds, for the WAV speaker mask.
    #[must_use]
    pub fn topology(&self) -> &ChannelTopology {
        &self.topology
    }

    /// The width of the file: the topology's length. Blocks written to the
    /// encoder have this width.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.topology.width()
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
