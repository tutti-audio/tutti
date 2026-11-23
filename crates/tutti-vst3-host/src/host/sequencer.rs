//! The sequencer context of a block (chords, scales and per-note texts) in the
//! form VST3's event list takes.
//!
//! The Processor converts a block's [`ExpressiveContext`]
//! into these buffers and stages them as [`InputEvents`](crate::InputEvents).

use crate::events::{EventText, Utf16Chord, Utf16Scale, Utf16Text, event_text};
use tutti_plugin_types::{
    ChordChanges, ExpressiveContext, HARMONY_CAPACITY, NoteExpressionIntValue,
    NoteExpressionTextChanges, ScaleChanges,
};
use tutti_rt::{CappedVec, TallyWriter};

/// The buffers the sequencer-context conversion in `Vst3Processor::process_shared`
/// fills each block.
///
/// Each stream is a `CappedVec` of [`HARMONY_CAPACITY`], the capacity of the shared
/// streams it is filled from, so every event fits; every text holds at most
/// [`MAX_EVENT_TEXT_LEN`](tutti_vst3_ffi::MAX_EVENT_TEXT_LEN) UTF-16 units, inline
/// in its event. The streams are allocated once, by [`new`](Self::new): filling
/// allocates nothing, from the first block on. A text cut at its room is counted.
pub(super) struct SeqScratch {
    pub(super) chords: CappedVec<Utf16Chord>,
    pub(super) scales: CappedVec<Utf16Scale>,
    pub(super) expr_texts: CappedVec<Utf16Text>,
}

impl SeqScratch {
    /// Empty streams, each with room for [`HARMONY_CAPACITY`] events. Allocates:
    /// off the audio thread.
    pub(super) fn new() -> Self {
        Self {
            chords: CappedVec::with_capacity(HARMONY_CAPACITY),
            scales: CappedVec::with_capacity(HARMONY_CAPACITY),
            expr_texts: CappedVec::with_capacity(HARMONY_CAPACITY),
        }
    }
}

/// `src` as an event text, counting it in `truncated` when it was cut.
fn counted_text(src: &str, truncated: &TallyWriter<u64>) -> EventText {
    let text = event_text(src);
    if text.is_overflowed() {
        truncated.add(1);
    }
    text
}

/// Appends `item`: the source streams hold at most [`HARMONY_CAPACITY`] events
/// and so does the buffer, which `clear` emptied, so the push always fits.
fn append<T>(out: &mut CappedVec<T>, item: T) {
    let fitted = out.push(item);
    debug_assert!(fitted, "a stream and its buffer share one capacity");
}

/// Refills `out` from `chords`; an absent stream leaves it empty.
pub(super) fn fill_chords(
    chords: Option<&ChordChanges>,
    out: &mut CappedVec<Utf16Chord>,
    truncated: &TallyWriter<u64>,
) {
    out.clear();
    for c in chords.into_iter().flat_map(|c| c.changes.iter()) {
        append(
            out,
            Utf16Chord {
                sample_offset: c.sample_offset,
                root: c.root,
                bass_note: c.bass_note,
                mask: c.mask,
                text: counted_text(&c.text, truncated),
            },
        );
    }
}

/// Refills `out` from `scales`; an absent stream leaves it empty.
pub(super) fn fill_scales(
    scales: Option<&ScaleChanges>,
    out: &mut CappedVec<Utf16Scale>,
    truncated: &TallyWriter<u64>,
) {
    out.clear();
    for s in scales.into_iter().flat_map(|s| s.changes.iter()) {
        append(
            out,
            Utf16Scale {
                sample_offset: s.sample_offset,
                root: s.root,
                mask: s.mask,
                text: counted_text(&s.text, truncated),
            },
        );
    }
}

