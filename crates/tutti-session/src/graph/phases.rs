//! The graph's share of the session's update: what its phases report as
//! [`SessionEvent`]s, and the flag that a re-prepare resumed.

use std::collections::BTreeMap;

use tutti_buffers::Float;
use tutti_graph::NodeKey;

use super::{Graph, Retried};
use crate::SessionEvent;

impl<F: Float> Graph<F> {
    /// Retries the replacements that waited out a re-prepare, and reports each
    /// as it landed or failed.
    pub(crate) fn report_replacements(&mut self, out: &mut Vec<SessionEvent>) {
        out.extend(
            self.take_dropped()
                .into_iter()
                .map(|node| SessionEvent::ReplaceDropped { node }),
        );
        for retried in self.retry_replacements() {
            out.push(match retried {
                Retried::Landed { node } => SessionEvent::Replaced { node },
                Retried::Failed { node, refused } => SessionEvent::ReplaceFailed { node, refused },
            });
        }
    }

    /// [`collect`](Self::collect), noting whether it resumed a re-prepare:
    /// the figures of the resumed plan are published at the next commit
    /// ([`take_resumed`](Self::take_resumed)).
    pub(crate) fn collect_noting_resume(&mut self) {
        let repreparing = self.is_repreparing();
        self.collect();
        self.resumed |= repreparing && !self.is_repreparing();
    }

    /// Whether a re-prepare resumed since the last call.
    pub(crate) fn take_resumed(&mut self) -> bool {
        std::mem::take(&mut self.resumed)
    }

    /// The events the graph's ports lost since the last update, as
    /// [`SessionEvent::EventsDropped`], one per port.
    pub(crate) fn report_event_losses(&mut self, out: &mut Vec<SessionEvent>) {
        let mut sums: BTreeMap<(NodeKey, Option<u16>), (u64, u64)> = BTreeMap::new();
        for loss in self.take_event_losses() {
            let sum = sums.entry((loss.node, loss.port)).or_default();
            sum.0 += u64::from(loss.count);
            sum.1 += u64::from(loss.recovered);
        }
        out.extend(
            sums.into_iter()
                .filter(|(_, (count, recovered))| *count > 0 || *recovered > 0)
                .map(
                    |((node, port), (count, recovered))| SessionEvent::EventsDropped {
                        node,
                        port,
                        count,
                        recovered,
                    },
                ),
        );
        let elsewhere = self.take_unreported_event_losses();
        if elsewhere > 0 {
            out.push(SessionEvent::EventsDroppedElsewhere(elsewhere));
        }
    }

    /// What a commit left to report: the main output readers reopened and the wiring
    /// a re-insert dropped.
    pub(crate) fn report_commit(&mut self, out: &mut Vec<SessionEvent>) {
        out.extend(
            self.announce_reopened()
                .into_iter()
                .map(|(id, topology)| SessionEvent::MainOutputReaderReopened { id, topology }),
        );
        for u in self.take_unwired() {
            out.push(SessionEvent::Unwired {
                node: u.node,
                edges: u.edges,
                event_edges: u.event_edges,
                bus_edges: u.bus_edges,
            });
        }
    }

    /// Pushes the nodes the commits since the last report rewired.
    pub(crate) fn report_rewired(&mut self, out: &mut Vec<SessionEvent>) {
        out.extend(
            self.take_rewired()
                .into_iter()
                .map(|r| SessionEvent::Rewired {
                    node: r.node,
                    was: r.was,
                    now: r.now,
                }),
        );
    }
}
