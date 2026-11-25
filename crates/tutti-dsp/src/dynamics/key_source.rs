//! [`KeySource`]: where a keyed dynamics node's detector listens.

/// The signal a compressor's or a gate's detector reads: a type of the node, so a
/// graph's port layout and its sound say the same thing.
///
/// - [`Sidechain`](Self::Sidechain): a second input bus of the node, as wide as the signal.
///   The detector measures the key and the gain is applied to the signal, so the signal
///   being measured need not be the signal being shaped (a ducker, a de-esser, a gate keyed
///   from a kick). **A key nobody feeds is silence**, which compresses nothing and holds a
///   gate closed: the graph feeds an unconnected input zeros, and the keyed nodes read
///   those zeros as the key.
/// - [`Own`](Self::Own): no key bus. The detector reads the signal itself, as an ordinary
///   compressor or gate does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeySource {
    /// The detector reads a side-chain bus, as wide as the signal.
    Sidechain,
    /// The detector reads the signal itself; the node has no key bus.
    Own,
}
