//! A source that opens again and again.

use std::fmt;
use std::io::{self, Cursor, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::file_stamp::FileStamp;

/// A reader that seeks and moves between threads: what [`Open::open`] hands out.
///
/// Every `Read + Seek + Send` type is one.
pub trait ReadSeek: Read + Seek + Send {}

impl<T: Read + Seek + Send + ?Sized> ReadSeek for T {}

/// A cache key for one source: two equal identities name the same content.
///
/// It is opaque. A path's identity carries its [`FileStamp`], so a file that changed
/// is another identity; a source of bytes carries an id minted once per source,
/// never an address, so a freed source's key is not reused.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Identity(Key);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    File(PathBuf, FileStamp),
    Minted(u64),
}

impl Identity {
    /// A new identity, equal to no other: for a source of bytes that does not
    /// change, which mints it once and returns it from [`Open::identity`].
    #[must_use]
    pub fn minted() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(Key::Minted(NEXT.fetch_add(1, Ordering::Relaxed)))
    }

    /// The identity of the file at `path`, as it is now.
    fn of_file(path: &Path) -> io::Result<Self> {
        Ok(Self(Key::File(path.to_path_buf(), FileStamp::of(path)?)))
    }
}

/// A source of bytes that can be opened again and again.
///
/// Each [`open`](Self::open) gives a fresh reader, independent of the others and
/// positioned at the start. A clip is opened to probe it, again for the live
/// streamer, and again for every offline fork; a single reader would serve only
/// the first. A source is held as an `Arc<dyn Open>` inside an [`Asset`], so an
/// app's own source goes wherever a path does. It is for:
///
/// - HTTP range fetches, where each open starts a new ranged request;
/// - an asset pack, where each open is a view of one entry;
/// - a browser `File`, where each open is a new reader of the same blob;
/// - an engine's already-loaded bytes, where each open is a cursor over a
///   shared buffer (an [`Asset`] made from `Arc<[u8]>` is this).
pub trait Open: Send + Sync {
    /// A fresh reader of the source, positioned at its start.
    ///
    /// # Errors
    ///
    /// The source cannot be read: a file that is gone, a request that failed,
    /// or a target with no filesystem.
    fn open(&self) -> io::Result<Box<dyn ReadSeek>>;

    /// The source's file extension (`"wav"`), which a decoder uses to probe a
    /// container. `None`, the default, probes from the bytes alone.
    fn extension(&self) -> Option<&str> {
        None
    }

    /// The source's name, for an error message to say what failed.
    fn name(&self) -> String;

    /// The key a cache holds this source's decode under. `None`, the default,
    /// is a source nothing caches, and that a pool refresh opens to check it
    /// can still be read: for a source behind a network, every refresh is one
    /// open.
    fn identity(&self) -> Option<Identity> {
        None
    }
}

/// A file, opened as a `File` on every call.
struct FileSource {
    path: PathBuf,
}

impl Open for FileSource {
    fn open(&self) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(std::fs::File::open(&self.path)?))
    }

    fn extension(&self) -> Option<&str> {
        self.path.extension().and_then(|e| e.to_str())
    }

    fn name(&self) -> String {
        self.path.display().to_string()
    }

    fn identity(&self) -> Option<Identity> {
        Identity::of_file(&self.path).ok()
    }
}

/// Shared bytes, opened as a `Cursor` over a clone of the `Arc`: no copy.
struct BytesSource {
    bytes: Arc<[u8]>,
    id: Identity,
}

impl Open for BytesSource {
    fn open(&self) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(Cursor::new(Arc::clone(&self.bytes))))
    }

    fn name(&self) -> String {
        format!("{} bytes in memory", self.bytes.len())
    }

    fn identity(&self) -> Option<Identity> {
        Some(self.id.clone())
    }
}

/// A source of audio, a SoundFont or a MIDI file, as every entry point takes it
/// (`impl Into<Asset>`).
///
/// It is made from a path ([`&Path`](Path), [`PathBuf`], `&str`), from bytes
/// (`Arc<[u8]>`, `Vec<u8>`) or from an app's own [`Open`] (`Arc<dyn Open>`), and
/// is cheap to clone: clones share the source and keep the [`identity`](Self::identity).
///
/// - A path opens a `File` on every [`open`](Self::open). Its identity is the
///   path with the file's [`FileStamp`] as of the call that made the asset, so a file
///   edited since is another asset; it is `None` when the file cannot be stamped.
/// - Bytes open a cursor over the shared buffer, with no copy. Their identity is
///   minted when the asset is made, so two assets of equal bytes differ and a
///   clone of one does not.
/// - An `Arc<dyn Open>` asks the source for its [`identity`](Open::identity).
///
/// ```
/// use tutti_platform::Asset;
///
/// let asset = Asset::from(vec![1u8, 2, 3]).hint("wav");
/// assert_eq!(asset.extension(), Some("wav"));
/// let (a, b) = (asset.open()?, asset.open()?); // independent readers
/// # let _ = (a, b);
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Clone)]
pub struct Asset {
    source: Arc<dyn Open>,
    key: Arc<SourceKey>,
    hint: Option<Arc<str>>,
}

