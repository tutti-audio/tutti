//! [`Encoding`]: what happens to audio on its way into a file.

use crate::{Format, Resample};
use tutti_buffers::{ChannelLayout, ChannelSet, Placement};
use tutti_units::SampleRate;

/// The pipeline in front of the encoder: the format a file is written as, the
/// channels it holds, the rate it is converted to and the dither noise it is
/// quantized with. It names no destination: [`ToFile`](crate::ToFile) is a
/// path, [`write`](crate::write) and [`encode`](crate::encode) take an
/// `Encoding` for a whole buffer, and a [`Chain`](crate::Chain) takes one for a
/// stream.
///
/// The stages run in the order fold, resample, gain, quantize. Quantizing is
/// last because an integer file's dither is scaled to one least significant bit
/// at the file's depth, and a rate conversion after it would filter that noise
/// along with the signal.
///
/// A `Chain` reads the options once, when it is made.
///
/// # Examples
///
/// ```
/// # #[cfg(feature = "encode-wav")] {
/// use tutti_buffers::{ChannelLayout, ChannelSet};
/// use tutti_codec::{Encoding, Format, IntDepth, IntPcm, Pcm, Resample};
/// use tutti_units::SampleRate;
///
/// let encoding = Encoding::new(Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24))))
///     .channels(ChannelSet::default_for(ChannelLayout::STEREO))
///     .resample(Resample::to(SampleRate::SR_44K1))
///     .dither_seed(7);
/// assert_eq!(encoding.rate(SampleRate::SR_48K), SampleRate::SR_44K1);
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
#[must_use = "an encoding does nothing until a `write`, `encode` or `Chain` takes it"]
pub struct Encoding {
    format: Format,
    channels: Channels,
    resample: Option<Resample>,
    seed: Option<u64>,
}

impl Encoding {
    /// An encoding to `format`, with the channels and the rate of its source
    /// and the dither of [`DEFAULT_SEED`](tutti_dsp::DitherNoise::DEFAULT_SEED) until a
    /// [`dither_seed`](Self::dither_seed) states another.
    pub fn new(format: Format) -> Self {
        Self {
            format,
            channels: Channels::Source,
            resample: None,
            seed: None,
        }
    }

    /// Writes `channels`, folded from the source's by [`tutti_buffers::Fold::to_channels`]
    /// (a width it cannot place is an error, never a dropped channel). Without it or
    /// [`width`](Self::width) the file has the source's channels. Replaces a `width` named
    /// before it.
    pub fn channels(mut self, channels: ChannelSet) -> Self {
        self.channels = Channels::Stated(channels);
        self
    }

    /// Writes `width` channels in the default order for that width
    /// ([`ChannelSet::default_for`]), folded from the source's as
    /// [`channels`](Self::channels) folds. The caller states a count, not speakers, so the
    /// file's [`placement`](Self::placement) is [`Placement::Default`]. Replaces `channels`
    /// named before it.
    pub fn width(mut self, width: ChannelLayout) -> Self {
        self.channels = Channels::Width(width);
        self
    }

    /// Writes at another rate than the source's.
    pub fn resample(mut self, resample: Resample) -> Self {
        self.resample = Some(resample);
        self
    }

    /// The noise sequence an integer file's dither draws from (a float file has none).
    ///
    /// Files that are summed later (a bounce's stems and its master) need distinct
    /// seeds: a bounce mixes one per file
    /// ([`SplitMix64::mix`](tutti_rt::SplitMix64::mix) of its index) into every encoding that
    /// states none. A seed stated here is used as given, so one seed given to several files
    /// gives them correlated dither, which adds coherently when they are summed: that is the
    /// caller's choice.
    pub fn dither_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// The container and its settings.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// The channels the file holds, when named by [`channels`](Self::channels) or
    /// [`width`](Self::width): `None` is the source's.
    #[must_use]
    pub fn channel_set(&self) -> Option<ChannelSet> {
        match self.channels {
            Channels::Source => None,
            Channels::Width(width) => Some(ChannelSet::default_for(width)),
            Channels::Stated(set) => Some(set),
        }
    }

    /// [`Placement::Stated`] when [`channels`](Self::channels) named the speakers, else
    /// [`Placement::Default`]: a [`width`](Self::width) is a count, and the source's channels
    /// are taken in the default order for their width.
    #[must_use]
    pub fn placement(&self) -> Placement {
        match self.channels {
            Channels::Stated(_) => Placement::Stated,
            Channels::Source | Channels::Width(_) => Placement::Default,
        }
    }

    /// The resample, when there is one.
    #[must_use]
    pub fn resampling(&self) -> Option<Resample> {
        self.resample
    }

    /// The dither seed [`dither_seed`](Self::dither_seed) stated; `None` is
    /// [`DEFAULT_SEED`](tutti_dsp::DitherNoise::DEFAULT_SEED) (or a bounce's per-file seed).
    #[must_use]
    pub fn seed(&self) -> Option<u64> {
        self.seed
    }

    /// The rate the file is written at: the resample's target, else `source`.
    #[must_use]
    pub fn rate(&self, source: SampleRate) -> SampleRate {
        self.resample.map_or(source, |r| r.target_rate)
    }
}

/// The channels an [`Encoding`] writes: the source's, a count in its default order, or a
/// stated set.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Channels {
    Source,
    Width(ChannelLayout),
    Stated(ChannelSet),
}