/// Refills `out` from `texts`; an absent stream leaves it empty.
pub(super) fn fill_expr_texts(
    texts: Option<&NoteExpressionTextChanges>,
    out: &mut CappedVec<Utf16Text>,
    truncated: &TallyWriter<u64>,
) {
    out.clear();
    for t in texts.into_iter().flat_map(|t| t.changes.iter()) {
        append(
            out,
            Utf16Text {
                sample_offset: t.sample_offset,
                note_id: t.note_id,
                type_id: t.type_id,
                text: counted_text(&t.text, truncated),
            },
        );
    }
}

/// The block's integer note expressions, borrowed as they are: the shared and
/// the VST3 value are one type, so nothing is converted.
pub(super) fn expr_ints_of<'a>(
    expressive: Option<&ExpressiveContext<'a>>,
) -> &'a [NoteExpressionIntValue] {
    expressive
        .and_then(|e| e.expr_ints)
        .map(|i| i.changes.as_slice())
        .unwrap_or_default()
}

/// Sequencer-context conversion, exercised without a plugin binary: the
/// converters are pure functions over the protocol types.
#[cfg(test)]
mod seq_scratch_tests {
    use super::*;
    use tutti_plugin_types::ChordValue;
    use tutti_rt::tally;

    fn chord(offset: i32, text: &str) -> ChordValue {
        ChordValue {
            sample_offset: offset,
            root: 60,
            bass_note: 60,
            mask: 0b1001_0001,
            text: text.to_string(),
        }
    }

