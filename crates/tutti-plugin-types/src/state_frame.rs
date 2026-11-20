//! The check a host makes before it reads a saved plugin state as its own container.
//!
//! A format whose host wraps the plugin's state in a container of its own (VST2's four-byte
//! tags, VST3's two-stream magic) opens it with a frame. A blob that begins with one of the
//! frame's tags is framed, and everything after is read through a [`FrameReader`], which
//! refuses a short or oversized field by name. A framed blob that does not read is never
//! handed to the plugin as an unframed one.
//!
//! A blob that begins with no tag is *unframed*, and the two frame types differ in what that
//! is. [`StateFrame`] refuses it: the format has no state but the host's own. [`BareFrame`]
//! accepts it as the plugin's own stream, and is constructed only where the format's spec
//! defines no container, so the plugin's stream stands without one; the rustdoc of each
//! such frame names the rule.
//!
//! CLAP and AU have no container: the plugin's state is the stream the plugin wrote, so a
//! host passes it through and has no frame to open.

use thiserror::Error;

/// The most one plugin state holds, in bytes: 1 GiB.
///
/// Every layer that accumulates a state a plugin or a peer sends (a format's
/// stream, the object a state travels in between processes, the
/// server's reply) refuses what passes it. It sits above the real tail and
/// below anything that threatens the host: the largest states in practice are
/// sample-embedding instruments and convolution reverbs, which reach high
/// hundreds of MiB, because the formats stream rather than inline beyond that.
/// A larger blob is a plugin misbehaving or a corrupt project, and refusing it
/// (`StateError::TooLarge`) is survivable, unlike an allocation the host
/// cannot satisfy.
pub const MAX_STATE_BYTES: usize = 1 << 30;

/// A saved state read through a [`StateFrame`] that did not read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum StateFrameError {
    /// The blob begins with none of the frame's tags and the format accepts no unframed state.
    #[error("the {format} state carries none of the tags a saved state begins with")]
    Unframed {
        /// The format whose frame refused it.
        format: &'static str,
    },
    /// A framed blob ends inside a field.
    #[error("the {format} state is cut short: {field} is missing")]
    Truncated {
        /// The format whose frame was read.
        format: &'static str,
        /// The field that did not fit.
        field: &'static str,
    },
    /// A framed blob names a container version this host does not read.
    #[error("the {format} state is container version {found}, which this host does not read")]
    UnknownVersion {
        /// The format whose frame was read.
        format: &'static str,
        /// The version byte the blob carries.
        found: u8,
    },
    /// A length field in a framed blob claims more bytes than the blob holds.
    #[error("the {format} state's {field} claims {claimed} bytes and {available} remain")]
    LengthPastEnd {
        /// The format whose frame was read.
        format: &'static str,
        /// The field whose length is wrong.
        field: &'static str,
        /// The bytes the field claims.
        claimed: u64,
        /// The bytes left in the blob.
        available: usize,
    },
    /// A part of a state is longer than the container's length field can describe.
    #[error("the {format} state's {field} is {len} bytes, more than its container describes")]
    TooLarge {
        /// The format whose frame was written.
        format: &'static str,
        /// The field that does not fit.
        field: &'static str,
        /// Its length in bytes.
        len: u64,
    },
    /// A framed blob holds a value its format does not allow.
    #[error("the {format} state's {field} is invalid: {reason}")]
    Invalid {
        /// The format whose frame was read.
        format: &'static str,
        /// The field that is wrong.
        field: &'static str,
        /// What is wrong with it.
        reason: String,
    },
}

/// What [`BareFrame::open`] found at the start of a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened<'a> {
    /// The blob begins with `tags[tag]`; `body` is what follows it.
    Framed {
        /// The index in the frame's tags of the one that matched.
        tag: usize,
        /// The bytes after the tag.
        body: &'a [u8],
    },
    /// The blob begins with no tag: the whole blob is the plugin's own stream.
    Bare(&'a [u8]),
}

/// Whether no tag of `tags` is a prefix of another (or equal to it): the property that makes
/// the tag a blob begins with unique.
const fn tags_are_prefix_free(tags: &[&[u8]]) -> bool {
    let mut a = 0;
    while a < tags.len() {
        let mut b = 0;
        while b < tags.len() {
            if a != b && is_prefix(tags[a], tags[b]) {
                return false;
            }
            b += 1;
        }
        a += 1;
    }
    true
}

