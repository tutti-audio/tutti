//! Where the executor's dropped events went: the running totals, and for each
//! block the node ports that lost some.
//!
//! Three figures, all exact:
//!
//! - [`Executor::dropped_event_count`]: every event refused or skipped. A
//!   note-off a node refused and sent again later is in it.
//! - [`Executor::lost_event_count`]: that, less the refusals a node says it
//!   recovered (`EventWriter::recovered`): the events that never arrived.
//! - the per-port attribution, which is best effort: the audio thread notes
//!   a loss in a fixed-capacity log as it happens (no allocation), and at
//!   the end of the block hands the log to a ring the editor drains on the
//!   control side ([`Editor::take_event_losses`]). A loss that does not fit
//!   (a block that lost at more places than the log holds, a full ring, a
//!   merge, which belongs to no node) is added to a counter instead
//!   ([`Editor::take_unreported_event_losses`]), so nothing is lost silently:
//!   what the entries do not account for, that counter does.
//!
//! In a parallel block the entries come in the order the workers finished and
//! each worker's log is its own, so the entries, and which of them a full
//! log leaves out, can differ from one participant's; the totals and the
//! render do not.
//!
//! [`Executor::dropped_event_count`]: crate::Executor::dropped_event_count
//! [`Executor::lost_event_count`]: crate::Executor::lost_event_count
//! [`Editor::take_event_losses`]: crate::Editor::take_event_losses
//! [`Editor::take_unreported_event_losses`]: crate::Editor::take_unreported_event_losses

use crate::compile::ids::{DelayId, EventFeedbackId, Id, PlanUnitId};

use crate::NodeKey;
use tutti_rt::{CappedVec, CounterWriter, RingWriter};

use crate::compile::plan::{FeedbackKey, Plan};
use crate::model::spec::EventOutputPort;

/// Entries one block's log holds (and, per worker, one parallel block's).
pub(crate) const LOG_CAPACITY: usize = 64;

/// The events one node lost in one block, at one of its ports or in all.
///
/// `port` is the output port whose events were lost: refused by its own
/// writer (past its capacity), skipped by the node because they belonged to
/// an event the port refused, or dropped by a delay or feedback FIFO fed by
/// it. `None` for a loss the node had but no output port owns: events a
/// plugin took from its input and did not deliver (`Io::count_lost`), and a
/// scheduled command that did not fit its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventLoss {
    /// The node.
    pub node: NodeKey,
    /// Its event output port.
    pub port: Option<u16>,
    /// How many events were refused or skipped, in that block.
    pub count: u32,
    /// How many refusals a node says it then recovered in that block
    /// (a note-off it sent again): refused earlier, delivered in this block.
    pub recovered: u32,
}

#[derive(Clone, Copy, Debug)]
enum Owner {
    /// A unit of the running plan, by index.
    Unit(PlanUnitId),
    Key(NodeKey),
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    owner: Owner,
    port: Option<u16>,
    count: u32,
    recovered: u32,
}

/// The running totals and this block's log. One per executor, one per
/// parallel worker.
#[derive(Debug)]
pub(crate) struct Dropped {
    total: u64,
    recovered: u64,
    /// This block's refusals the log could not name.
    unreported: u64,
    log: CappedVec<Entry>,
}

