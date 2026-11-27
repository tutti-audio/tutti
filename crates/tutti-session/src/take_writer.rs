//! What a take's pump writes into: the take half of a [`Destination`](crate::Destination),
//! opened for the input's source once the take is armed and closed once the pump is done.

use std::path::PathBuf;
use std::sync::Arc;

use tutti_buffers::{
    ChannelLayout, ChannelSet, Float, Fold, Interleaved, InterleavedMut, Placement, Sink,
    SinkFailed, Wave,
};
use tutti_codec::{Chain, EncodeError, Kept, ToFile};
use tutti_dsp::FftFloat;
use tutti_export::Written;
use tutti_units::SampleRate;

use crate::{TakeError, ToMemory};

/// A take's destination, open: the frames of the source in, what the destination holds out.
///
/// A write that fails is [`SinkFailed`], and the failure's detail comes from the close, as a
/// [`Sink`]'s does.
pub trait Writer<F: Float>: Send + 'static {
    /// What the closed destination holds.
    type Output: Send;

    /// Appends `frames`, whole frames at the source's width.
    ///
    /// # Errors
    ///
    /// [`SinkFailed`] once a write has failed.
    fn write(&mut self, frames: Interleaved<'_, F>) -> Result<(), SinkFailed>;

    /// Closes the destination, flushing and committing.
    ///
    /// # Errors
    ///
    /// The take's error for a failed write or close, or a commit that put nothing in place.
    fn close(self) -> Result<Self::Output, TakeError>;
}

/// A file: the frames go through the encoding's stages into a file beside its path, which the
/// close puts in place.
#[derive(Debug)]
pub struct FileWriter<F: FftFloat> {
    chain: Chain<F>,
    path: PathBuf,
    placement: Placement,
}

impl<F: FftFloat> FileWriter<F> {
    pub(crate) fn open(
        to: ToFile,
        layout: ChannelLayout,
        rate: SampleRate,
    ) -> Result<Self, TakeError> {
        // A take has no length to plan: the format's limit is judged on each write.
        let chain = Chain::new(&to, &ChannelSet::default_for(layout), rate, None)
            .map_err(|e| TakeError::Open(Arc::new(e)))?;
        Ok(Self {
            chain,
            path: to.path().to_path_buf(),
            placement: to.placement(),
        })
    }
}

impl<F: FftFloat> Writer<F> for FileWriter<F> {
    type Output = Written;

    fn write(&mut self, frames: Interleaved<'_, F>) -> Result<(), SinkFailed> {
        self.chain.block(frames).map_err(|_| SinkFailed)
    }

    /// A file that cannot be closed whole is kept unfinished beside its path
    /// ([`TakeError::Sink`]), and one that cannot be put in place is kept whole
    /// ([`TakeError::Commit`]): a take never deletes its recording.
    fn close(self) -> Result<Written, TakeError> {
        let Self {
            chain,
            path,
            placement,
        } = self;
        let closed = chain.finalize().map_err(|kept| match kept {
            Kept::Commit(commit) => TakeError::Commit(commit),
            Kept::Unfinished { source, unfinished } => TakeError::Sink {
                source: Arc::new(source),
                unfinished: Some(unfinished),
            },
        })?;
        let landed = closed.commit().map_err(|e| match e {
            EncodeError::Commit(commit) => TakeError::Commit(commit),
            other => TakeError::Sink {
                source: Arc::new(other),
                unfinished: None,
            },
        })?;
        Ok(Written::new(path, landed, placement))
    }
}

/// A [`Wave`] in memory: the frames, folded onto the wave's channels when they differ from the
/// source's, are appended to it.
#[derive(Debug)]
pub struct MemoryWriter<F: Float> {
    wave: Wave<F>,
    width: ChannelLayout,
    /// Built when the take was armed.
    fold: Option<Fold<F>>,
    folded: Vec<F>,
}

impl<F: Float> MemoryWriter<F> {
    pub(crate) fn open(
        to: &ToMemory,
        layout: ChannelLayout,
        rate: SampleRate,
    ) -> Result<Self, TakeError> {
        let (set, fold) = to.fold(&ChannelSet::default_for(layout))?;
        Ok(Self {
            wave: Wave::with_placement(set, to.placement(), rate),
            width: set.width(),
            fold,
            folded: Vec::new(),
        })
    }
}

impl<F: Float> Writer<F> for MemoryWriter<F> {
    type Output = Wave<F>;

    fn write(&mut self, frames: Interleaved<'_, F>) -> Result<(), SinkFailed> {
        let spread = match &self.fold {
            None => frames,
            Some(fold) => {
                self.folded.clear();
                self.folded
                    .resize(self.width.interleaved_len(frames.frames()), F::ZERO);
                let to =
                    InterleavedMut::new(&mut self.folded, self.width).map_err(|_| SinkFailed)?;
                fold.fold(frames, to).map_err(|_| SinkFailed)?;
                Interleaved::new(&self.folded, self.width).map_err(|_| SinkFailed)?
            }
        };
        self.wave.append(spread).map_err(|_| SinkFailed)
    }

    fn close(self) -> Result<Wave<F>, TakeError> {
        Ok(self.wave)
    }
}

/// A caller's sink, as it is: the frames go to it unchanged.
pub struct SinkWriter<S>(S);

impl<S> std::fmt::Debug for SinkWriter<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SinkWriter").finish_non_exhaustive()
    }
}

impl<S: Sink> SinkWriter<S> {
    pub(crate) fn open(sink: S, layout: ChannelLayout) -> Result<Self, TakeError> {
        let wide = sink.layout();
        if wide != layout {
            return Err(TakeError::Width {
                input: layout,
                sink: wide,
            });
        }
        Ok(Self(sink))
    }
}

impl<F, S> Writer<F> for SinkWriter<S>
where
    F: Float,
    S: Sink<Sample = F> + Send + 'static,
    S::Error: Send + Sync + 'static,
{
    type Output = ();

    fn write(&mut self, frames: Interleaved<'_, F>) -> Result<(), SinkFailed> {
        self.0.write(frames)
    }

    fn close(self) -> Result<(), TakeError> {
        self.0.finalize().map_err(|e| TakeError::Sink {
            source: Arc::new(e),
            unfinished: None,
        })
    }
}