const fn is_prefix(prefix: &[u8], of: &[u8]) -> bool {
    if prefix.len() > of.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if prefix[i] != of[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// The index of the tag `data` begins with.
fn tag_of(tags: &[&[u8]], data: &[u8]) -> Option<(usize, usize)> {
    tags.iter()
        .position(|tag| data.starts_with(tag))
        .map(|at| (at, tags[at].len()))
}

/// A frame that refuses an unframed blob: every state the format's host writes begins with
/// one of its tags, and a blob without one is not a state of it.
#[derive(Debug, Clone, Copy)]
pub struct StateFrame {
    format: &'static str,
    tags: &'static [&'static [u8]],
}

impl StateFrame {
    /// A frame for `format` whose blobs begin with one of `tags`.
    ///
    /// # Panics
    ///
    /// At compile time, in a `const`, when a tag is a prefix of another.
    #[must_use]
    pub const fn new(format: &'static str, tags: &'static [&'static [u8]]) -> Self {
        assert!(
            tags_are_prefix_free(tags),
            "a state tag is a prefix of another"
        );
        Self { format, tags }
    }

    /// The index of the tag `data` begins with, and what follows it.
    ///
    /// # Errors
    ///
    /// [`StateFrameError::Unframed`] when the blob begins with no tag.
    pub fn open<'a>(&self, data: &'a [u8]) -> Result<(usize, &'a [u8]), StateFrameError> {
        match tag_of(self.tags, data) {
            Some((tag, len)) => Ok((tag, &data[len..])),
            None => Err(StateFrameError::Unframed {
                format: self.format,
            }),
        }
    }

    /// A reader over the body of a framed blob, whose errors name this frame's format.
    #[must_use]
    pub const fn reader<'a>(&self, body: &'a [u8]) -> FrameReader<'a> {
        FrameReader {
            format: self.format,
            rest: body,
        }
    }
}

/// A frame that accepts an unframed blob as the plugin's own stream.
///
/// Constructed only for a format whose spec defines no state container, so the plugin's
/// stream is a state without one; the host's own container is the one framed form.
#[derive(Debug, Clone, Copy)]
pub struct BareFrame {
    format: &'static str,
    tags: &'static [&'static [u8]],
}

impl BareFrame {
    /// A frame for `format` whose framed blobs begin with one of `tags`.
    ///
    /// # Panics
    ///
    /// At compile time, in a `const`, when a tag is a prefix of another.
    #[must_use]
    pub const fn new(format: &'static str, tags: &'static [&'static [u8]]) -> Self {
        assert!(
            tags_are_prefix_free(tags),
            "a state tag is a prefix of another"
        );
        Self { format, tags }
    }

    /// The format's name, as its errors spell it.
    #[must_use]
    pub const fn format(&self) -> &'static str {
        self.format
    }

    /// Separates a framed blob from the plugin's own bare stream.
    #[must_use]
    pub fn open<'a>(&self, data: &'a [u8]) -> Opened<'a> {
        match tag_of(self.tags, data) {
            Some((tag, len)) => Opened::Framed {
                tag,
                body: &data[len..],
            },
            None => Opened::Bare(data),
        }
    }

    /// A reader over the body of a framed blob, whose errors name this frame's format.
    #[must_use]
    pub const fn reader<'a>(&self, body: &'a [u8]) -> FrameReader<'a> {
        FrameReader {
            format: self.format,
            rest: body,
        }
    }
}

/// A cursor over the body of a framed state, whose every read is checked.
#[derive(Debug, Clone, Copy)]
pub struct FrameReader<'a> {
    format: &'static str,
    rest: &'a [u8],
}

impl<'a> FrameReader<'a> {
    /// The next byte.
    ///
    /// # Errors
    ///
    /// [`StateFrameError::Truncated`] when none is left.
    pub fn u8(&mut self, field: &'static str) -> Result<u8, StateFrameError> {
        let (&byte, rest) = self.rest.split_first().ok_or(self.truncated(field))?;
        self.rest = rest;
        Ok(byte)
    }

    /// The next four bytes as a little-endian `u32`.
    ///
    /// # Errors
    ///
    /// [`StateFrameError::Truncated`] when fewer than four are left.
    pub fn u32_le(&mut self, field: &'static str) -> Result<u32, StateFrameError> {
        let (word, rest) = self
            .rest
            .split_first_chunk::<4>()
            .ok_or(self.truncated(field))?;
        self.rest = rest;
        Ok(u32::from_le_bytes(*word))
    }

    /// The next four bytes as a little-endian `i32`.
    ///
    /// # Errors
    ///
    /// [`StateFrameError::Truncated`] when fewer than four are left.
    pub fn i32_le(&mut self, field: &'static str) -> Result<i32, StateFrameError> {
        self.u32_le(field).map(|word| word as i32)
    }

