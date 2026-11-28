//! [`VoicePoolConfig`]: the voices a pool holds, as rows keyed by [`SlotId`].

use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_graph::{ForkCause, HostValues, Initials, NodeConfig, PublishedKind, RenderMode};
use tutti_rt::Mutex;
use tutti_units::{Cents, StretchFactor};

use super::command::MAX_RESIDENT_VOICES;
use super::memory_source::VoiceWindow;
use super::node_config::{Built, Free, Placed, Streamed, VoiceDrive, VoiceNodeConfig};
use super::node_controls::Steering;
use super::pool::VoicePool;
use super::types::{Playback, SlotId, Voice};
use crate::streamer::StreamHandle;
use crate::{PlaceError, SamplerFloat, voice_width};

const _: () = assert!(MAX_RESIDENT_VOICES <= u16::MAX as usize);

tutti_units::ranged! {
    /// How many voices a pool holds at most: at least one. Not a render budget: the
    /// slot vector and the rings are sized from it, and each voice costs what its
    /// source costs, so there is no upper bound beyond the type's.
    VoiceCount(u16), VoiceCountOutOfRange, "at least one voice", |raw| raw >= 1
}

// `ranged!` derives `Eq` on the error only, so the count's is written here.
impl Eq for VoiceCount {}

impl VoiceCount {
    /// The voices a pool holds when [`VoicePoolConfig::voices`] is not called.
    pub const DEFAULT: Self = Self(MAX_RESIDENT_VOICES as u16);
}

impl Default for VoiceCount {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What a [`VoicePoolConfig`] refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TableError {
    /// A row names a [`SlotId`] another row already has.
    #[error("slot {} already has a row", .0.0)]
    DuplicateKey(SlotId),
    /// The rows are more than the pool's voices.
    #[error("{rows} rows do not fit a pool of {} voices", .capacity.get())]
    TooMany {
        /// How many rows the config would hold.
        rows: usize,
        /// The pool's voices.
        capacity: VoiceCount,
    },
}

/// The stretch and pitch a stretching row carries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Stretching {
    pub(crate) factor: StretchFactor,
    pub(crate) pitch: Cents,
}

/// What a row asks of its voice that is a value: every one is applied to a
/// running voice by comparison with what it last applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RowValues {
    /// The gain, speed, loop and direction, and the stretch and pitch of a
    /// stretching row (unity and zero for one that does not stretch).
    pub(crate) play: Playback,
    /// The window of a placed resident voice. A streamed voice's window is its
    /// stream's, fixed when the stream was opened.
    pub(crate) window: Option<VoiceWindow>,
    /// The play state of a free-running voice.
    pub(crate) steering: Steering,
    /// Whether the row stretches: structure, so equal for a voice and its row.
    pub(crate) stretching: bool,
}

/// `config` with `stretch` as its stretch and pitch: the row's, not the config's.
fn with_stretch<F: SamplerFloat, D: VoiceDrive>(
    config: &VoiceNodeConfig<F, D>,
    stretch: Option<Stretching>,
) -> VoiceNodeConfig<F, D> {
    match stretch {
        Some(Stretching { factor, pitch }) => config.clone().stretched_by(factor, pitch),
        None => config.clone(),
    }
}

/// What `config` asks that is a value, with `stretch` as its stretch and pitch.
fn values_of<F: SamplerFloat, D: VoiceDrive>(
    config: &VoiceNodeConfig<F, D>,
    stretch: Option<Stretching>,
) -> RowValues {
    let (mut play, window, steering) = config.asks();
    if let Some(Stretching { factor, pitch }) = stretch {
        play.stretch = factor;
        play.pitch = pitch;
    }
    RowValues {
        play,
        window,
        steering,
        stretching: stretch.is_some(),
    }
}

/// The config a row was made from, one arm per drive.
enum RowForm<F: SamplerFloat> {
    Free(VoiceNodeConfig<F, Free>),
    Placed(VoiceNodeConfig<F, Placed>),
    Streamed(VoiceNodeConfig<F, Streamed>),
}

