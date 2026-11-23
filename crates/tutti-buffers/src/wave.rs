//! [`Wave`]: a resident, planar, multichannel sample buffer.

use crate::{BufferError, Interleaved, Mono, MonoMut, Planes, PlanesMut, Sample, deinterleave};
use crate::{ChannelLayout, ChannelSet, ChannelTopology, Placement};
use alloc::vec;
use alloc::vec::Vec;
use tutti_units::{SampleRate, Samples, Seconds};

/// Multichannel audio held in memory, one `Vec<S>` per channel.
///
/// What a sampler voice plays and what a decoder reads a file into. Planar
/// rather than interleaved because its consumers index it by `(channel,
/// frame)`; [`planes`](Self::planes) and [`channel`](Self::channel) give the
/// views of it, and [`append`](Self::append) takes interleaved frames in. Every
/// channel has the same length, [`len`](Self::len). The sample rate is carried
/// as data; nothing here resamples.
///
/// Indexing methods ([`channel`](Self::channel), [`at`](Self::at),
/// [`set`](Self::set), ...) panic on an out-of-range channel or frame, like
/// slice indexing.
///
/// # Examples
///
/// ```
/// use tutti_buffers::{Interleaved, Wave};
/// use tutti_units::{SampleRate, Samples};
/// use tutti_buffers::ChannelLayout;
///
/// let mut wave = Wave::<f32>::new(2, SampleRate::SR_48K);
/// wave.append(Interleaved::new(&[0.5, -0.5, 0.25, -0.25], ChannelLayout::STEREO)?)?;
/// assert_eq!(wave.len(), Samples::new(2));
/// assert_eq!(wave.channel(1).samples(), &[-0.5, -0.25]);
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Clone)]
pub struct Wave<S: Sample = f32> {
    /// One vector per channel, all of length `len`.
    vec: Vec<Vec<S>>,
    /// The rate the samples were recorded at. Nothing here resamples.
    sample_rate: SampleRate,
    /// Length in frames. 0 if there are no channels.
    len: Samples,
    /// Which speaker each channel feeds, when the width has a topology (at most
    /// [`MAX_CHANNELS`](crate::MAX_CHANNELS) channels).
    topology: Option<ChannelTopology>,
    /// Whether `topology` was stated or guessed from the count.
    placement: Placement,
}

/// Two waves are equal when their samples, rate and speakers are: the
/// [`placement`](Wave::placement) says where the topology came from, not what
/// the wave is, so a wave and its WAV round trip (a stated mono against the
/// default one) are equal.
impl<S: Sample> PartialEq for Wave<S> {
    fn eq(&self, other: &Self) -> bool {
        self.sample_rate == other.sample_rate
            && self.len == other.len
            && self.topology == other.topology
            && self.vec == other.vec
    }
}

/// What a bare channel count is taken to mean: [`ChannelTopology::default_for`].
fn default_for(channels: usize) -> (Option<ChannelTopology>, Placement) {
    let layout = ChannelLayout::try_from(channels).expect("a wave has at most u16::MAX channels");
    (ChannelTopology::default_for(layout), Placement::Default)
}

