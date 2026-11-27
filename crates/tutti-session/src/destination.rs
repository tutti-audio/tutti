//! [`Destination`]: where a bounce's output goes, and what the finished bounce hands back for
//! it.
//!
//! A destination is a request: [`ToFile`] pairs the codec's [`Encoding`] (format, channels,
//! rate, dither) with the codec's path request, and [`ToMemory`] asks for planes. The bounce's
//! main output and each of its stems take one, and the type says what comes back
//! ([`Destination::Output`]): a [`Written`] for a file and a [`Rendered`] for planes, with no
//! run-time match on the kind. What a destination does inside the pipeline (its target, its
//! key, the result it takes out of the finished render) is the sealed supertrait's.

use std::fmt::Debug;
use std::path::PathBuf;

use tutti_buffers::{ChannelLayout, ChannelSet, Float};
use tutti_codec::Encoding;
use tutti_export::{
    BufferKey, BufferTarget, Done, DoneError, FileKey, FileTarget, Gain, Output, Outputs,
    Placement, Rendered, Written,
};
use tutti_graph::TapReader;
use tutti_rt::SplitMix64;

use crate::bounce::{BounceConfigError, StemGain, StemKey};
use sealed::{Landing, MainShape, StemOutput, StemShape};

/// Where a bounce's output goes: a [`ToFile`] or a [`ToMemory`] (sealed). It names what the
/// finished bounce holds for that output, so the caller who chose a file is handed a
/// [`Written`] and one who chose memory a [`Rendered`].
pub trait Destination: sealed::Sealed {
    /// What the output is once rendered, at the graph's float.
    type Output<F: Float>: Debug + Send;
}

/// A file: the [`Encoding`] it is written as and the codec's [`ToFile`](tutti_codec::ToFile)
/// it lands at. [`Bounced::main_output`](crate::Bounced::main_output) is its [`Written`].
///
/// The file is written beside its path and put in place when the bounce finishes, so a
/// cancelled or failed bounce leaves what was at the path as it was, and nothing beside it.
/// The encoding's channels are the width of the file, and without them the output's own
/// (a main output's, or the width of the tapped channels of a stem). The dither seed of an
/// encoding that states none is mixed from the output's index, so files that are summed later
/// do not share one noise sequence; a seed stated with
/// [`Encoding::dither_seed`] is used as given.
///
/// ```no_run
/// # use tutti_session::{Bounce, ToFile};
/// # use tutti_codec::{Encoding, Format, IntDepth, IntPcm, Pcm};
/// # use tutti_buffers::ChannelLayout;
/// let wav = Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24)));
/// let _ = Bounce::<tutti_graph::NodeKey>::new().main_output(ToFile::new(
///     Encoding::new(wav).width(ChannelLayout::STEREO),
///     tutti_codec::ToFile::new("out/mix.wav").replace(),
/// ));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ToFile {
    encoding: Encoding,
    file: tutti_codec::ToFile,
}

impl ToFile {
    /// The file `file` names, written as `encoding`.
    pub fn new(encoding: Encoding, file: tutti_codec::ToFile) -> Self {
        Self { encoding, file }
    }

    /// A file at `path` written as this one is: the same format, and the channels and rate of
    /// whatever it is given.
    pub(crate) fn beside(&self, path: impl Into<PathBuf>) -> Self {
        Self::new(
            Encoding::new(self.encoding.format()),
            tutti_codec::ToFile::new(path),
        )
    }

    /// The file of output number `index` of a bounce, `width` channels wide unless the encoding
    /// states its channels.
    fn file(self, index: usize, width: ChannelLayout) -> FileTarget {
        let Self { encoding, file } = self;
        let encoding = if encoding.channel_set().is_none() {
            encoding.width(width)
        } else {
            encoding
        };
        let encoding = if encoding.seed().is_none() {
            encoding.dither_seed(SplitMix64::mix(index as u64))
        } else {
            encoding
        };
        FileTarget::new(encoding, file)
    }
}

