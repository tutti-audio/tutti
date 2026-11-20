//! Splitting and reassembling a plugin state across control-socket frames.
//!
//! State is the one message whose size a *plugin* chooses rather than the
//! protocol, and it does not fit the frame cap: a sample-embedding instrument
//! or a convolution reverb carrying an impulse response routinely exceeds
//! 64 MiB. So state travels as chunks of at most
//! [`STATE_CHUNK_BYTES`]: each frame stays
//! small, and the *total* is bounded separately by [`MAX_STATE_BYTES`]: every
//! chunk carries it, so the receiver sizes its buffer once and refuses a total
//! over the limit before it allocates.
//!
//! Both directions use the same two helpers, because the hazard is symmetric:
//! the host reassembles what a plugin produced, and the server reassembles what
//! a project file supplied. Neither end may trust the other's sequencing.

use crate::STATE_CHUNK_BYTES;
use tutti_plugin_types::MAX_STATE_BYTES;

/// Splits `data` into wire-sized pieces, as `(seq, last, bytes)`.
///
/// **An empty state yields one empty chunk, not zero chunks.** A receiver waits
/// for `last`, so a zero-chunk sequence would never terminate — the reader
/// would sit until its deadline and report a timeout for a plugin that simply
/// had nothing to save. This is the case a `chunks()` call alone gets wrong,
/// since `[].chunks(n)` yields nothing.
pub fn split(data: &[u8]) -> impl Iterator<Item = (u32, bool, &[u8])> {
    let total = data.len().div_ceil(STATE_CHUNK_BYTES).max(1);
    (0..total).map(move |i| {
        let start = i * STATE_CHUNK_BYTES;
        let end = ((i + 1) * STATE_CHUNK_BYTES).min(data.len());
        // `start > end` only for the empty-input case, where `total` was forced
        // to 1 above and the slice must be empty rather than a panic.
        let bytes = if start >= data.len() {
            &data[0..0]
        } else {
            &data[start..end]
        };
        (i as u32, i + 1 == total, bytes)
    })
}

/// Accumulates a chunk sequence, refusing a malformed or oversized one.
///
/// Stateful rather than a fold over a collected `Vec` because the whole point
/// is to stop *before* the bytes are all in memory. The first chunk's `total`
/// is checked against the limit and reserved in one fallible allocation
/// ([`Vec::try_reserve_exact`]); no later chunk grows the buffer.
#[derive(Debug)]
pub struct Reassembler {
    buf: Vec<u8>,
    next_seq: u32,
    done: bool,
    /// The `total` the first chunk declared, which every chunk repeats.
    total: u64,
    /// Ceiling on the reassembled total. A field rather than a direct read of
    /// `MAX_STATE_BYTES` so the refusal path is reachable from a test without
    /// allocating a gigabyte to reach it.
    limit: usize,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::with_limit(MAX_STATE_BYTES)
    }
}

/// Why a chunk sequence was refused.
///
/// Separate from `StateError` because this layer does not know which direction
/// it is serving; the caller maps it to the right typed error for its side.
///
/// Exhaustive: the plugin server maps each cause to its own message.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    /// A chunk arrived out of order, repeated, or after `last`. The sequence is
    /// not what the sender believes, and reassembling anyway would hand the
    /// plugin a corrupt blob to parse.
    #[error("state chunk {got} arrived where {expected} was expected")]
    OutOfOrder {
        /// The `seq` that arrived.
        got: u32,
        /// The `seq` that was required.
        expected: u32,
    },
    /// A chunk before `last` carried no bytes. [`split`] never sends one, and
    /// a sequence of them makes no progress: a peer could keep a reader taking
    /// them forever while the byte limit never trips. Refusing
    /// them bounds a sequence by its bytes.
    #[error("state chunk {seq} is empty and not the last")]
    Empty {
        /// The `seq` of the empty chunk.
        seq: u32,
    },
    /// The declared total exceeds [`MAX_STATE_BYTES`] (or the reassembler's
    /// limit): refused on the first chunk, before any allocation.
    #[error("state is {bytes} bytes, over the {limit}-byte limit")]
    TooLarge {
        /// The declared total.
        bytes: usize,
        /// The limit it exceeded.
        limit: usize,
    },
    /// The buffer for the declared total could not be allocated.
    #[error("no room for a state of {bytes} bytes")]
    NoRoom {
        /// The declared total.
        bytes: usize,
    },
    /// A chunk's `total` is not the one the first chunk declared.
    #[error("state chunk declares a total of {got} bytes where {expected} were declared")]
    TotalChanged {
        /// The total the first chunk declared.
        expected: u64,
        /// The total this chunk declares.
        got: u64,
    },
    /// The chunks carry more bytes than the declared total, or the last one
    /// ends short of it.
    #[error("state chunks carry {got} bytes where {total} were declared")]
    NotTotal {
        /// The bytes the chunks carry, up to the one that broke it.
        got: usize,
        /// The declared total.
        total: u64,
    },
}

