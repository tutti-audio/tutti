//! The outputs of one render, by kind, and the results read back by key.
//!
//! [`Outputs`] holds a list of file outputs, of buffer outputs and of sink outputs.
//! Adding a file or buffers returns its key, a [`FileKey`] or a [`BufferKey`]; a sink has none; the finished
//! render ([`Done`]) gives each result by the key it was added under, so what
//! comes back is typed by what went in. A key is branded by the `Outputs` that
//! made it: another render's key is a named error where it is used.

use std::sync::Arc;

use tutti_buffers::{Float, Wave};

use crate::Written;
use crate::output::{Brand, BufferKey, BufferTarget, FileKey, FileTarget, Output};
use crate::{DoneError, SinkTarget};

/// An output, and where it was added among all of the render's outputs.
pub(crate) struct Placed<T, F: Float> {
    /// How many outputs, of either kind, were added before it: the number the
    /// render's errors and progress name it by.
    pub(crate) position: usize,
    pub(crate) output: Output<T, F>,
}

/// The outputs one render feeds: files, buffers and sinks, each added by its kind.
///
/// ```
/// # use tutti_export::{BufferTarget, Output, Outputs};
/// # use tutti_buffers::ChannelLayout;
/// let mut outputs = Outputs::<f32>::new();
/// let planes = outputs.buffers(Output::main_output(BufferTarget::new(ChannelLayout::STEREO)));
/// # let _ = planes;
/// ```
#[derive(Debug)]
pub struct Outputs<F: Float = f32> {
    id: Arc<()>,
    files: Vec<Placed<FileTarget, F>>,
    buffers: Vec<Placed<BufferTarget, F>>,
    sinks: Vec<Placed<SinkTarget<F>, F>>,
}

impl<T, F: Float> std::fmt::Debug for Placed<T, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Placed")
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

impl<F: Float> Default for Outputs<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Float> Outputs<F> {
    /// No outputs yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: Arc::new(()),
            files: Vec::new(),
            buffers: Vec::new(),
            sinks: Vec::new(),
        }
    }

    /// Adds a file output; the key reads its [`Written`] from the finished render.
    pub fn file(&mut self, output: Output<FileTarget, F>) -> FileKey {
        let key = FileKey::new(self.brand(), self.files.len());
        let position = self.len();
        self.files.push(Placed { position, output });
        key
    }

    /// Adds a buffer output; the key reads its [`Wave`] from the finished render.
    pub fn buffers(&mut self, output: Output<BufferTarget, F>) -> BufferKey {
        let key = BufferKey::new(self.brand(), self.buffers.len());
        let position = self.len();
        self.buffers.push(Placed { position, output });
        key
    }

    /// Adds an output streamed into a caller's sink. It has no key: its result is the sink
    /// itself, and a sink that fails fails the render.
    pub fn sink(&mut self, output: Output<SinkTarget<F>, F>) {
        let position = self.len();
        self.sinks.push(Placed { position, output });
    }

    /// How many outputs, of every kind: the position the next one is added at.
    fn len(&self) -> usize {
        self.files.len() + self.buffers.len() + self.sinks.len()
    }

    fn brand(&self) -> Brand {
        Brand::of(&self.id)
    }

    /// The outputs, taken apart for the render.
    pub(crate) fn into_parts(self) -> Parts<F> {
        Parts {
            brand: self.brand(),
            files: self.files,
            buffers: self.buffers,
            sinks: self.sinks,
        }
    }
}

/// An [`Outputs`] taken apart for the render.
pub(crate) struct Parts<F: Float> {
    brand: Brand,
    pub(crate) files: Vec<Placed<FileTarget, F>>,
    pub(crate) buffers: Vec<Placed<BufferTarget, F>>,
    pub(crate) sinks: Vec<Placed<SinkTarget<F>, F>>,
}

/// The brand of the [`Outputs`] a render was given, which its [`Done`] carries.
pub(crate) struct Stamp(Brand);

impl<F: Float> Parts<F> {
    /// Whether the outputs made `key`.
    pub(crate) fn owns(&self, key: &FileKey) -> bool {
        key.issued_by(&self.brand)
    }

    pub(crate) fn stamp(&self) -> Stamp {
        Stamp(self.brand.clone())
    }
}

impl Stamp {
    /// The results of the render, branded as the outputs' keys are.
    pub(crate) fn done<F: Float>(
        self,
        files: Vec<Written>,
        buffers: Vec<Wave<F>>,
        dropped_events: u64,
    ) -> Done<F> {
        Done {
            brand: self.0,
            files: files.into_iter().map(Some).collect(),
            buffers: buffers.into_iter().map(Some).collect(),
            dropped_events,
        }
    }
}

/// What a finished render produced, read by the key each output was added under.
#[derive(Debug)]
pub struct Done<F: Float = f32> {
    brand: Brand,
    files: Vec<Option<Written>>,
    buffers: Vec<Option<Wave<F>>>,
    dropped_events: u64,
}

impl<F: Float> Done<F> {
    /// The file `key` names.
    ///
    /// # Errors
    ///
    /// [`DoneError::Foreign`] when another [`Outputs`] made `key`;
    /// [`DoneError::Taken`] after [`take_file`](Self::take_file).
    pub fn file(&self, key: &FileKey) -> Result<&Written, DoneError> {
        self.check(key.issued_by(&self.brand))?;
        self.files[key.index()].as_ref().ok_or(DoneError::Taken)
    }

    /// The audio `key` names.
    ///
    /// # Errors
    ///
    /// [`DoneError::Foreign`] when another [`Outputs`] made `key`;
    /// [`DoneError::Taken`] after [`take_buffers`](Self::take_buffers).
    pub fn buffers(&self, key: &BufferKey) -> Result<&Wave<F>, DoneError> {
        self.check(key.issued_by(&self.brand))?;
        self.buffers[key.index()].as_ref().ok_or(DoneError::Taken)
    }

    /// Moves the file `key` names out, so a caller who owns the result need not
    /// copy it; the other results stay.
    ///
    /// # Errors
    ///
    /// As [`file`](Self::file); a second take is [`DoneError::Taken`].
    pub fn take_file(&mut self, key: &FileKey) -> Result<Written, DoneError> {
        self.check(key.issued_by(&self.brand))?;
        self.files[key.index()].take().ok_or(DoneError::Taken)
    }

    /// Moves the audio `key` names out; the other results stay.
    ///
    /// # Errors
    ///
    /// As [`buffers`](Self::buffers); a second take is [`DoneError::Taken`].
    pub fn take_buffers(&mut self, key: &BufferKey) -> Result<Wave<F>, DoneError> {
        self.check(key.issued_by(&self.brand))?;
        self.buffers[key.index()].take().ok_or(DoneError::Taken)
    }

    /// Events the graph lost during the render (not a note-off it refused and
    /// sent again), whichever output read them: nonzero only under
    /// [`EventLossPolicy::Allow`](crate::EventLossPolicy::Allow) (under `Fail` the render is
    /// an error instead). A lost note-on or note-off makes every output wrong.
    /// Each [`Written`] states the same figure.
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    fn check(&self, issued_here: bool) -> Result<(), DoneError> {
        if issued_here {
            Ok(())
        } else {
            Err(DoneError::Foreign)
        }
    }
}