/// Planes in memory: [`Bounced::main_output`](crate::Bounced::main_output) is their
/// [`Rendered`], one plane per channel at the render's rate. Nothing is encoded, resampled,
/// dithered or normalized, and a bounce into memory has no way to ask for any of them.
///
/// A bounce knows its span, so what it holds is bounded by its frame limit
/// ([`tutti_export::MAX_FRAMES`]): the cost is the frames, times the channels, times the
/// size of the graph's float, and a span too long for that is refused before anything is
/// rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToMemory {
    channels: Option<ChannelSet>,
}

impl ToMemory {
    /// Planes as wide as the output they hold.
    pub fn new() -> Self {
        Self::default()
    }

    /// The planes' channels: their width and the speakers they are, folded onto from the
    /// output's. A narrowing the fold cannot place is refused when the render starts, never
    /// truncated.
    pub fn channels(mut self, channels: ChannelSet) -> Self {
        self.channels = Some(channels);
        self
    }

    /// The planes of an output `width` channels wide unless channels were stated.
    fn planes(self, width: ChannelLayout) -> BufferTarget {
        match self.channels {
            None => BufferTarget::new(width),
            Some(set) => {
                let planes = BufferTarget::new(set.width());
                match set.topology() {
                    Some(topology) => planes.topology(*topology),
                    None => planes,
                }
            }
        }
    }
}

/// A file stem: its target, and whether it takes the main output's gain, which is known once
/// the main output has its key.
#[derive(Debug)]
struct FileStem {
    target: FileTarget,
    shared: bool,
}

impl<F: Float> StemOutput<F> for FileStem {
    fn landing(&self) -> Landing<'_> {
        Landing::File(&self.target)
    }

    fn add(
        self: Box<Self>,
        outputs: &mut Outputs<F>,
        reader: TapReader<F>,
        placement: Placement,
        main: Option<&FileKey>,
    ) -> Result<StemKey, BounceConfigError> {
        let Self { target, shared } = *self;
        let target = if shared {
            // Only a file main output is normalized, which is what the start checked a
            // shared gain against.
            let main = main.ok_or(BounceConfigError::SharedGainWithoutMainOutput)?;
            target.normalize(Gain::Of(main.clone()))
        } else {
            target
        };
        Ok(StemKey::File(
            outputs.file(Output::tap(reader, placement, target)),
        ))
    }
}

/// A stem held in memory.
#[derive(Debug)]
struct MemoryStem(BufferTarget);

impl<F: Float> StemOutput<F> for MemoryStem {
    fn landing(&self) -> Landing<'_> {
        Landing::Buffers(&self.0)
    }

    fn add(
        self: Box<Self>,
        outputs: &mut Outputs<F>,
        reader: TapReader<F>,
        placement: Placement,
        _main: Option<&FileKey>,
    ) -> Result<StemKey, BounceConfigError> {
        Ok(StemKey::Buffers(
            outputs.buffers(Output::tap(reader, placement, self.0)),
        ))
    }
}

pub(crate) mod sealed {
    use super::{
        BounceConfigError, BufferTarget, ChannelLayout, Destination, Done, DoneError, FileKey,
        FileTarget, Float, Gain, Output, Outputs, Placement, StemGain, StemKey, TapReader,
    };

    /// What a main output is built from: the bounce's figures that a destination turns into
    /// its target.
    #[derive(Debug)]
    pub struct MainShape {
        /// The graph's output width.
        pub channels: ChannelLayout,
        /// The main output's normalize, which only a file is given.
        pub normalize: Option<Gain>,
    }

    /// What a stem is built from: what the bounce knows of it once it starts.
    #[derive(Debug, Clone, Copy)]
    pub struct StemShape {
        /// The width of the channels it taps.
        pub lanes: ChannelLayout,
        /// The stem's position among the bounce's outputs, which the main output precedes.
        pub index: usize,
        /// How its file is normalized.
        pub gain: StemGain,
    }