/// One voice of a pool: what a [`VoiceNodeConfig`] describes, whatever its
/// drive, as a row of a [`VoicePoolConfig`].
///
/// A row is made from a [`VoiceNodeConfig`] by `From`, and its constructor has
/// already checked the gain and the width. Whether the row stretches (holds a
/// vocoder) is structure: a config that asks for a stretch or a pitch shift
/// stretches, and a non-stretching row that later asks for one is a new voice.
/// [`stretching`](Self::stretching) is how a warped row starts at unity. A row
/// that does not stretch has no factor or pitch to hold. The factor and the
/// pitch of a stretching row, and the gain, speed, loop, direction, window and
/// steering, are values, held as units ([`Amplitude`](tutti_units::Amplitude),
/// [`PlaybackRate`](tutti_units::PlaybackRate), [`StretchFactor`], [`Cents`]).
///
/// A streamed row's loop and direction are its stream's cells and nothing else: the
/// pool's controls write them from the row when the table is written, and the node
/// never reads them from the table.
pub struct Row<F: SamplerFloat> {
    /// The config with its stretch and pitch at their defaults: a row's
    /// stretch is `stretch`, not the config's.
    form: RowForm<F>,
    stretch: Option<Stretching>,
}

macro_rules! row_from {
    ($($drive:ident),+) => {$(
        impl<F: SamplerFloat> From<VoiceNodeConfig<F, $drive>> for Row<F> {
            fn from(config: VoiceNodeConfig<F, $drive>) -> Self {
                let (config, stretch) = config.split_stretch();
                Self {
                    form: RowForm::$drive(config),
                    stretch: stretch.map(|(factor, pitch)| Stretching { factor, pitch }),
                }
            }
        }
    )+};
}
row_from!(Free, Placed, Streamed);

impl<F: SamplerFloat> Row<F> {
    /// This row stretching by `factor` and shifted by `pitch`, at unity too: a
    /// row that stretches owns its vocoder, and a change of the factor or the
    /// pitch is a change of values.
    #[must_use]
    pub fn stretching(mut self, factor: StretchFactor, pitch: Cents) -> Self {
        self.stretch = Some(Stretching { factor, pitch });
        self
    }

    /// What the row asks that is a value, read on the audio thread: copies,
    /// nothing allocated.
    pub(crate) fn values(&self) -> RowValues {
        match &self.form {
            RowForm::Free(c) => values_of(c, self.stretch),
            RowForm::Placed(c) => values_of(c, self.stretch),
            RowForm::Streamed(c) => values_of(c, self.stretch),
        }
    }

    /// The voice the row describes, at its values. A streamed row's reader is taken,
    /// or waited for. Control side.
    pub(crate) fn built(&self) -> Built<F> {
        match &self.form {
            RowForm::Free(c) => with_stretch(c, self.stretch).built(),
            RowForm::Placed(c) => with_stretch(c, self.stretch).built(),
            RowForm::Streamed(c) => with_stretch(c, self.stretch).built(),
        }
    }

    /// The voice a fork of the pool plays for this row.
    pub(crate) fn forked(&self, mode: RenderMode) -> Result<Voice<F>, ForkCause> {
        match &self.form {
            RowForm::Free(c) => with_stretch(c, self.stretch).forked(mode),
            RowForm::Placed(c) => with_stretch(c, self.stretch).forked(mode),
            RowForm::Streamed(c) => with_stretch(c, self.stretch).forked(mode),
        }
    }

    /// The stream a streamed row reads, whose cells the controls sweep.
    pub(crate) fn stream(&self) -> Option<Arc<Mutex<StreamHandle<F>>>> {
        match &self.form {
            RowForm::Streamed(c) => c.stream(),
            RowForm::Free(_) | RowForm::Placed(_) => None,
        }
    }

    /// Writes the row's loop and direction to its stream's cells, which the
    /// streamer reads: the one place a streamed row's loop and direction are
    /// written, and the only place they are read from (the node reads the
    /// stream's cells, never the table). A resident row has no stream. Control
    /// side.
    pub(crate) fn steer_stream(&self) {
        if let RowForm::Streamed(c) = &self.form {
            c.steer_stream();
        }
    }