/// What two assets are equal by: their source, with no I/O.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SourceKey {
    File(PathBuf),
    /// The id the bytes' source holds.
    Minted(Identity),
    /// The data address of an app's own source, which the asset keeps alive.
    Pointer(usize),
}

impl Asset {
    fn new(source: Arc<dyn Open>, key: SourceKey) -> Self {
        Self {
            source,
            key: Arc::new(key),
            hint: None,
        }
    }

    /// Sets the file extension a decoder probes with, in place of the source's
    /// own: bytes carry none, and a path's is its own.
    #[must_use]
    pub fn hint(mut self, extension: &str) -> Self {
        self.hint = Some(extension.into());
        self
    }

    /// A fresh reader of the source, positioned at its start; see [`Open::open`].
    ///
    /// # Errors
    ///
    /// As [`Open::open`].
    pub fn open(&self) -> io::Result<Box<dyn ReadSeek>> {
        self.source.open()
    }

    /// The file extension to probe with: [`hint`](Self::hint)'s, else
    /// the source's.
    #[must_use]
    pub fn extension(&self) -> Option<&str> {
        self.hint.as_deref().or_else(|| self.source.extension())
    }

    /// The source's name, for an error message.
    #[must_use]
    pub fn name(&self) -> String {
        self.source.name()
    }

    /// The cache key, taken now; see [`Open::identity`]. A file's key is its path and
    /// its current [`FileStamp`], so a file changed since the last call has a new key;
    /// `None` for a source that is not cached.
    #[must_use]
    pub fn identity(&self) -> Option<Identity> {
        self.source.identity()
    }
}

/// Two assets are equal when they are the same source, decided without I/O:
///
/// - paths are equal when their `PathBuf`s are, so two assets of one path are
///   equal whatever the file holds now. The comparison is lexical, with no
///   canonicalisation: `a/./kick.wav` and `a/kick.wav` are two assets;
/// - bytes are equal when one is a clone of the other: the id minted when the
///   bytes were wrapped is the key, so two assets of equal bytes are not;
/// - an `Arc<dyn Open>` is equal to another of the same `Arc` (its data
///   address). The asset keeps its `Arc` alive, so the address is not reused
///   while an asset is compared with it.
///
/// The [`hint`](Asset::hint) is not part of it.
impl PartialEq for Asset {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Asset {}

/// Hashes the same source key [`PartialEq`] compares.
impl std::hash::Hash for Asset {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl fmt::Debug for Asset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Asset")
            .field("name", &self.source.name())
            .field("extension", &self.extension())
            .finish()
    }
}

impl From<PathBuf> for Asset {
    fn from(path: PathBuf) -> Self {
        let key = SourceKey::File(path.clone());
        Self::new(Arc::new(FileSource { path }), key)
    }
}

impl From<&Path> for Asset {
    fn from(path: &Path) -> Self {
        path.to_path_buf().into()
    }
}

impl From<&PathBuf> for Asset {
    fn from(path: &PathBuf) -> Self {
        path.clone().into()
    }
}

impl From<&str> for Asset {
    fn from(path: &str) -> Self {
        PathBuf::from(path).into()
    }
}

impl From<Arc<[u8]>> for Asset {
    fn from(bytes: Arc<[u8]>) -> Self {
        let id = Identity::minted();
        let key = SourceKey::Minted(id.clone());
        Self::new(Arc::new(BytesSource { bytes, id }), key)
    }
}

impl From<Vec<u8>> for Asset {
    fn from(bytes: Vec<u8>) -> Self {
        Arc::<[u8]>::from(bytes).into()
    }
}

impl From<Arc<dyn Open>> for Asset {
    fn from(source: Arc<dyn Open>) -> Self {
        let key = SourceKey::Pointer(Arc::as_ptr(&source).cast::<()>() as usize);
        Self::new(source, key)
    }
}

#[cfg(test)]
mod tests;