    fn changes(items: &[ChordValue]) -> ChordChanges {
        let mut c = ChordChanges::new();
        for i in items {
            assert!(c.add_change(i.clone()), "the fixture fits the cap");
        }
        c
    }

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    /// The first block that carries a chord, on buffers nothing has warmed,
    /// allocates nothing: the streams are sized when built, so there is no
    /// growth to wait for.
    ///
    /// The source `ChordChanges` is built outside the gate: constructing one
    /// allocates its `String`s, which is the caller's cost.
    #[test]
    fn a_first_event_on_cold_buffers_does_not_allocate() {
        let source = changes(&[chord(8, "Dmin9"), chord(96, "G7")]);
        let refused = tally::<u64>();
        let writer = refused.writer();
        let mut out = CappedVec::with_capacity(HARMONY_CAPACITY);
        assert_no_alloc::assert_no_alloc(|| {
            fill_chords(Some(&source), &mut out, &writer);
        });

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].sample_offset, 8);
        assert_eq!(out[0].text.as_slice(), units("Dmin9"));
        assert_eq!(out[1].text.as_slice(), units("G7"));
        assert_eq!(refused.get(), 0);
    }

    /// A shorter block truncates; a longer one grows. Both must leave the
    /// surviving prefix correct rather than stale.
    #[test]
    fn refilling_tracks_a_changing_event_count() {
        let writer = tally::<u64>().writer();
        let mut out = CappedVec::with_capacity(HARMONY_CAPACITY);
        fill_chords(
            Some(&changes(&[chord(0, "a"), chord(1, "b"), chord(2, "c")])),
            &mut out,
            &writer,
        );
        assert_eq!(out.len(), 3);

        fill_chords(Some(&changes(&[chord(9, "z")])), &mut out, &writer);
        assert_eq!(out.len(), 1, "shrinks to the new count");
        assert_eq!(out[0].sample_offset, 9);
        assert_eq!(out[0].text.as_slice(), units("z"));

        fill_chords(
            Some(&changes(&[chord(3, "p"), chord(4, "q")])),
            &mut out,
            &writer,
        );
        assert_eq!(out.len(), 2, "grows past the retained entry");
        assert_eq!(out[1].text.as_slice(), units("q"));
    }

    /// An absent or empty stream must clear rather than leave the previous
    /// block's chords visible.
    #[test]
    fn an_absent_or_empty_stream_clears_the_buffer() {
        let writer = tally::<u64>().writer();
        let mut out = CappedVec::with_capacity(HARMONY_CAPACITY);
        fill_chords(Some(&changes(&[chord(0, "Cmaj7")])), &mut out, &writer);
        assert_eq!(out.len(), 1);

        fill_chords(Some(&changes(&[])), &mut out, &writer);
        assert!(out.is_empty(), "no chords this block means none are staged");
        fill_chords(Some(&changes(&[chord(0, "x")])), &mut out, &writer);
        fill_chords(None, &mut out, &writer);
        assert!(out.is_empty());
    }

    /// A name longer than the event list's per-text room is cut to the room
    /// and counted once; one that fits exactly is not.
    #[test]
    fn a_text_past_the_room_is_cut_and_counted() {
        let refused = tally::<u64>();
        let writer = refused.writer();
        let mut out = CappedVec::with_capacity(HARMONY_CAPACITY);
        let exact = "a".repeat(tutti_vst3_ffi::MAX_EVENT_TEXT_LEN);
        fill_chords(Some(&changes(&[chord(0, &exact)])), &mut out, &writer);
        assert_eq!(refused.get(), 0);
        assert_eq!(out[0].text.len(), tutti_vst3_ffi::MAX_EVENT_TEXT_LEN);

        let long = "a".repeat(tutti_vst3_ffi::MAX_EVENT_TEXT_LEN + 1);
        fill_chords(Some(&changes(&[chord(0, &long)])), &mut out, &writer);
        assert_eq!(refused.get(), 1);
        assert_eq!(out[0].text.len(), tutti_vst3_ffi::MAX_EVENT_TEXT_LEN);
    }

    /// A cut never splits a surrogate pair: a text whose last kept unit would be
    /// a pair's high half loses that half too, and is counted.
    #[test]
    fn a_cut_does_not_split_a_surrogate_pair() {
        let truncated = tally::<u64>();
        let writer = truncated.writer();
        let mut out = CappedVec::with_capacity(HARMONY_CAPACITY);
        // 255 one-unit characters, then U+1F3B5 (units 0xD83C 0xDFB5): the
        // pair's high half is the 256th unit.
        let name = format!(
            "{}\u{1F3B5}",
            "a".repeat(tutti_vst3_ffi::MAX_EVENT_TEXT_LEN - 1)
        );
        fill_chords(Some(&changes(&[chord(0, &name)])), &mut out, &writer);
        assert_eq!(out[0].text.len(), tutti_vst3_ffi::MAX_EVENT_TEXT_LEN - 1);
        assert!(out[0].text.iter().all(|&unit| unit == u16::from(b'a')));
        assert_eq!(truncated.get(), 1);
    }

    /// The block's integer expressions reach the plugin's event list: the
    /// context's slice is what `process_shared` hands the stager.
    #[test]
    fn int_expressions_reach_the_event_list() {
        use crate::events::{InputEvents, Payload, Stager};
        use tutti_plugin_types::NoteExpressionIntChanges;
        use tutti_vst3_ffi::EventList;

        let mut ints = NoteExpressionIntChanges::new();
        assert!(ints.add_change(NoteExpressionIntValue {
            sample_offset: 4,
            note_id: 7,
            type_id: 2,
            value: 41,
        }));
        let expressive = ExpressiveContext {
            expr_ints: Some(&ints),
            ..Default::default()
        };
        let events = InputEvents {
            expr_ints: expr_ints_of(Some(&expressive)),
            ..Default::default()
        };
        let list = EventList::input();
        assert!(Stager::default().stage(&list, &events));
        list.with_events(|staged| match staged.events {
            [e] => match e.payload {
                Payload::ExpressionInt(x) => {
                    assert_eq!(
                        (x.typeId, x.noteId, x.value, e.sample_offset()),
                        (2, 7, 41, 4)
                    );
                }
                other => panic!("expected an int expression, got {other:?}"),
            },
            other => panic!("expected one event, got {other:?}"),
        });
        assert!(expr_ints_of(None).is_empty());
    }
}