    /// Whether `other` is the same voice with other values: the drive, the wave
    /// by identity or the stream by identity, the width and whether the row
    /// stretches are equal. The gain, speed, loop, direction, window, steering,
    /// stretch factor and pitch are not compared.
    #[must_use]
    pub fn same_structure(&self, other: &Self) -> bool {
        self.stretch.is_some() == other.stretch.is_some()
            && match (&self.form, &other.form) {
                (RowForm::Free(a), RowForm::Free(b)) => a.same_structure(b),
                (RowForm::Placed(a), RowForm::Placed(b)) => a.same_structure(b),
                (RowForm::Streamed(a), RowForm::Streamed(b)) => a.same_structure(b),
                _ => false,
            }
    }
}

impl<F: SamplerFloat> Clone for RowForm<F> {
    fn clone(&self) -> Self {
        match self {
            Self::Free(c) => Self::Free(c.clone()),
            Self::Placed(c) => Self::Placed(c.clone()),
            Self::Streamed(c) => Self::Streamed(c.clone()),
        }
    }
}

impl<F: SamplerFloat> PartialEq for RowForm<F> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Free(a), Self::Free(b)) => a == b,
            (Self::Placed(a), Self::Placed(b)) => a == b,
            (Self::Streamed(a), Self::Streamed(b)) => a == b,
            _ => false,
        }
    }
}

impl<F: SamplerFloat> std::fmt::Debug for RowForm<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Free(c) => c.fmt(f),
            Self::Placed(c) => c.fmt(f),
            Self::Streamed(c) => c.fmt(f),
        }
    }
}

impl<F: SamplerFloat> Clone for Row<F> {
    fn clone(&self) -> Self {
        Self {
            form: self.form.clone(),
            stretch: self.stretch,
        }
    }
}

impl<F: SamplerFloat> PartialEq for Row<F> {
    fn eq(&self, other: &Self) -> bool {
        self.stretch == other.stretch && self.form == other.form
    }
}

impl<F: SamplerFloat> std::fmt::Debug for Row<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row")
            .field("form", &self.form)
            .field("stretch", &self.stretch)
            .finish()
    }
}

/// The rows of a [`VoicePoolConfig`], sorted by key: two configs over the same
/// rows hold equal tables. It is the value of [`VoiceTableKind`].
pub struct VoiceTable<F: SamplerFloat> {
    rows: Vec<(SlotId, Row<F>)>,
}

impl<F: SamplerFloat> VoiceTable<F> {
    /// The rows, in key order.
    #[must_use]
    pub fn rows(&self) -> &[(SlotId, Row<F>)] {
        &self.rows
    }
}

impl<F: SamplerFloat> Clone for VoiceTable<F> {
    fn clone(&self) -> Self {
        Self {
            rows: self.rows.clone(),
        }
    }
}

impl<F: SamplerFloat> PartialEq for VoiceTable<F> {
    fn eq(&self, other: &Self) -> bool {
        self.rows == other.rows
    }
}

impl<F: SamplerFloat> std::fmt::Debug for VoiceTable<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(&self.rows).finish()
    }
}

/// The kind of a pool's voices: the [`VoiceTable`] a session writes.
#[derive(Clone, Copy, Debug)]
pub enum VoiceTableKind<F: SamplerFloat> {
    #[doc(hidden)]
    Uninhabited(Infallible, PhantomData<fn() -> F>),
}

impl<F: SamplerFloat> PublishedKind for VoiceTableKind<F> {
    type Value = Arc<VoiceTable<F>>;
}

/// What a voice pool is made of: its output width, how many voices it holds and
/// the rows it plays, each a [`Row`] under a [`SlotId`].
///
/// The width and the voice count are structure, and the rows are published
/// values (a [`VoiceTable`]). A row is added with [`row`](Self::row), which
/// refuses a key already in use and a row past the voice count, so a config
/// never holds either. [`voices`](Self::voices) refuses a count below the rows
/// already added, so the order of the two calls does not decide what is
/// accepted. The rows are kept in key order: configs over the same rows are
/// equal whatever order the rows were added in.
///
/// ```
/// use std::sync::Arc;
/// use tutti_buffers::ChannelLayout;
/// use tutti_sampler::{MemorySourceConfig, SlotId, VoiceNodeConfig, VoicePoolConfig, Wave};
/// use tutti_units::SampleRate;
///
/// let wave = Arc::new(Wave::<f32>::new(2, SampleRate::SR_48K));
/// let source = MemorySourceConfig::new(wave, ChannelLayout::STEREO).expect("a stereo wave places");
/// let voice = VoiceNodeConfig::free(source).expect("a finite gain, and a source on its own cursor");
/// let config = VoicePoolConfig::<f32>::new(ChannelLayout::STEREO)
///     .expect("stereo is a width a pool runs")
///     .row(SlotId(7), voice)
///     .expect("one row in a free key");
/// assert_eq!(config.table().rows().len(), 1);
/// ```
pub struct VoicePoolConfig<F: SamplerFloat> {
    channels: ChannelLayout,
    voices: VoiceCount,
    table: Arc<VoiceTable<F>>,
}