impl Reassembler {
    /// Creates a reassembler bounded by `limit` rather than [`MAX_STATE_BYTES`].
    ///
    /// [`Default`] uses [`MAX_STATE_BYTES`]; a smaller limit makes the
    /// over-limit path testable at a size a test can build.
    #[must_use]
    pub(crate) fn with_limit(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            next_seq: 0,
            done: false,
            total: 0,
            limit,
        }
    }

    /// Takes one chunk, returning `Ok(true)` when the sequence is complete.
    ///
    /// # Errors
    ///
    /// [`ChunkError::OutOfOrder`] for a chunk whose `seq` is not the next
    /// expected one (or that arrives after `last`); [`ChunkError::Empty`] for
    /// a chunk before `last` with no bytes; [`ChunkError::TooLarge`] and
    /// [`ChunkError::NoRoom`] on the first chunk, for a total over the limit
    /// or one that cannot be reserved; [`ChunkError::TotalChanged`] for a
    /// chunk whose `total` differs from the first's; [`ChunkError::NotTotal`]
    /// for bytes past the total, or a `last` chunk that ends short of it.
    pub fn push(
        &mut self,
        seq: u32,
        total: u64,
        last: bool,
        bytes: &[u8],
    ) -> Result<bool, ChunkError> {
        if self.done || seq != self.next_seq {
            return Err(ChunkError::OutOfOrder {
                got: seq,
                expected: self.next_seq,
            });
        }
        if bytes.is_empty() && !last {
            return Err(ChunkError::Empty { seq });
        }
        if seq == 0 {
            let wanted = usize::try_from(total).unwrap_or(usize::MAX);
            if wanted > self.limit {
                return Err(ChunkError::TooLarge {
                    bytes: wanted,
                    limit: self.limit,
                });
            }
            self.buf
                .try_reserve_exact(wanted)
                .map_err(|_| ChunkError::NoRoom { bytes: wanted })?;
            self.total = total;
        } else if total != self.total {
            return Err(ChunkError::TotalChanged {
                expected: self.total,
                got: total,
            });
        }
        let held = self.buf.len().saturating_add(bytes.len());
        if held as u64 > self.total || (last && held as u64 != self.total) {
            return Err(ChunkError::NotTotal {
                got: held,
                total: self.total,
            });
        }
        self.buf.extend_from_slice(bytes);
        self.next_seq += 1;
        self.done = last;
        Ok(last)
    }

    /// The reassembled bytes. Call after [`push`](Self::push) returned
    /// `Ok(true)`.
    #[must_use]
    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sequence that has `data` as its whole state.
    fn feed(r: &mut Reassembler, data: &[u8]) -> Result<bool, ChunkError> {
        let mut done = Ok(false);
        for (seq, last, bytes) in split(data) {
            done = r.push(seq, data.len() as u64, last, bytes);
            done.as_ref()
                .map_err(|e| format!("{e}"))
                .expect("well-formed");
        }
        done
    }

    #[test]
    fn an_empty_state_is_one_terminating_chunk() {
        let chunks: Vec<_> = split(&[]).collect();
        assert_eq!(
            chunks.len(),
            1,
            "an empty state produced {} chunks; a receiver waits for `last`, so \
             zero chunks is a hang rather than an empty answer",
            chunks.len()
        );
        assert_eq!(chunks[0].0, 0, "first chunk must be seq 0");
        assert!(
            chunks[0].1,
            "the only chunk of an empty state must be `last`"
        );
        assert!(chunks[0].2.is_empty(), "an empty state must carry no bytes");
    }

    #[test]
    fn split_then_reassemble_round_trips() {
        for len in [
            0,
            1,
            STATE_CHUNK_BYTES - 1,
            STATE_CHUNK_BYTES,
            STATE_CHUNK_BYTES + 1,
            STATE_CHUNK_BYTES * 2 + 7,
        ] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut r = Reassembler::default();
            assert_eq!(feed(&mut r, &data), Ok(true), "len {len} never set `last`");
            assert_eq!(
                r.into_inner(),
                data,
                "round trip changed the bytes at len {len}"
            );
        }
    }

    /// A gap in the sequence is refused rather than silently closed.
    #[test]
    fn a_skipped_chunk_is_refused() {
        let mut r = Reassembler::default();
        r.push(0, 3, false, b"a").expect("first chunk");
        assert_eq!(
            r.push(2, 3, true, b"c"),
            Err(ChunkError::OutOfOrder {
                got: 2,
                expected: 1
            }),
            "a skipped chunk was accepted: the plugin would receive a blob \
             with a hole in it and try to parse it"
        );
    }

    /// A chunk after `last` is refused.
    ///
    /// Distinct from a gap: the sequence numbers are contiguous, so only the
    /// `done` latch catches it.
    #[test]
    fn a_chunk_after_last_is_refused() {
        let mut r = Reassembler::default();
        assert!(r.push(0, 1, true, b"a").expect("first chunk"));
        assert!(
            matches!(r.push(1, 1, true, b"b"), Err(ChunkError::OutOfOrder { .. })),
            "a chunk after `last` was accepted, extending a completed state"
        );
    }

    /// **A total over the limit is refused on the first chunk, and nothing is
    /// allocated for it**: the buffer's capacity is what is asserted, as an
    /// implementation that reserves first and checks after returns the same
    /// error having already allocated.
    #[test]
    fn a_total_over_the_limit_is_refused_before_it_is_allocated() {
        let mut r = Reassembler::with_limit(10);
        assert_eq!(
            r.push(0, 11, false, &[0u8; 4]),
            Err(ChunkError::TooLarge {
                bytes: 11,
                limit: 10
            }),
            "a state declared at 11 bytes was accepted under a 10-byte limit"
        );
        assert_eq!(r.buf.capacity(), 0, "the refused total was reserved anyway");
    }

    /// A total no allocator can give is refused as a value, not an abort:
    /// `try_reserve_exact` reports it where `Vec::reserve` would end the
    /// process.
    #[test]
    fn a_total_that_cannot_be_reserved_is_refused_not_aborted() {
        let mut r = Reassembler::with_limit(usize::MAX);
        assert_eq!(
            r.push(0, isize::MAX as u64, false, b"a"),
            Err(ChunkError::NoRoom {
                bytes: isize::MAX as usize
            })
        );
    }

    /// One reservation carries the whole state: later chunks never grow the
    /// buffer, so the doubling a 1 GiB state would cost is gone.
    #[test]
    fn the_first_chunk_reserves_the_whole_total_and_no_chunk_regrows_it() {
        let data = vec![7u8; STATE_CHUNK_BYTES * 2 + 5];
        let mut r = Reassembler::default();
        let mut capacity = None;
        for (seq, last, bytes) in split(&data) {
            r.push(seq, data.len() as u64, last, bytes).expect("chunk");
            let held = *capacity.get_or_insert(r.buf.capacity());
            assert_eq!(r.buf.capacity(), held, "chunk {seq} regrew the buffer");
        }
        assert_eq!(capacity, Some(data.len()), "reserved exactly the total");
    }

    /// A chunk that declares another total than the first is refused.
    #[test]
    fn a_changed_total_is_refused() {
        let mut r = Reassembler::default();
        r.push(0, 4, false, b"ab").expect("first chunk");
        assert_eq!(
            r.push(1, 9, true, b"cd"),
            Err(ChunkError::TotalChanged {
                expected: 4,
                got: 9
            })
        );
    }

    /// The bytes must add up to the declared total: a `last` chunk that ends
    /// short, and one that runs past it, are both refused.
    #[test]
    fn chunks_that_do_not_add_up_to_the_total_are_refused() {
        let mut short = Reassembler::default();
        short.push(0, 6, false, b"ab").expect("first chunk");
        assert_eq!(
            short.push(1, 6, true, b"cd"),
            Err(ChunkError::NotTotal { got: 4, total: 6 })
        );

        let mut long = Reassembler::default();
        long.push(0, 3, false, b"ab").expect("first chunk");
        assert_eq!(
            long.push(1, 3, false, b"cd"),
            Err(ChunkError::NotTotal { got: 4, total: 3 })
        );
    }

    /// A state of exactly the limit is accepted: pairs with the refusal above
    /// so the comparison is pinned in both directions.
    #[test]
    fn a_sequence_of_exactly_the_limit_is_accepted() {
        let mut r = Reassembler::with_limit(10);
        assert!(
            r.push(0, 10, true, &[0u8; 10])
                .expect("exactly at the limit"),
            "a state of exactly the limit must complete"
        );
        assert_eq!(r.into_inner().len(), 10);
    }

    /// **An empty chunk before `last` is refused**: it makes no
    /// progress, so a peer sending them could hold a reader forever under the
    /// byte limit. An empty `last` chunk is the empty state, and stays valid.
    #[test]
    fn an_empty_chunk_before_the_last_is_refused() {
        let mut r = Reassembler::default();
        r.push(0, 1, false, b"a").expect("first chunk");
        assert_eq!(r.push(1, 1, false, b""), Err(ChunkError::Empty { seq: 1 }));

        let mut empty = Reassembler::default();
        assert_eq!(empty.push(0, 0, true, b""), Ok(true), "the empty state");
    }
}