impl<S: Sample> core::fmt::Debug for Wave<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Wave")
            .field("sample_rate", &self.sample_rate)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl<S: Sample> Wave<S> {
    /// Creates an empty wave with `channels` channels.
    ///
    /// # Panics
    ///
    /// If `channels` is above `u16::MAX`, which no layout holds.
    pub fn new(channels: usize, sample_rate: SampleRate) -> Self {
        Self::with_capacity(channels, sample_rate, Samples::ZERO)
    }

    /// Creates an empty wave with `channels` channels, each with room for
    /// `capacity` frames.
    ///
    /// # Panics
    ///
    /// If `channels` is above `u16::MAX`, which no layout holds.
    pub fn with_capacity(channels: usize, sample_rate: SampleRate, capacity: Samples) -> Self {
        let (topology, placement) = default_for(channels);
        Self {
            vec: (0..channels)
                .map(|_| Vec::with_capacity(capacity.get()))
                .collect(),
            sample_rate,
            len: Samples::ZERO,
            topology,
            placement,
        }
    }

    /// Creates an empty wave whose channels feed the speakers of `topology`,
    /// one channel per speaker, with a [`Stated`](Placement::Stated) placement.
    ///
    /// [`new`](Self::new) takes a bare count, and a count is not a placement:
    /// it takes [`ChannelTopology::default_for`] and reports
    /// [`Placement::Default`]. Use this when the source says more.
    pub fn with_topology(topology: ChannelTopology, sample_rate: SampleRate) -> Self {
        Self::with_placement(ChannelSet::from(topology), Placement::Stated, sample_rate)
    }

    /// Creates an empty wave of `channels`, reporting `placement` as the basis
    /// of them (a decoder whose container stated a count and no placement
    /// passes [`Placement::Default`]). A set wider than
    /// [`MAX_CHANNELS`](crate::MAX_CHANNELS) has no topology, so neither does
    /// the wave.
    pub fn with_placement(
        channels: ChannelSet,
        placement: Placement,
        sample_rate: SampleRate,
    ) -> Self {
        Self {
            vec: vec![Vec::new(); usize::from(channels.width().count())],
            sample_rate,
            len: Samples::ZERO,
            topology: channels.topology().cloned(),
            placement,
        }
    }

    /// Creates an all-zero wave `duration` seconds long, rounded to the
    /// nearest frame.
    ///
    /// A [`Seconds`] is an `f64`, so a frame count in a long file stays exact.
    ///
    /// # Panics
    ///
    /// If `channels` is 0 and the duration rounds to at least one frame.
    pub fn zero(channels: usize, sample_rate: SampleRate, duration: impl Into<Seconds>) -> Self {
        let length = duration.into().to_samples(sample_rate).get();
        assert!(channels > 0 || length == 0);
        let (topology, placement) = default_for(channels);
        Self {
            vec: vec![vec![S::SILENCE; length]; channels],
            sample_rate,
            len: Samples::new(length),
            topology,
            placement,
        }
    }

    /// Creates a wave holding a copy of `planes`, one channel per plane, with
    /// the default layout for that width.
    pub fn from_planes(sample_rate: SampleRate, planes: Planes<'_, S>) -> Self {
        let (topology, placement) = default_for(planes.channels());
        Self {
            vec: planes.iter().map(|p| p.samples().to_vec()).collect(),
            sample_rate,
            len: planes.frames(),
            topology,
            placement,
        }
    }

    /// Creates a mono wave holding a copy of `samples`.
    pub fn from_mono(sample_rate: SampleRate, samples: Mono<'_, S>) -> Self {
        let (topology, placement) = default_for(1);
        Self {
            vec: vec![samples.samples().to_vec()],
            sample_rate,
            len: samples.frames(),
            topology,
            placement,
        }
    }

    /// Creates a wave holding the frames of `block`, deinterleaved, with the
    /// default layout for its width.
    pub fn from_interleaved(sample_rate: SampleRate, block: Interleaved<'_, S>) -> Self {
        let mut wave = Self::with_capacity(block.channels(), sample_rate, block.frames());
        // A new wave is the block's width, the one thing `append` refuses.
        let appended = wave.append(block);
        debug_assert!(appended.is_ok());
        wave
    }

    /// Returns a wave of `f` applied to every sample, with the same rate, topology
    /// and length.
    ///
    /// The way to change a wave's storage format with the caller's own rule;
    /// narrowing a float wave is `wave.map(|x| x as f32)`, and widening is
    /// `Wave::<f64>::from(&wave)`.
    ///
    /// ```
    /// use tutti_buffers::{Interleaved, Wave, ChannelLayout};
    /// use tutti_units::SampleRate;
    ///
    /// let wide = Wave::from_interleaved(
    ///     SampleRate::SR_48K,
    ///     Interleaved::new(&[0.5f64, -0.25], ChannelLayout::MONO)?,
    /// );
    /// let narrow = wide.map(|x| x as f32);
    /// assert_eq!(narrow.channel(0).samples(), &[0.5f32, -0.25]);
    /// # Ok::<(), tutti_buffers::BufferError>(())
    /// ```
    pub fn map<T: Sample>(&self, mut f: impl FnMut(S) -> T) -> Wave<T> {
        Wave {
            vec: self
                .vec
                .iter()
                .map(|channel| channel.iter().map(|&x| f(x)).collect())
                .collect(),
            sample_rate: self.sample_rate,
            len: self.len,
            topology: self.topology,
            placement: self.placement,
        }
    }

    /// Returns which speaker each channel feeds, or `None` for a wave wider than
    /// [`MAX_CHANNELS`](crate::MAX_CHANNELS), which no topology holds (its width is
    /// [`layout`](Self::layout)).
    ///
    /// A decoded file states its own; a wave built from a count carries the
    /// assumption [`new`](Self::new) describes. Its length is
    /// [`channels`](Self::channels).
    #[inline]
    pub fn topology(&self) -> Option<&ChannelTopology> {
        self.topology.as_ref()
    }

    /// Returns whether [`topology`](Self::topology) was stated or the default layout for
    /// the channel count.
    #[inline]
    pub fn placement(&self) -> Placement {
        self.placement
    }

    /// Returns the width, as a [`ChannelLayout`]: the length of the
    /// [`topology`](Self::topology) when it has one.
    ///
    /// # Panics
    ///
    /// Never for a wave built here, which holds at most `u16::MAX` channels
    /// ([`new`](Self::new) refuses more).
    #[inline]
    pub fn layout(&self) -> ChannelLayout {
        ChannelLayout::try_from(self.vec.len()).expect("a wave has at most u16::MAX channels")
    }

    /// Returns the channels as one value: the [`topology`](Self::topology)
    /// when the wave has one, else its width as unassigned channels.
    #[must_use]
    pub fn channel_set(&self) -> ChannelSet {
        self.topology
            .map_or_else(|| ChannelSet::unassigned(self.layout()), ChannelSet::from)
    }

    /// Returns the rate the samples were recorded at.
    #[inline]
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Returns the number of channels.
    #[inline]
    pub fn channels(&self) -> usize {
        self.vec.len()
    }

    /// Returns one channel.
    ///
    /// # Panics
    ///
    /// If `channel` is not below [`channels`](Self::channels).
    #[inline]
    #[must_use]
    pub fn channel(&self, channel: usize) -> Mono<'_, S> {
        Mono::new(&self.vec[channel])
    }

    /// Returns one channel, mutably. The length cannot change through this.
    ///
    /// # Panics
    ///
    /// If `channel` is not below [`channels`](Self::channels).
    #[inline]
    #[must_use]
    pub fn channel_mut(&mut self, channel: usize) -> MonoMut<'_, S> {
        MonoMut::new(&mut self.vec[channel])
    }

    /// Returns every channel as one view: empty (and of no width) for a wave
    /// with no channels.
    #[inline]
    #[must_use]
    pub fn planes(&self) -> Planes<'_, S> {
        if self.vec.is_empty() {
            return Planes::default();
        }
        Planes::from_vecs(&self.vec).unwrap_or_default()
    }

    /// Returns every channel as one mutable view: empty for a wave with no
    /// channels. The length cannot change through this.
    #[inline]
    #[must_use]
    pub fn planes_mut(&mut self) -> PlanesMut<'_, '_, S> {
        if self.vec.is_empty() {
            return PlanesMut::default();
        }
        PlanesMut::from_vecs(&mut self.vec).unwrap_or_default()
    }

    /// Returns the sample at frame `index` of `channel`.
    #[inline]
    pub fn at(&self, channel: usize, index: usize) -> S {
        self.vec[channel][index]
    }

    /// Overwrites the sample at frame `index` of `channel`.
    #[inline]
    pub fn set(&mut self, channel: usize, index: usize, value: S) {
        self.vec[channel][index] = value;
    }

    /// Appends the frames of `block`, a one-frame view being one frame.
    ///
    /// # Errors
    ///
    /// [`BufferError::WidthMismatch`] if `block` is not [`layout`](Self::layout)
    /// wide. Nothing is appended then.
    pub fn append(&mut self, block: Interleaved<'_, S>) -> Result<(), BufferError> {
        self.check_width(block.layout())?;
        let old = self.len;
        let new = old + block.frames();
        for channel in &mut self.vec {
            channel.resize(new.get(), S::SILENCE);
        }
        let mut planes = self.planes_mut();
        deinterleave(block, planes.slice_mut(old..new))?;
        self.len = new;
        Ok(())
    }

    /// Appends the frames of `planes`, plane `c` to channel `c`.
    ///
    /// # Errors
    ///
    /// As [`append`](Self::append).
    pub fn append_planes(&mut self, planes: Planes<'_, S>) -> Result<(), BufferError> {
        self.check_width(planes.layout())?;
        for (channel, plane) in self.vec.iter_mut().zip(planes) {
            channel.extend_from_slice(plane.samples());
        }
        self.len += planes.frames();
        Ok(())
    }

    /// Reserves room for `frames` more frames in every channel.
    pub fn reserve(&mut self, frames: Samples) {
        for channel in &mut self.vec {
            channel.reserve_exact(frames.get());
        }
    }

    fn check_width(&self, got: ChannelLayout) -> Result<(), BufferError> {
        if self.vec.len() == usize::from(got.count()) {
            return Ok(());
        }
        Err(BufferError::WidthMismatch {
            src: got,
            dst: self.layout(),
        })
    }

    /// Returns the length in frames.
    #[inline]
    #[must_use]
    pub fn len(&self) -> Samples {
        self.len
    }

    /// Returns whether the wave holds no frames.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == Samples::ZERO
    }

    /// Returns the duration, exact to the frame for the reason
    /// [`zero`](Self::zero) gives.
    #[inline]
    pub fn duration(&self) -> Seconds {
        Seconds::new(self.len.get() as f64 / self.sample_rate.get())
    }
}

/// Widening is exact: every sample is the `f32` as an `f64`, with the same
/// rate, topology and length. There is no `From` that narrows; that is
/// [`Wave::map`] with the caller's cast.
impl From<&Wave<f32>> for Wave<f64> {
    fn from(wave: &Wave<f32>) -> Self {
        wave.map(f64::from)
    }
}

#[cfg(test)]
mod tests;
