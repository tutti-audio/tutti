//! The wires of a patch as the patch keeps them: by the keys it declares and
//! the ports they join, with the options each was declared with.

use tutti_units::Gain;

/// One wire of a patch by its ends: the keys it joins and which port of each,
/// of the one kind of wire the ends are. What a wire is identified by after
/// the patch, and what [`PatchError::Wire`](crate::PatchError::Wire) names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WireEnds<K> {
    /// A main bus into another node's main bus, or into the main output
    /// (`None`).
    Main(K, Option<K>),
    /// One output channel into one input channel: the source, its output, the
    /// sink and its input.
    Channel(K, usize, K, usize),
    /// An event output into an event input: the source, its output, the sink
    /// and its input.
    Event(K, u16, K, u16),
    /// An output bus into an input bus (the sink and its bus), or into the
    /// main output (`None`).
    Bus(K, u8, Option<(K, u8)>),
}

impl<K> WireEnds<K> {
    /// The key the wire leaves from.
    pub(crate) fn source(&self) -> &K {
        match self {
            Self::Main(from, _)
            | Self::Channel(from, ..)
            | Self::Event(from, ..)
            | Self::Bus(from, ..) => from,
        }
    }

    /// The key the wire arrives at, `None` for the main output.
    pub(crate) fn sink(&self) -> Option<&K> {
        match self {
            Self::Main(_, to) => to.as_ref(),
            Self::Channel(_, _, to, _) | Self::Event(_, _, to, _) => Some(to),
            Self::Bus(_, _, to) => to.as_ref().map(|(to, _)| to),
        }
    }
}

/// One wire of a patch, with its options.
#[derive(Clone, Debug)]
pub(crate) struct WireDecl<K> {
    pub(crate) ends: WireEnds<K>,
    pub(crate) unfaded: bool,
    pub(crate) initial: Option<Gain>,
}
