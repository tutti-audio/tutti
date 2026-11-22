//! The edits a plugin reports to the host, as a ring of events: [`reports`].
//!
//! The Processor sends each [`ParamEdit`] the plugin made from its audio
//! thread; the Controller drains them on the control side. It is the direction
//! opposite to the host's writes, which are slots ([`edit_slots`](crate::edit_slots())):
//! a report is an event (a gesture's `Begin` and `End` must all arrive, in
//! order), so it is a ring.

use tutti_rt::{Overflow, PushError, RingReader, RingWriter, ring};
use tutti_units::{Normalized, ParamId};

use crate::ParamEdit;

const BEGIN_TAG: u64 = 1;
const CHANGE_TAG: u64 = 2;
const END_TAG: u64 = 3;
/// Set in a change's tag when it is not to be recorded.
const UNRECORDED: u64 = 0x100;

fn encode(edit: ParamEdit) -> [u64; 2] {
    let (tag, id, value) = match edit {
        ParamEdit::Begin(id) => (BEGIN_TAG, id, 0),
        ParamEdit::Change { id, value, record } => (
            if record {
                CHANGE_TAG
            } else {
                CHANGE_TAG | UNRECORDED
            },
            id,
            value.get().to_bits(),
        ),
        ParamEdit::End(id) => (END_TAG, id, 0),
    };
    [tag << 32 | u64::from(id.get()), value]
}

fn decode(raw: [u64; 2]) -> Option<ParamEdit> {
    let id = ParamId::new(raw[0] as u32);
    // `clamped`: the value was a `Normalized` when it was sent, and the audio
    // or control thread must not panic on a corrupt entry.
    let value = Normalized::clamped(f64::from_bits(raw[1]));
    Some(match raw[0] >> 32 {
        BEGIN_TAG => ParamEdit::Begin(id),
        END_TAG => ParamEdit::End(id),
        CHANGE_TAG => ParamEdit::Change {
            id,
            value,
            record: true,
        },
        t if t == CHANGE_TAG | UNRECORDED => ParamEdit::Change {
            id,
            value,
            record: false,
        },
        _ => return None,
    })
}

/// A ring of [`ParamEdit`]s holding up to `capacity`, as its sending and
/// draining ends. A full ring drops the newest report.
///
/// # Panics
///
/// If `capacity` is zero.
#[must_use]
pub fn reports(capacity: usize) -> (ReportSink, ReportDrain) {
    assert!(capacity > 0, "a report ring holds at least one value");
    let (writer, reader) = ring(capacity, Overflow::DropNewest);
    (
        ReportSink(writer),
        ReportDrain {
            reader,
            undecodable: 0,
        },
    )
}

/// The audio thread's end of a report ring.
#[derive(Debug)]
pub struct ReportSink(RingWriter<[u64; 2]>);

impl ReportSink {
    /// Queues `edit`. Wait-free; never allocates.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] when the ring has no room (the caller counts it), and
    /// [`PushError::Disconnected`] when the drain is gone.
    pub fn send(&mut self, edit: ParamEdit) -> Result<(), PushError> {
        self.0.push(encode(edit)).map_err(|returned| returned.error)
    }
}

/// The control side's end of a report ring.
#[derive(Debug)]
pub struct ReportDrain {
    reader: RingReader<[u64; 2]>,
    undecodable: u64,
}

impl ReportDrain {
    /// Hands each report queued when the call began to `apply`, oldest first;
    /// what the producer pushes meanwhile waits for the next call, so a hot
    /// producer cannot extend it.
    ///
    /// An entry that decodes to no known [`ParamEdit`] is dropped and counted in
    /// [`undecodable`](Self::undecodable): only a producer of another version
    /// could write one.
    pub fn drain(&mut self, mut apply: impl FnMut(ParamEdit)) {
        for _ in 0..self.reader.len() {
            let Some(raw) = self.reader.pop() else {
                break;
            };
            match decode(raw) {
                Some(edit) => apply(edit),
                None => self.undecodable = self.undecodable.saturating_add(1),
            }
        }
    }

    /// Entries dropped because they decoded to no known edit.
    #[must_use]
    pub fn undecodable(&self) -> u64 {
        self.undecodable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring of no room is refused at construction.
    #[test]
    #[should_panic(expected = "a report ring holds at least one value")]
    fn a_report_ring_of_no_room_is_refused() {
        let _ = reports(0);
    }

    /// A report survives the ring in order: each gesture end, a change with its
    /// value to the bit, and the flag of one not to be recorded; a full ring
    /// refuses.
    #[test]
    fn reports_round_trip_in_order() {
        let (mut sink, mut drain) = reports(4);
        let id = ParamId::new(u32::MAX);
        let sent = [
            ParamEdit::Begin(id),
            ParamEdit::change(id, Normalized::of(0.3)),
            ParamEdit::change(ParamId::new(2), Normalized::of(0.7)).unrecorded(),
            ParamEdit::End(id),
        ];
        for edit in sent {
            sink.send(edit).unwrap();
        }
        assert_eq!(sink.send(sent[0]), Err(PushError::Full));
        let mut got = Vec::new();
        drain.drain(|e| got.push(e));
        assert_eq!(got, sent);
        assert_eq!(drain.undecodable(), 0);
    }

    /// An entry with an unknown tag is dropped and counted, and the reports
    /// around it still arrive.
    #[test]
    fn an_unknown_entry_is_dropped_and_counted() {
        assert_eq!(decode([99 << 32, 0]), None);
        let (mut writer, reader) = ring::<[u64; 2]>(4, Overflow::DropNewest);
        let good = ParamEdit::change(ParamId::new(1), Normalized::of(0.5));
        writer.push(encode(good)).unwrap();
        writer.push([99 << 32, 0]).unwrap();
        writer.push(encode(good)).unwrap();
        let mut drain = ReportDrain {
            reader,
            undecodable: 0,
        };
        let mut got = 0;
        drain.drain(|_| got += 1);
        assert_eq!(got, 2);
        assert_eq!(drain.undecodable(), 1);
    }

    /// A drain applies what was queued when it began, not what arrives during
    /// it.
    #[test]
    fn a_drain_is_bounded_by_what_was_queued() {
        let (mut sink, mut drain) = reports(8);
        let edit = ParamEdit::change(ParamId::new(1), Normalized::of(0.5));
        sink.send(edit).unwrap();
        sink.send(edit).unwrap();
        let mut applied = 0;
        drain.drain(|_| {
            applied += 1;
            let _ = sink.send(edit);
        });
        assert_eq!(applied, 2);
        let mut rest = 0;
        drain.drain(|_| rest += 1);
        assert_eq!(
            rest, 2,
            "the pushes made during the drain wait for the next"
        );
    }
}
