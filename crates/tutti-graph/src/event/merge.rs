//! The merge of several sorted event sources into one port's events.

use tutti_rt::CappedVec;

use crate::event::types::Event;
use crate::node::param_event::ParamEvent;
use crate::node::time::Offset;

/// Merge `sources` (each sorted by offset) into `out`, ordered by
/// `(offset, source index)`. At most `out`'s room lands; the rest are counted.
///
/// A k-way merge rather than concatenate-and-sort: the stable sort that would
/// give the same order allocates, and the unstable one does not give it.
///
/// When the sources hold more than `cap`, the note-offs keep their room: the
/// others are cut (the later ones first) so that every note-off lands, up to
/// the room. Order is that of the merge either way. A merge slot holds all its
/// sources unless the merge is a node's input under its declared bound
/// (`Shape::event_input_capacity`), which is the one merge that cuts (it walks
/// the sources itself, with [`merge_walk`]); the caller charges the returned
/// count.
pub(crate) fn merge_into(sources: &[&[Event]], out: &mut CappedVec<Event>) -> u32 {
    let room = out.remaining();
    merge_walk(sources, room, |_, e| {
        out.push(e);
    })
}

/// Merge the param events of `sources` (each sorted by offset) into `out`,
/// ordered by `(offset, source index)`: the merge of [`merge_into`] with no
/// note-off to keep. At most `out`'s room lands; the rest are counted, the
/// later ones first.
pub(crate) fn merge_params(sources: &[&[ParamEvent]], out: &mut CappedVec<ParamEvent>) -> u32 {
    let room = out.remaining();
    let mut heads = [0usize; 2 * crate::MAX_PORTS];
    let heads = &mut heads[..sources.len()];
    let (mut kept, mut dropped) = (0usize, 0u32);
    loop {
        let mut best: Option<(Offset, usize)> = None;
        for (i, src) in sources.iter().enumerate() {
            if let Some(e) = src.get(heads[i]) {
                // Strict `<`: on a tie the lower source index keeps it.
                if best.is_none_or(|(o, _)| e.offset < o) {
                    best = Some((e.offset, i));
                }
            }
        }
        let Some((_, i)) = best else { break };
        let e = sources[i][heads[i]];
        heads[i] += 1;
        if kept < room {
            kept += 1;
            out.push(e);
        } else {
            dropped += 1;
        }
    }
    dropped
}

/// Walks `sources` (each sorted by offset) in `(offset, source index)` order
/// and calls `keep(source index, event)` for each of the first `room` that
/// land, with the note-off rule of [`merge_into`]. Returns how many it
/// dropped. At most [`crate::MAX_PORTS`] `* 3` sources.
pub(crate) fn merge_walk(
    sources: &[&[Event]],
    room: usize,
    mut keep: impl FnMut(usize, Event),
) -> u32 {
    let total: usize = sources.iter().map(|s| s.len()).sum();
    // How many events that are not note-offs may land: all of them while all
    // fit, else what the note-offs leave of the room.
    let mut others = usize::MAX;
    if total > room {
        let offs: usize = sources
            .iter()
            .map(|s| s.iter().filter(|e| e.is_note_off()).count())
            .sum();
        others = room.saturating_sub(offs);
    }
    let mut heads = [0usize; 3 * crate::MAX_PORTS];
    let heads = &mut heads[..sources.len()];
    let (mut kept, mut dropped) = (0usize, 0u32);
    loop {
        let mut best: Option<(Offset, usize)> = None;
        for (i, src) in sources.iter().enumerate() {
            if let Some(e) = src.get(heads[i]) {
                // Strict `<`: on a tie the lower source index keeps it.
                if best.is_none_or(|(o, _)| e.offset < o) {
                    best = Some((e.offset, i));
                }
            }
        }
        let Some((_, i)) = best else { break };
        let e = sources[i][heads[i]];
        heads[i] += 1;
        let fits = kept < room;
        let lands = if e.is_note_off() {
            fits
        } else {
            fits && others > 0
        };
        if lands {
            if !e.is_note_off() {
                others = others.saturating_sub(1);
            }
            kept += 1;
            keep(i, e);
        } else {
            dropped += 1;
        }
    }
    dropped
}