impl<F: SamplerFloat> VoicePoolConfig<F> {
    /// A pool of `channels` outputs, [`VoiceCount::DEFAULT`] voices and no rows.
    ///
    /// # Errors
    ///
    /// [`PlaceError::EmptyWidth`] for no channels, and
    /// [`PlaceError::WidthTooWide`] for a width the sampler cannot run a voice
    /// at ([`voice_width`]).
    pub fn new(channels: impl Into<ChannelLayout>) -> Result<Self, PlaceError> {
        Ok(Self {
            channels: voice_width(channels.into())?,
            voices: VoiceCount::DEFAULT,
            table: Arc::new(VoiceTable { rows: Vec::new() }),
        })
    }

    /// This config holding `voices` voices at most.
    ///
    /// # Errors
    ///
    /// [`TableError::TooMany`] for fewer voices than the rows already added.
    pub fn voices(mut self, voices: VoiceCount) -> Result<Self, TableError> {
        let rows = self.table.rows.len();
        if rows > usize::from(voices.get()) {
            return Err(TableError::TooMany {
                rows,
                capacity: voices,
            });
        }
        self.voices = voices;
        Ok(self)
    }

    /// This config with `row` under `key`.
    ///
    /// # Errors
    ///
    /// [`TableError::DuplicateKey`] for a key that has a row, and
    /// [`TableError::TooMany`] for a row past the voices.
    pub fn row(mut self, key: SlotId, row: impl Into<Row<F>>) -> Result<Self, TableError> {
        let rows = &self.table.rows;
        let at = match rows.binary_search_by_key(&key.0, |(held, _)| held.0) {
            Ok(_) => return Err(TableError::DuplicateKey(key)),
            Err(at) => at,
        };
        if rows.len() >= usize::from(self.voices.get()) {
            return Err(TableError::TooMany {
                rows: rows.len() + 1,
                capacity: self.voices,
            });
        }
        Arc::make_mut(&mut self.table)
            .rows
            .insert(at, (key, row.into()));
        Ok(self)
    }

    /// The rows, as the value a session writes.
    #[must_use]
    pub fn table(&self) -> &Arc<VoiceTable<F>> {
        &self.table
    }

    /// The output width.
    #[must_use]
    pub(crate) fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// The voices the pool holds at most.
    #[must_use]
    pub(crate) fn capacity(&self) -> VoiceCount {
        self.voices
    }
}

impl<F: SamplerFloat> NodeConfig<F> for VoicePoolConfig<F> {
    type Node = VoicePool<F>;

    fn build(&self, _: &HostValues) -> VoicePool<F> {
        VoicePool::from(self.clone())
    }

    /// The width and the voice count: the slot vector and the rings are sized
    /// from them. The rows are a published value.
    fn same_structure(&self, other: &Self) -> bool {
        self.channels == other.channels && self.voices == other.voices
    }

    fn initials(&self) -> Initials {
        Initials::new().published::<VoiceTableKind<F>>(Arc::clone(&self.table))
    }
}

impl<F: SamplerFloat> Clone for VoicePoolConfig<F> {
    fn clone(&self) -> Self {
        Self {
            channels: self.channels,
            voices: self.voices,
            table: Arc::clone(&self.table),
        }
    }
}

impl<F: SamplerFloat> PartialEq for VoicePoolConfig<F> {
    fn eq(&self, other: &Self) -> bool {
        self.channels == other.channels && self.voices == other.voices && self.table == other.table
    }
}

impl<F: SamplerFloat> std::fmt::Debug for VoicePoolConfig<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoicePoolConfig")
            .field("channels", &self.channels)
            .field("voices", &self.voices)
            .field("table", &self.table)
            .finish()
    }
}
