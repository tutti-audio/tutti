//! The packet form of a text: the cut of a byte string into Complete, Start,
//! Continue and End packets that every text message of UMP shares (the Flex
//! Data text of section 7.5.9, the Stream names of sections 7.1.4, 7.1.5 and
//! 7.1.9), and the Stream text's reader.

use core::iter::FusedIterator;

use crate::Form;

/// The packets of `bytes`, `W` bytes each: one `Complete` when it fits, else a
/// `Start`, any number of `Continue` and an `End`, the last padded with `0x00`.
/// An empty text is one `Complete` of zeros: each Filter bit of an Endpoint
/// Discovery is owed a reply (M2-104 section 7.1.1), so a text that is empty
/// is still answered. A packet may end inside a multi-byte character: the
/// reader joins the bytes before it decodes them. Nothing is allocated.
///
/// ```
/// use tutti_ump::{Form, text_packets};
///
/// let mut packets = text_packets::<4>(b"hello");
/// assert_eq!(packets.len(), 2);
/// assert_eq!(packets.next(), Some((Form::Start, *b"hell")));
/// assert_eq!(packets.next(), Some((Form::End, [b'o', 0, 0, 0])));
/// assert_eq!(text_packets::<4>(b"").next(), Some((Form::Complete, [0; 4])));
/// ```
#[must_use]
pub fn text_packets<const W: usize>(bytes: &[u8]) -> TextPackets<'_, W> {
    TextPackets {
        rest: bytes,
        started: false,
    }
}

/// The packets of one text; see [`text_packets`].
#[derive(Clone, Debug)]
pub struct TextPackets<'a, const W: usize> {
    rest: &'a [u8],
    /// Whether a packet has been yielded, so an empty text is one packet.
    started: bool,
}

impl<'a, const W: usize> TextPackets<'a, W> {
    /// The packets of `rest`, which is what is left of a text once `started`
    /// says whether its first packet is out.
    pub(crate) const fn resume(rest: &'a [u8], started: bool) -> Self {
        Self { rest, started }
    }
}

impl<const W: usize> Iterator for TextPackets<'_, W> {
    type Item = (Form, [u8; W]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.started && self.rest.is_empty() {
            return None;
        }
        let form = match (self.started, self.rest.len() > W) {
            (false, false) => Form::Complete,
            (false, true) => Form::Start,
            (true, true) => Form::Continue,
            (true, false) => Form::End,
        };
        let (chunk, rest) = self.rest.split_at(self.rest.len().min(W));
        self.rest = rest;
        self.started = true;
        let mut packet = [0u8; W];
        packet[..chunk.len()].copy_from_slice(chunk);
        Some((form, packet))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match (self.started, self.rest.len()) {
            (true, 0) => 0,
            (_, len) => len.div_ceil(W).max(1),
        };
        (n, Some(n))
    }
}

impl<const W: usize> ExactSizeIterator for TextPackets<'_, W> {}
impl<const W: usize> FusedIterator for TextPackets<'_, W> {}

/// Reassembles the packets of the Stream texts (M2-104 sections 7.1.4, 7.1.5
/// and 7.1.9) that [`text_packets`] writes, for texts of up to `N` bytes. It
/// holds one message at a time, which belongs to the `owner` that started it:
/// the Function Block a name is of, or any constant for a text with one source.
///
/// - A `Complete` or `Start` begins a message; one already open is dropped,
///   counted.
/// - A `Continue` or `End` of another owner than the open message's, or with
///   none open, is not part of a message the assembler holds: it is skipped.
/// - A message longer than `N` is dropped whole, counted, and its remaining
///   packets are skipped.
/// - The text ends at the first `0x00`, the padding.
///
/// ```
/// use tutti_ump::{StreamTextAssembler, text_packets};
///
/// let mut assembler = StreamTextAssembler::<16>::new();
/// let mut done = None;
/// for (form, packet) in text_packets::<4>(b"a name") {
///     if let Some(text) = assembler.feed(0, form, &packet) {
///         done = Some(text.to_vec());
///     }
/// }
/// assert_eq!(done.as_deref(), Some(&b"a name"[..]));
/// assert_eq!(assembler.dropped(), 0);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct StreamTextAssembler<const N: usize> {
    buf: [u8; N],
    len: usize,
    open: Option<u8>,
    dropped: u64,
}

