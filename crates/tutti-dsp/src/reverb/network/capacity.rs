//! [`NetworkCapacity`]: the reverb's worst-case sizes, what a
//! [`FdnLayout`](crate::FdnLayout) sizes its bank from.

use tutti_types::{Db, SampleSpan, Samples};

use super::{Network, Stage};
use crate::{Ring, System};

/// The size of a [`Network`], or the worst case over a layout's controls: every
/// figure a runner allocates from.
///
/// [`need`](Self::need) measures one network.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NetworkCapacity {
    /// Delay lines in all.
    pub lines: usize,
    /// Stages.
    pub stages: usize,
    /// Taps in all.
    pub taps: usize,
    /// Output channels of the widest recirculating stage.
    pub outputs: usize,
    /// The longest single line, in frames, with its nested all-pass and its
    /// modulation excursion.
    pub line: SampleSpan,
    /// The total delay of every line, all-pass and tap, in frames, with the
    /// modulation excursions.
    pub delay: SampleSpan,
    /// The longest FIR.
    pub ir: Samples,
    /// The longest pre-delay.
    pub pre_delay: Samples,
    /// The longest [`tail`](System::tail) at the floor it was measured to.
    pub tail: Ring,
}

impl NetworkCapacity {
    /// The sizes `network` needs, its tail measured to `floor`.
    #[must_use]
    pub fn need(network: &Network, floor: Db) -> Self {
        let mut b = NetworkCapacity {
            lines: 0,
            stages: network.stages.len(),
            taps: 0,
            outputs: 0,
            line: SampleSpan::ZERO,
            delay: SampleSpan::ZERO,
            ir: Samples::ZERO,
            pre_delay: network.pre_delay,
            tail: network.tail(floor),
        };
        for stage in network.stages.iter() {
            match stage {
                Stage::Taps(t) => {
                    b.taps += t.taps.len();
                    b.delay += t
                        .taps
                        .iter()
                        .map(|t| t.delay)
                        .fold(SampleSpan::ZERO, SampleSpan::max);
                }
                Stage::AllPass(a) => {
                    b.delay = a
                        .iter()
                        .fold(b.delay, |sum, a| sum + a.delay + a.modulation.depth);
                }
                Stage::Fdn(f) => {
                    b.lines += f.lines.len();
                    b.outputs = b.outputs.max(f.output.rows.len());
                    for l in f.lines.iter() {
                        // An unbounded nested all-pass saturates.
                        let len = l
                            .effective_length()
                            .unwrap_or(SampleSpan::clamped(f64::MAX))
                            + l.modulation.depth;
                        b.line = b.line.max(len);
                        b.delay += len;
                    }
                }
                Stage::Fir(s) => b.ir = b.ir.max(s.length),
            }
        }
        b
    }
}