impl Dropped {
    /// Control side: reserves the log.
    pub(crate) fn new() -> Self {
        Self {
            total: 0,
            recovered: 0,
            unreported: 0,
            log: CappedVec::with_capacity(LOG_CAPACITY),
        }
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    pub(crate) fn recovered(&self) -> u64 {
        self.recovered
    }

    /// Refusals less recoveries.
    pub(crate) fn lost(&self) -> u64 {
        self.total.saturating_sub(self.recovered)
    }

    /// Forget the totals and the log (a worker's, before a block).
    pub(crate) fn reset(&mut self) {
        self.total = 0;
        self.recovered = 0;
        self.unreported = 0;
        self.log.clear();
    }

    /// `lost` events a merge cut: charged to the node whose input bound cut
    /// them, or to no node for a merge with none.
    pub(crate) fn charge_merge(
        &mut self,
        bound: Option<crate::compile::plan::InputBound>,
        lost: u64,
    ) {
        match bound {
            Some(b) => self.at_node(
                PlanUnitId::new(b.unit),
                u32::try_from(lost).unwrap_or(u32::MAX),
            ),
            None => self.count(lost),
        }
    }

    /// `n` events lost where no node owns them (a merge).
    pub(crate) fn count(&mut self, n: u64) {
        self.total += n;
        self.unreported += n;
    }

    /// `count` events charged to the plan's unit `unit`, no port: events the
    /// node took in and did not deliver (`Io::count_lost`), or events cut from
    /// its declared input bound before it ran.
    pub(crate) fn at_node(&mut self, unit: PlanUnitId, count: u32) {
        self.note(Owner::Unit(unit), None, count, 0);
    }

    /// `count` events lost at output port `port` of the plan's unit `unit`.
    pub(crate) fn at_port(&mut self, unit: PlanUnitId, port: u16, count: u32, recovered: u32) {
        self.note(Owner::Unit(unit), Some(port), count, recovered);
    }

    /// `count` events lost downstream of `from`, in a FIFO it feeds.
    pub(crate) fn charge_source(&mut self, from: EventOutputPort, count: u32) {
        self.note(Owner::Key(from.node), Some(from.port), count, 0);
    }

    /// `count` events lost in the FIFO of the plan's delay `delay`, charged
    /// to the output port that feeds it.
    pub(crate) fn charge_delay(&mut self, plan: &Plan, delay: u32, count: u64) {
        // A tap of an input merges several sources, so its loss is nobody's.
        let from = match plan
            .program
            .delays
            .get(DelayId::new(delay))
            .and_then(|d| d.key.event_origin())
        {
            Some(crate::EventTapPort::Out(from)) => Some(from),
            _ => None,
        };
        self.charge(from, count);
    }

    /// As [`charge_delay`](Self::charge_delay), for the plan's event feedback
    /// `feedback`.
    pub(crate) fn charge_feedback(&mut self, plan: &Plan, feedback: u32, count: u64) {
        let from = match plan
            .program
            .event_feedback
            .get(EventFeedbackId::new(feedback))
            .map(|f| f.key)
        {
            Some(FeedbackKey::Event { from, .. }) => Some(from),
            _ => None,
        };
        self.charge(from, count);
    }

    fn charge(&mut self, from: Option<EventOutputPort>, count: u64) {
        let n = u32::try_from(count).unwrap_or(u32::MAX);
        match from {
            Some(from) => self.charge_source(from, n),
            None => self.count(count),
        }
    }

    fn note(&mut self, owner: Owner, port: Option<u16>, count: u32, recovered: u32) {
        self.total += u64::from(count);
        self.recovered += u64::from(recovered);
        if count == 0 && recovered == 0 {
            return;
        }
        let entry = Entry {
            owner,
            port,
            count,
            recovered,
        };
        if !self.log.push(entry) {
            self.unreported += u64::from(count.saturating_sub(recovered));
        }
    }

    /// Add a worker's block: its totals, its log and what it left out.
    pub(crate) fn absorb(&mut self, other: &mut Self) {
        self.total += other.total;
        self.recovered += other.recovered;
        self.unreported += other.unreported;
        for e in other.log.drain(..) {
            if !self.log.push(e) {
                self.unreported += u64::from(e.count.saturating_sub(e.recovered));
            }
        }
        other.reset();
    }

    /// End of block: the log, by node key as `plan` names them, onto `tx`;
    /// whatever the log or the ring could not take added to `unreported`.
    /// Clears the log; the totals stand.
    pub(crate) fn flush(
        &mut self,
        plan: &Plan,
        tx: &mut RingWriter<EventLoss>,
        unreported: &mut CounterWriter<u64>,
    ) {
        for e in self.log.drain(..) {
            let node = match e.owner {
                Owner::Unit(u) => match plan.units.get(u) {
                    Some(unit) => unit.key,
                    None => {
                        self.unreported += u64::from(e.count.saturating_sub(e.recovered));
                        continue;
                    }
                },
                Owner::Key(k) => k,
            };
            let entry = EventLoss {
                node,
                port: e.port,
                count: e.count,
                recovered: e.recovered,
            };
            if tx.push(entry).is_err() {
                self.unreported += u64::from(e.count.saturating_sub(e.recovered));
            }
        }
        if self.unreported != 0 {
            unreported.add(self.unreported);
            self.unreported = 0;
        }
    }
}

/// Entries the editor's loss ring holds.
pub(crate) const RING_CAPACITY: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::ids::Id;

    /// A loss no node owns (a merge's) is unreported, and so is one the full
    /// log has no entry for; the totals have both. 64 places fill the log; a
    /// 65th, and 5 events from a merge, are unreported: 7.
    #[test]
    fn what_no_entry_names_is_unreported() {
        let mut d = Dropped::new();
        for u in 0..LOG_CAPACITY as u32 {
            d.at_port(PlanUnitId::new(u), 0, 1, 0);
        }
        assert_eq!(d.unreported, 0);
        d.at_port(PlanUnitId::new(99), 0, 2, 0);
        d.count(5);
        assert_eq!(d.unreported, 7);
        assert_eq!(d.total(), LOG_CAPACITY as u64 + 7);
    }

    /// Refusals taken back leave the total and take the lost figure down.
    #[test]
    fn a_recovered_refusal_is_not_lost() {
        let mut d = Dropped::new();
        d.at_port(PlanUnitId::new(0), 0, 3, 0);
        d.at_port(PlanUnitId::new(0), 0, 0, 2);
        assert_eq!((d.total(), d.lost()), (3, 1));
    }

    /// A worker's unreported count and recoveries join the block's.
    #[test]
    fn a_workers_block_joins_the_blocks() {
        let (mut a, mut b) = (Dropped::new(), Dropped::new());
        b.at_port(PlanUnitId::new(1), 0, 4, 1);
        b.count(2);
        a.absorb(&mut b);
        assert_eq!((a.total(), a.lost(), a.unreported), (6, 5, 2));
        assert_eq!(b.total(), 0);
    }

    /// What the log cannot name is unreported net of what was recovered: a
    /// refusal taken back is not a loss to report. A full log, then an entry
    /// of 3 refused and 3 recovered, adds nothing; 3 refused and 1 recovered
    /// adds 2; the same through `absorb`.
    #[test]
    fn unreported_is_net_of_recoveries() {
        let mut d = Dropped::new();
        for u in 0..LOG_CAPACITY as u32 {
            d.at_port(PlanUnitId::new(u), 0, 1, 0);
        }
        d.at_port(PlanUnitId::new(99), 0, 3, 3);
        assert_eq!(d.unreported, 0);
        d.at_port(PlanUnitId::new(99), 0, 3, 1);
        assert_eq!(d.unreported, 2);
        let mut w = Dropped::new();
        w.at_port(PlanUnitId::new(1), 0, 4, 4);
        d.absorb(&mut w);
        assert_eq!(d.unreported, 2);
    }
}
