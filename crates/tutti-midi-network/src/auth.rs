//! The authentication values of an Invitation (M2-124 sections 6.7 to 6.10):
//! the [`Nonce`] a Host issues and the [`Digest`] a Client answers with.

use sha2::{Digest as _, Sha256};
use tutti_midi_device::{EntropyError, random_bytes};

use crate::Username;

/// The first printable ASCII character, the space.
const PRINTABLE_FIRST: u8 = 32;
/// The printable ASCII characters, `32..=126`.
const PRINTABLE_COUNT: u8 = 95;
/// The random bytes below this map onto the printable characters evenly: twice
/// [`PRINTABLE_COUNT`].
const UNBIASED_BELOW: u8 = 2 * PRINTABLE_COUNT;

/// The 16 bytes of printable ASCII (`32..=126`) a Host issues to a Client that
/// must authenticate (M2-124 section 6.7).
///
/// A Client hashes it with its secret, so a digest from an earlier session
/// does not authenticate a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Nonce([u8; 16]);

impl Nonce {
    /// The nonce with these bytes, or `None` when one is outside `32..=126`.
    ///
    /// The nonces this crate issues follow the specification's range for a
    /// sender (M2-124 section 6.7). A nonce received from a peer is read
    /// leniently: any ASCII is taken.
    #[must_use]
    pub fn new(bytes: [u8; 16]) -> Option<Self> {
        bytes
            .iter()
            .all(|b| (PRINTABLE_FIRST..PRINTABLE_FIRST + PRINTABLE_COUNT).contains(b))
            .then_some(Self(bytes))
    }

    /// A nonce a peer sent: any ASCII bytes. The range is a requirement on the
    /// sender, and a received nonce is only hashed with the secret, so reading
    /// one outside it costs nothing; the Client answers with the digest of the
    /// bytes it was given.
    pub(crate) fn from_wire(bytes: [u8; 16]) -> Option<Self> {
        bytes.is_ascii().then_some(Self(bytes))
    }

    /// A nonce drawn from blocks of random bytes: each byte below 190 picks
    /// the printable character `b % 95` above the space, and every other byte
    /// is skipped, so the 95 characters are equally likely (190 is two whole
    /// turns of 95; a modulo over all 256 values would favour the first 66).
    /// More blocks are asked for until 16 characters are picked.
    ///
    /// # Errors
    ///
    /// The error of `next_block`, which ends the draw.
    pub fn draw<E>(mut next_block: impl FnMut() -> Result<[u8; 16], E>) -> Result<Self, E> {
        let mut nonce = [0u8; 16];
        let mut filled = 0;
        while filled < nonce.len() {
            for byte in next_block()? {
                if byte < UNBIASED_BELOW && filled < nonce.len() {
                    nonce[filled] = PRINTABLE_FIRST + byte % PRINTABLE_COUNT;
                    filled += 1;
                }
            }
        }
        Ok(Self(nonce))
    }

    /// A nonce from the operating system's random bytes.
    ///
    /// # Errors
    ///
    /// [`EntropyError`] when the operating system has no random bytes to give.
    pub fn random() -> Result<Self, EntropyError> {
        Self::draw(random_bytes::<16>)
    }

    /// The 16 bytes.
    #[must_use]
    pub const fn get(&self) -> [u8; 16] {
        self.0
    }
}

/// A SHA-256 authentication digest (M2-124 sections 6.9 and 6.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// A digest from its 32 bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The 32 bytes.
    #[must_use]
    pub const fn get(&self) -> [u8; 32] {
        self.0
    }

    /// The digest of the nonce followed by a shared secret.
    #[must_use]
    pub fn of_secret(nonce: &Nonce, secret: &str) -> Self {
        Self::hash(&[&nonce.0, secret.as_bytes()])
    }

    /// The digest of the nonce, the user name and the password, in that order.
    #[must_use]
    pub fn of_user(nonce: &Nonce, username: &Username, password: &str) -> Self {
        Self::hash(&[&nonce.0, username.as_str().as_bytes(), password.as_bytes()])
    }

    /// Whether the digests are equal, comparing every byte whatever the
    /// first difference, so the time taken does not say how much matched.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(&other.0)
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }

    fn hash(parts: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part);
        }
        Self(hasher.finalize().into())
    }
}