impl<const N: usize> Default for StreamTextAssembler<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> StreamTextAssembler<N> {
    /// An assembler with no message open.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
            open: None,
            dropped: 0,
        }
    }

    /// Takes one packet of `owner`'s text; the message it completes comes back,
    /// cut at the first `0x00`.
    pub fn feed(&mut self, owner: u8, form: Form, bytes: &[u8]) -> Option<&[u8]> {
        match form {
            Form::Complete | Form::Start => {
                if self.open.is_some() {
                    self.dropped += 1;
                }
                self.len = 0;
                self.open = Some(owner);
            }
            Form::Continue | Form::End => {}
        }
        if self.open != Some(owner) {
            return None;
        }
        let end = self.len + bytes.len();
        if end > N {
            self.open = None;
            self.dropped += 1;
            return None;
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        if !matches!(form, Form::Complete | Form::End) {
            return None;
        }
        self.open = None;
        let text = &self.buf[..self.len];
        let cut = text.iter().position(|&b| b == 0).unwrap_or(text.len());
        Some(&text[..cut])
    }

    /// Messages dropped whole since this assembler was made: interrupted by
    /// another message's start, or longer than `N` bytes.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all<const N: usize>(
        assembler: &mut StreamTextAssembler<N>,
        owner: u8,
        parts: &[(Form, &[u8])],
    ) -> Vec<Option<Vec<u8>>> {
        parts
            .iter()
            .map(|(form, bytes)| assembler.feed(owner, *form, bytes).map(<[u8]>::to_vec))
            .collect()
    }

    /// M2-104 7.1.4: a name of up to W bytes is one Complete packet; a longer
    /// one is Start, Continues, End with the last padded with zeros.
    /// Spec: m2-104/7.1.4#3, m2-104/7.1.4#4, m2-104/7.1.4#5
    #[test]
    fn a_text_is_one_complete_packet_or_start_continues_end() {
        let forms = |text: &[u8]| {
            text_packets::<4>(text)
                .map(|(form, _)| form)
                .collect::<Vec<_>>()
        };
        assert_eq!(forms(b"abcd"), [Form::Complete]);
        assert_eq!(forms(b"abcde"), [Form::Start, Form::End]);
        assert_eq!(
            forms(b"abcdefghi"),
            [Form::Start, Form::Continue, Form::End]
        );
        assert_eq!(forms(b""), [Form::Complete]);
        assert_eq!(text_packets::<4>(b"abcdefghi").len(), 3);
    }

    #[test]
    fn the_packets_of_a_text_reassemble_to_it() {
        for text in ["", "a", "abcd", "abcde", "twenty-three bytes of txt"] {
            let mut assembler = StreamTextAssembler::<32>::new();
            let done = text_packets::<4>(text.as_bytes())
                .filter_map(|(form, packet)| assembler.feed(0, form, &packet).map(<[u8]>::to_vec))
                .last();
            assert_eq!(done.as_deref(), Some(text.as_bytes()), "{text:?}");
        }
    }

    /// M2-104 7.1.4: the padding after the text is `0x00`, which ends it.
    #[test]
    fn the_text_ends_at_the_padding() {
        let mut a = StreamTextAssembler::<98>::new();
        let out = feed_all(
            &mut a,
            0,
            &[
                (Form::Start, b"AAAAAAAAAAAAAA"),
                (Form::End, b"BB\0\0\0\0\0\0\0\0\0\0\0\0"),
            ],
        );
        assert_eq!(out, [None, Some(b"AAAAAAAAAAAAAABB".to_vec())]);
    }

    #[test]
    fn a_start_drops_the_open_message_and_counts_it() {
        let mut a = StreamTextAssembler::<8>::new();
        let out = feed_all(
            &mut a,
            0,
            &[
                (Form::Start, b"abcd"),
                (Form::Start, b"wxyz"),
                (Form::End, b"yy\0\0"),
            ],
        );
        assert_eq!(out[2].as_deref(), Some(&b"wxyzyy"[..]));
        assert_eq!(a.dropped(), 1);
    }

    #[test]
    fn a_continue_with_no_start_or_of_another_owner_is_skipped() {
        let mut a = StreamTextAssembler::<8>::new();
        assert_eq!(a.feed(0, Form::End, b"abcd"), None);
        assert_eq!(a.feed(1, Form::Start, b"abcd"), None);
        assert_eq!(a.feed(2, Form::End, b"zz\0\0"), None);
        assert_eq!(a.feed(1, Form::End, b"ef\0\0"), Some(&b"abcdef"[..]));
        assert_eq!(a.dropped(), 0);
    }

    #[test]
    fn a_message_past_capacity_is_dropped_whole_and_counted() {
        let mut a = StreamTextAssembler::<6>::new();
        let out = feed_all(
            &mut a,
            0,
            &[
                (Form::Start, b"abcd"),
                (Form::Continue, b"efgh"),
                (Form::End, b"ij\0\0"),
            ],
        );
        assert_eq!(out, [None, None, None]);
        assert_eq!(a.dropped(), 1);
    }
}
