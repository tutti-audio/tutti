//! The sequencer context of a block (chords, scales and per-note texts) in the
//! form VST3's event list takes.
//!
//! The Processor converts a block's [`ExpressiveContext`]
//! into these buffers and stages them as [`InputEvents`](crate::InputEvents).

use crate::events::{Utf16Chord, Utf16Scale, Utf16Text, event_text};
use tutti_plugin_types::{
    ChordChanges, ExpressiveContext, HARMONY_CAPACITY, NoteExpressionIntValue,
    NoteExpressionTextChanges, ScaleChanges,
};
use tutti_rt::{RtVec, TallyWriter};

/// The buffers the sequencer-context conversion in `Vst3Processor::process_shared`
/// fills each block.
///
/// Every stream holds at most [`HARMONY_CAPACITY`] events (the shared types cap
/// them) and every text at most
/// [`MAX_EVENT_TEXT_LEN`](tutti_vst3_ffi::MAX_EVENT_TEXT_LEN) UTF-16 units, both
/// inline: filling them allocates nothing, from the first block on. A text cut
/// at its room, or an event past a stream's, is counted.
#[derive(Default)]
pub(super) struct SeqScratch {
    pub(super) chords: RtVec<Utf16Chord, HARMONY_CAPACITY>,
    pub(super) scales: RtVec<Utf16Scale, HARMONY_CAPACITY>,
    pub(super) expr_texts: RtVec<Utf16Text, HARMONY_CAPACITY>,
}

/// Adds one to `refused` when `kept` is false.
fn count_refused(kept: bool, refused: &TallyWriter<u64>) {
    if !kept {
        refused.add(1);
    }
}

/// Refills `out` from `chords`; an absent stream leaves it empty.
pub(super) fn fill_chords(
    chords: Option<&ChordChanges>,
    out: &mut RtVec<Utf16Chord, HARMONY_CAPACITY>,
    refused: &TallyWriter<u64>,
) {
    out.clear();
    for c in chords.into_iter().flat_map(|c| c.changes.iter()) {
        let text = event_text(&c.text);
        count_refused(!text.is_overflowed(), refused);
        count_refused(
            out.push(Utf16Chord {
                sample_offset: c.sample_offset,
                root: c.root,
                bass_note: c.bass_note,
                mask: c.mask,
                text,
            }),
            refused,
        );
    }
}

/// Refills `out` from `scales`; an absent stream leaves it empty.
pub(super) fn fill_scales(
    scales: Option<&ScaleChanges>,
    out: &mut RtVec<Utf16Scale, HARMONY_CAPACITY>,
    refused: &TallyWriter<u64>,
) {
    out.clear();
    for s in scales.into_iter().flat_map(|s| s.changes.iter()) {
        let text = event_text(&s.text);
        count_refused(!text.is_overflowed(), refused);
        count_refused(
            out.push(Utf16Scale {
                sample_offset: s.sample_offset,
                root: s.root,
                mask: s.mask,
                text,
            }),
            refused,
        );
    }
}

/// Refills `out` from `texts`; an absent stream leaves it empty.
pub(super) fn fill_expr_texts(
    texts: Option<&NoteExpressionTextChanges>,
    out: &mut RtVec<Utf16Text, HARMONY_CAPACITY>,
    refused: &TallyWriter<u64>,
) {
    out.clear();
    for t in texts.into_iter().flat_map(|t| t.changes.iter()) {
        let text = event_text(&t.text);
        count_refused(!text.is_overflowed(), refused);
        count_refused(
            out.push(Utf16Text {
                sample_offset: t.sample_offset,
                note_id: t.note_id,
                type_id: t.type_id,
                text,
            }),
            refused,
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
    /// allocates nothing: the storage is inline, so there is no growth to
    /// wait for.
    ///
    /// The source `ChordChanges` is built outside the gate: constructing one
    /// allocates its `String`s, which is the caller's cost.
    #[test]
    fn a_first_event_on_cold_buffers_does_not_allocate() {
        let source = changes(&[chord(8, "Dmin9"), chord(96, "G7")]);
        let refused = tally::<u64>();
        let writer = refused.writer();
        let mut out = RtVec::new();
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
        let mut out = RtVec::new();
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
        let mut out = RtVec::new();
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
        let mut out = RtVec::new();
        let exact = "a".repeat(tutti_vst3_ffi::MAX_EVENT_TEXT_LEN);
        fill_chords(Some(&changes(&[chord(0, &exact)])), &mut out, &writer);
        assert_eq!(refused.get(), 0);
        assert_eq!(out[0].text.len(), tutti_vst3_ffi::MAX_EVENT_TEXT_LEN);

        let long = "a".repeat(tutti_vst3_ffi::MAX_EVENT_TEXT_LEN + 1);
        fill_chords(Some(&changes(&[chord(0, &long)])), &mut out, &writer);
        assert_eq!(refused.get(), 1);
        assert_eq!(out[0].text.len(), tutti_vst3_ffi::MAX_EVENT_TEXT_LEN);
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
