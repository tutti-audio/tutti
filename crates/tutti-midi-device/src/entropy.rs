//! The operating system's random bytes, taken without panicking: an
//! [`EntropyError`] when the system has none to give.

/// The operating system has no random bytes to give.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the operating system has no random bytes: {0}")]
pub struct EntropyError(#[from] getrandom::Error);

/// `N` random bytes from the operating system.
///
/// # Errors
///
/// [`EntropyError`] when the operating system has no random bytes to give.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], EntropyError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes)?;
    Ok(bytes)
}

/// A stream of random 32-bit words, seeded once from the operating system: the
/// source of the MUIDs of a [`CiDevice`](crate::CiDevice), which asks for a
/// word as often as a collision needs one and so cannot fail at each.
///
/// The words are SplitMix64's high halves: well spread, not secret.
#[derive(Clone, Debug)]
pub struct EntropyWords {
    state: u64,
}

impl EntropyWords {
    /// A stream with a seed from the operating system.
    ///
    /// # Errors
    ///
    /// [`EntropyError`] when the operating system has no random bytes to give.
    pub fn from_os() -> Result<Self, EntropyError> {
        Ok(Self {
            state: u64::from_le_bytes(random_bytes()?),
        })
    }

    /// The next word.
    pub fn next_word(&mut self) -> u32 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 32) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // SplitMix64 from seed 0 (Steele, Lea and Flood 2014): the first output is
    // 0xE220A8397B1DCDAF, so its high half is the first word.
    #[test]
    fn the_words_are_splitmix64_high_halves() {
        let mut words = EntropyWords { state: 0 };
        assert_eq!(words.next_word(), 0xE220_A839);
    }

    #[test]
    fn two_streams_from_the_os_differ() {
        let (mut a, mut b) = (
            EntropyWords::from_os().unwrap(),
            EntropyWords::from_os().unwrap(),
        );
        let (a, b): (Vec<u32>, Vec<u32>) = (0..4).map(|_| (a.next_word(), b.next_word())).unzip();
        assert_ne!(a, b);
    }
}