    /// Where a target lands, borrowed: the part of it the bounce's checks read.
    #[derive(Debug)]
    pub enum Landing<'a> {
        File(&'a FileTarget),
        Buffers(&'a BufferTarget),
    }

    /// A stem's output, built: what the bounce checks of it and how it joins the render.
    pub trait StemOutput<F: Float>: Send + std::fmt::Debug {
        /// Where it lands.
        fn landing(&self) -> Landing<'_>;

        /// Adds the stem, reading `reader` placed as `placement`, to the render; `main` is
        /// the main output's file key, when it is a file.
        ///
        /// # Errors
        ///
        /// [`BounceConfigError::SharedGainWithoutMainOutput`] for a shared gain with no file
        /// main output to take it from.
        fn add(
            self: Box<Self>,
            outputs: &mut Outputs<F>,
            reader: TapReader<F>,
            placement: Placement,
            main: Option<&FileKey>,
        ) -> Result<StemKey, BounceConfigError>;
    }

    /// The destination's part that touches the pipeline: its target, its key, and the
    /// result it takes out of the finished render by that key.
    pub trait Sealed: Sized + Send + 'static {
        /// What the main output becomes: a `FileTarget` or a `BufferTarget`.
        type Target: Send + 'static;
        /// The key the main output was added under.
        type Key: Send + 'static;

        /// The main output's target from the bounce's figures.
        fn target(self, shape: MainShape) -> Self::Target;
        /// What `target` lands as.
        fn landing(target: &Self::Target) -> Landing<'_>;
        /// Adds the main output to the render.
        fn add<F: Float>(outputs: &mut Outputs<F>, output: Output<Self::Target, F>) -> Self::Key;
        /// The key of the main output's file, when it is one: the gain
        /// [`StemGain::Shared`] follows.
        fn file_key(key: &Self::Key) -> Option<&FileKey>;
        /// The main output's result, moved out of the finished render.
        fn take<F: Float>(
            done: &mut Done<F>,
            key: &Self::Key,
        ) -> Result<<Self as Destination>::Output<F>, DoneError>
        where
            Self: Destination;
        /// A stem's output, built once the bounce knows its figures.
        fn stem<F: Float>(self, shape: StemShape) -> Box<dyn StemOutput<F>>;
    }
}

impl sealed::Sealed for ToFile {
    type Target = FileTarget;
    type Key = FileKey;

    fn target(self, shape: MainShape) -> FileTarget {
        let file = self.file(0, shape.channels);
        match shape.normalize {
            Some(gain) => file.normalize(gain),
            None => file,
        }
    }

    fn landing(target: &FileTarget) -> Landing<'_> {
        Landing::File(target)
    }

    fn add<F: Float>(outputs: &mut Outputs<F>, output: Output<FileTarget, F>) -> FileKey {
        outputs.file(output)
    }

    fn file_key(key: &FileKey) -> Option<&FileKey> {
        Some(key)
    }

    fn take<F: Float>(done: &mut Done<F>, key: &FileKey) -> Result<Written, DoneError> {
        done.take_file(key)
    }

    fn stem<F: Float>(self, shape: StemShape) -> Box<dyn StemOutput<F>> {
        let target = self.file(shape.index, shape.lanes);
        Box::new(match shape.gain {
            StemGain::None => FileStem {
                target,
                shared: false,
            },
            StemGain::Shared => FileStem {
                target,
                shared: true,
            },
            StemGain::Each(normalize) => FileStem {
                target: target.normalize(Gain::Own(normalize)),
                shared: false,
            },
        })
    }
}

impl Destination for ToFile {
    type Output<F: Float> = Written;
}

impl sealed::Sealed for ToMemory {
    type Target = BufferTarget;
    type Key = BufferKey;

    fn target(self, shape: MainShape) -> BufferTarget {
        self.planes(shape.channels)
    }

    fn landing(target: &BufferTarget) -> Landing<'_> {
        Landing::Buffers(target)
    }

    fn add<F: Float>(outputs: &mut Outputs<F>, output: Output<BufferTarget, F>) -> BufferKey {
        outputs.buffers(output)
    }

    fn file_key(_: &BufferKey) -> Option<&FileKey> {
        None
    }

    fn take<F: Float>(done: &mut Done<F>, key: &BufferKey) -> Result<Rendered<F>, DoneError> {
        done.take_buffers(key)
    }

    fn stem<F: Float>(self, shape: StemShape) -> Box<dyn StemOutput<F>> {
        Box::new(MemoryStem(self.planes(shape.lanes)))
    }
}

impl Destination for ToMemory {
    type Output<F: Float> = Rendered<F>;
}