    /// The next `len` bytes.
    ///
    /// # Errors
    ///
    /// [`StateFrameError::LengthPastEnd`] when fewer are left.
    pub fn bytes(&mut self, len: u64, field: &'static str) -> Result<&'a [u8], StateFrameError> {
        let split = usize::try_from(len)
            .ok()
            .and_then(|len| self.rest.split_at_checked(len));
        let Some((taken, rest)) = split else {
            return Err(StateFrameError::LengthPastEnd {
                format: self.format,
                field,
                claimed: len,
                available: self.rest.len(),
            });
        };
        self.rest = rest;
        Ok(taken)
    }

    /// Everything not yet read.
    pub fn take_rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.rest)
    }

    /// An [`Invalid`](StateFrameError::Invalid) error for `field`.
    #[must_use]
    pub fn invalid(&self, field: &'static str, reason: impl Into<String>) -> StateFrameError {
        StateFrameError::Invalid {
            format: self.format,
            field,
            reason: reason.into(),
        }
    }

    fn truncated(&self, field: &'static str) -> StateFrameError {
        StateFrameError::Truncated {
            format: self.format,
            field,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BARE: BareFrame = BareFrame::new("TEST", &[b"AAA\x01", b"BBB\x01"]);
    const STRICT: StateFrame = StateFrame::new("TEST", &[b"AAA\x01"]);

    /// A blob opens on the tag it begins with and hands back what follows, whichever tag.
    #[test]
    fn a_tagged_blob_opens_on_its_tag() {
        assert_eq!(
            BARE.open(b"BBB\x01rest"),
            Opened::Framed {
                tag: 1,
                body: b"rest"
            }
        );
        assert_eq!(BARE.open(b"AAA\x01"), Opened::Framed { tag: 0, body: b"" });
        assert_eq!(STRICT.open(b"AAA\x01rest"), Ok((0, &b"rest"[..])));
    }

    /// A blob with no tag is the plugin's stream in a bare frame, and refused by name in a
    /// strict one; a blob shorter than a tag is the same case.
    #[test]
    fn an_untagged_blob_is_bare_or_refused_by_the_frame() {
        assert_eq!(BARE.open(b"plain"), Opened::Bare(b"plain"));
        assert_eq!(BARE.open(b""), Opened::Bare(b""));
        assert_eq!(
            STRICT.open(b"plain"),
            Err(StateFrameError::Unframed { format: "TEST" })
        );
        assert_eq!(
            STRICT.open(b"AA"),
            Err(StateFrameError::Unframed { format: "TEST" })
        );
        assert_eq!(
            STRICT.open(b"plain").unwrap_err().to_string(),
            "the TEST state carries none of the tags a saved state begins with"
        );
    }

    /// A tag that is a prefix of another, or a repeat of it, is not prefix-free; distinct
    /// tags of one length and tags that differ before their ends are.
    #[test]
    fn tags_must_be_prefix_free() {
        assert!(tags_are_prefix_free(&[b"CHK\x01", b"CHK\0", b"PRM\x01"]));
        assert!(tags_are_prefix_free(&[]));
        assert!(!tags_are_prefix_free(&[b"CHK", b"CHK\x01"]));
        assert!(!tags_are_prefix_free(&[b"CHK\x01", b"CHK"]));
        assert!(!tags_are_prefix_free(&[b"CHK", b"CHK"]));
        assert!(!tags_are_prefix_free(&[b"", b"A"]));
    }

    /// A reader refuses a field that does not fit and a length past the end, each by name.
    #[test]
    fn a_reader_refuses_a_short_field_and_a_length_past_the_end() {
        let mut r = STRICT.reader(&[7, 1, 2]);
        assert_eq!(r.u8("version"), Ok(7));
        assert_eq!(
            r.u32_le("count"),
            Err(StateFrameError::Truncated {
                format: "TEST",
                field: "count"
            })
        );
        assert_eq!(
            r.bytes(3, "payload"),
            Err(StateFrameError::LengthPastEnd {
                format: "TEST",
                field: "payload",
                claimed: 3,
                available: 2
            })
        );
        assert_eq!(r.bytes(2, "payload"), Ok(&[1u8, 2][..]));
        assert_eq!(r.take_rest(), b"");
        assert!(matches!(
            STRICT.reader(&[]).u8("version"),
            Err(StateFrameError::Truncated { .. })
        ));
        assert_eq!(STRICT.reader(&[0xFE, 0xFF, 0xFF, 0xFF]).i32_le("n"), Ok(-2));
        // A length no `usize` holds is past the end, not a wrap.
        assert!(matches!(
            STRICT.reader(&[1]).bytes(u64::MAX, "payload"),
            Err(StateFrameError::LengthPastEnd { .. })
        ));
    }
}
