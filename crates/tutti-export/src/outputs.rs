//! The outputs of one render, by kind, and the results read back by key.
//!
//! [`Outputs`] holds a list of file outputs and a list of buffer outputs.
//! Adding one returns its key, a [`FileKey`] or a [`BufferKey`]; the finished
//! render ([`Done`]) gives each result by the key it was added under, so what
//! comes back is typed by what went in. A key is branded by the `Outputs` that
//! made it: another render's key is a named error where it is used.

use std::sync::{Arc, Weak};

use tutti_buffers::Float;

use crate::output::{BufferTarget, FileTarget, Output};
use crate::{Rendered, Written};

/// The identity of one [`Outputs`], carried by its keys.
///
/// A key holds a weak count of the allocation its `Outputs` owns, so two
/// brands are equal only when they came from one `Outputs`, and an address is
/// never reused while a key of it lives.
#[derive(Debug, Clone)]
struct Brand(Weak<()>);

impl PartialEq for Brand {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.0, &other.0)
    }
}

/// The key of a file output, from [`Outputs::file`].
#[derive(Debug, Clone, PartialEq)]
pub struct FileKey {
    brand: Brand,
    index: usize,
}

/// The key of a buffer output, from [`Outputs::buffers`].
#[derive(Debug, Clone, PartialEq)]
pub struct BufferKey {
    brand: Brand,
    index: usize,
}

/// Why a [`Done`] has no result for a key.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DoneError {
    /// Another [`Outputs`] made the key.
    #[error("the key belongs to another Outputs")]
    Foreign,
    /// The result was moved out by [`Done::take_file`] or [`Done::take_buffers`].
    #[error("the result was already taken")]
    Taken,
}

impl FileKey {
    /// The index of the file among the file outputs, in the order they were added.
    pub(crate) fn index(&self) -> usize {
        self.index
    }
}

/// An output, and where it was added among all of the render's outputs.
pub(crate) struct Placed<T, F: Float> {
    /// How many outputs, of either kind, were added before it: the number the
    /// render's errors and progress name it by.
    pub(crate) position: usize,
    pub(crate) output: Output<T, F>,
}

/// The outputs one render feeds: files and buffers, each added by its kind.
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
        }
    }

    /// Adds a file output; the key reads its [`Written`] from the finished render.
    pub fn file(&mut self, output: Output<FileTarget, F>) -> FileKey {
        let key = FileKey {
            brand: self.brand(),
            index: self.files.len(),
        };
        let position = self.len();
        self.files.push(Placed { position, output });
        key
    }

    /// Adds a buffer output; the key reads its [`Rendered`] from the finished render.
    pub fn buffers(&mut self, output: Output<BufferTarget, F>) -> BufferKey {
        let key = BufferKey {
            brand: self.brand(),
            index: self.buffers.len(),
        };
        let position = self.len();
        self.buffers.push(Placed { position, output });
        key
    }

    /// How many outputs, of both kinds: the position the next one is added at.
    fn len(&self) -> usize {
        self.files.len() + self.buffers.len()
    }

    fn brand(&self) -> Brand {
        Brand(Arc::downgrade(&self.id))
    }

    /// The outputs, taken apart for the render.
    pub(crate) fn into_parts(self) -> Parts<F> {
        Parts {
            brand: self.brand(),
            files: self.files,
            buffers: self.buffers,
        }
    }
}

/// An [`Outputs`] taken apart for the render.
pub(crate) struct Parts<F: Float> {
    brand: Brand,
    pub(crate) files: Vec<Placed<FileTarget, F>>,
    pub(crate) buffers: Vec<Placed<BufferTarget, F>>,
}

/// The brand of the [`Outputs`] a render was given, which its [`Done`] carries.
pub(crate) struct Stamp(Brand);

impl<F: Float> Parts<F> {
    /// Whether the outputs made `key`.
    pub(crate) fn owns(&self, key: &FileKey) -> bool {
        key.brand == self.brand
    }

    pub(crate) fn stamp(&self) -> Stamp {
        Stamp(self.brand.clone())
    }
}

impl Stamp {
    /// The results of the render, branded as the outputs' keys are.
    pub(crate) fn done<F: Float>(self, files: Vec<Written>, buffers: Vec<Rendered<F>>) -> Done<F> {
        Done {
            brand: self.0,
            files: files.into_iter().map(Some).collect(),
            buffers: buffers.into_iter().map(Some).collect(),
        }
    }
}

/// What a finished render produced, read by the key each output was added under.
#[derive(Debug)]
pub struct Done<F: Float = f32> {
    brand: Brand,
    files: Vec<Option<Written>>,
    buffers: Vec<Option<Rendered<F>>>,
}

impl<F: Float> Done<F> {
    /// The file `key` names.
    ///
    /// # Errors
    ///
    /// [`DoneError::Foreign`] when another [`Outputs`] made `key`;
    /// [`DoneError::Taken`] after [`take_file`](Self::take_file).
    pub fn file(&self, key: &FileKey) -> Result<&Written, DoneError> {
        self.check(&key.brand)?;
        self.files[key.index].as_ref().ok_or(DoneError::Taken)
    }

    /// The planes `key` names.
    ///
    /// # Errors
    ///
    /// [`DoneError::Foreign`] when another [`Outputs`] made `key`;
    /// [`DoneError::Taken`] after [`take_buffers`](Self::take_buffers).
    pub fn buffers(&self, key: &BufferKey) -> Result<&Rendered<F>, DoneError> {
        self.check(&key.brand)?;
        self.buffers[key.index].as_ref().ok_or(DoneError::Taken)
    }

    /// Moves the file `key` names out, so a caller who owns the result need not
    /// copy it; the other results stay.
    ///
    /// # Errors
    ///
    /// As [`file`](Self::file); a second take is [`DoneError::Taken`].
    pub fn take_file(&mut self, key: &FileKey) -> Result<Written, DoneError> {
        self.check(&key.brand)?;
        self.files[key.index].take().ok_or(DoneError::Taken)
    }

    /// Moves the planes `key` names out; the other results stay.
    ///
    /// # Errors
    ///
    /// As [`buffers`](Self::buffers); a second take is [`DoneError::Taken`].
    pub fn take_buffers(&mut self, key: &BufferKey) -> Result<Rendered<F>, DoneError> {
        self.check(&key.brand)?;
        self.buffers[key.index].take().ok_or(DoneError::Taken)
    }

    fn check(&self, brand: &Brand) -> Result<(), DoneError> {
        if *brand == self.brand {
            Ok(())
        } else {
            Err(DoneError::Foreign)
        }
    }
}
