//! FNV-1a, 64 bits: the digest a backend names a device by when the OS gives a string,
//! not a number, that stays the same across a replug.

/// The 64-bit FNV-1a digest of `bytes`.
pub(super) fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes
        .into_iter()
        .fold(OFFSET, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME))
}

#[cfg(test)]
mod tests;
