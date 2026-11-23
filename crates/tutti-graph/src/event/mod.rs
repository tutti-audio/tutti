//! Events: what flows on an event port.
//!
//! Events are **graph ports**, not a side channel, so the one compiler pass
//! that aligns audio (PDC) aligns notes and automation too. An event input
//! port may have several sources, merged deterministically by `(offset,
//! source order)`, where source order is the source port's `(NodeKey, port)`
//! — layering a keyboard and a clip is the normal case, not an edge case.
//! Automation is carried as **ramp** events ([`ParamRamp`](crate::ParamRamp):
//! relative, from wherever the param stands) and as **curve segments**
//! ([`ParamSegment`](crate::ParamSegment): absolute, between their own
//! endpoints), both moving along a closed set of shapes
//! ([`Law`](tutti_curve::Law)) a param port evaluates per frame. They are
//! [`ParamEvent`](crate::ParamEvent)s, and travel in a buffer of their own beside the port's
//! events, merged and delayed with them, so a MIDI event stays 32 bytes.
//!
//! A MIDI event carries a [`tutti_ump::Ump`]. The graph reads one fact of it,
//! [`Event::is_note_off`](crate::Event::is_note_off), which the delay FIFO's overflow policy needs, and
//! that decode is `tutti-ump`'s.

pub(crate) mod merge;
pub(crate) mod sorted;
pub(crate) mod types;
pub(crate) mod writer;

#[cfg(test)]
mod tests;
