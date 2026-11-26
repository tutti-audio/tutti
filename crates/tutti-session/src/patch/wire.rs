//! Wires in a patch: what a node's output is connected to, by keys and ports.
//!
//! A wire names the keys the patch declares and which port of each: a
//! [`Mention`] or a `&`[`NodeDecl`] is a node's main bus, and its port methods
//! (`output`, `input`, `event_output`, `event_input`, `bus_output`,
//! `bus_input`) are the ports [`Key`](crate::Key) has. `MainOutput` ends a
//! wire at the graph's main output.
//!
//! `a.connect(b)` is one wire, and toward a node the result is a [`Chain`] that
//! goes on: `a.connect(&b).connect(&c)` is `a` into `b` and `b` into `c`. Toward
//! a port or `MainOutput` it is a [`Connected`], which ends. Both are parts of
//! a patch, and their options apply to the last wire only.

use tutti_units::Gain;

use crate::patch::{Declarations, IntoPatch, NodeDecl, PatchTarget, Sealed};

pub(crate) mod sealed {
    /// What a start hands `connect`.
    pub(crate) mod start {
        use super::super::{Chain, OutputEnd};

        pub trait Sealed<K> {
            /// The wires made so far, and where the next one leaves from.
            fn start(self) -> (Chain<K>, OutputEnd<K>);
        }
    }

    /// What an end hands `connect`.
    pub(crate) mod end {
        use super::super::{Chain, InputEnd};

        pub trait Sealed<K> {
            /// What the wire into this end gives: a chain toward a node, a
            /// connection toward a port.
            type Joined;
            fn end(self) -> InputEnd<K>;
            fn joined(wires: Chain<K>) -> Self::Joined;
        }
    }
}

/// Which output of a node a wire leaves from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum OutPort {
    /// The main bus.
    Main,
    /// One channel.
    Channel(usize),
    /// One event output.
    Event(u16),
    /// One output bus.
    Bus(u8),
}

/// Which input of a node a wire arrives at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum InPort {
    /// The main bus.
    Main,
    /// One channel.
    Channel(usize),
    /// One event input.
    Event(u16),
    /// One input bus.
    Bus(u8),
}

/// Where a wire leaves from: a declared key and one of its outputs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutputEnd<K> {
    pub(crate) key: K,
    pub(crate) port: OutPort,
}

/// Where a wire arrives: a declared key and one of its inputs, or the graph's
/// main output.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InputEnd<K> {
    /// `None` is the main output.
    pub(crate) key: Option<K>,
    pub(crate) port: InPort,
}

/// One wire of a patch, with its options.
#[derive(Clone, Debug)]
pub(crate) struct WireDecl<K> {
    pub(crate) from: OutputEnd<K>,
    pub(crate) to: InputEnd<K>,
    pub(crate) unfaded: bool,
    pub(crate) initial: Option<Gain>,
}

/// A node declared elsewhere in the patch, named by its key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention<K>(K);

impl<K> Mention<K> {
    /// The node declared under `key`: its main bus, or a port through the
    /// methods below.
    pub fn new(key: K) -> Self {
        Self(key)
    }
}

impl<K: Clone> Mention<K> {
    /// Output channel `i`, as a start.
    #[must_use]
    pub fn output(&self, i: usize) -> OutputEnd<K> {
        OutputEnd::of(self.0.clone(), OutPort::Channel(i))
    }

    /// Event output `i`, as a start.
    #[must_use]
    pub fn event_output(&self, i: u16) -> OutputEnd<K> {
        OutputEnd::of(self.0.clone(), OutPort::Event(i))
    }

    /// Output bus `bus`, as a start.
    #[must_use]
    pub fn bus_output(&self, bus: u8) -> OutputEnd<K> {
        OutputEnd::of(self.0.clone(), OutPort::Bus(bus))
    }

    /// Input channel `i`, as an end.
    #[must_use]
    pub fn input(&self, i: usize) -> InputEnd<K> {
        InputEnd::of(self.0.clone(), InPort::Channel(i))
    }

    /// Event input `i`, as an end.
    #[must_use]
    pub fn event_input(&self, i: u16) -> InputEnd<K> {
        InputEnd::of(self.0.clone(), InPort::Event(i))
    }

    /// Input bus `bus`, as an end (a sidechain).
    #[must_use]
    pub fn bus_input(&self, bus: u8) -> InputEnd<K> {
        InputEnd::of(self.0.clone(), InPort::Bus(bus))
    }
}

impl<K> OutputEnd<K> {
    fn of(key: K, port: OutPort) -> Self {
        Self { key, port }
    }
}

impl<K> InputEnd<K> {
    fn of(key: K, port: InPort) -> Self {
        Self {
            key: Some(key),
            port,
        }
    }
}

impl<K: Clone, C> NodeDecl<K, C> {
    fn mention(&self) -> Mention<K> {
        Mention::new(self.key().clone())
    }

    /// Output channel `i`, as a start.
    #[must_use]
    pub fn output(&self, i: usize) -> OutputEnd<K> {
        self.mention().output(i)
    }

    /// Event output `i`, as a start.
    #[must_use]
    pub fn event_output(&self, i: u16) -> OutputEnd<K> {
        self.mention().event_output(i)
    }

    /// Output bus `bus`, as a start.
    #[must_use]
    pub fn bus_output(&self, bus: u8) -> OutputEnd<K> {
        self.mention().bus_output(bus)
    }

    /// Input channel `i`, as an end.
    #[must_use]
    pub fn input(&self, i: usize) -> InputEnd<K> {
        self.mention().input(i)
    }

    /// Event input `i`, as an end.
    #[must_use]
    pub fn event_input(&self, i: u16) -> InputEnd<K> {
        self.mention().event_input(i)
    }

    /// Input bus `bus`, as an end (a sidechain).
    #[must_use]
    pub fn bus_input(&self, bus: u8) -> InputEnd<K> {
        self.mention().bus_input(bus)
    }
}

/// A value a wire can leave from: a [`Mention`], a `&`[`NodeDecl`], an
/// [`OutputEnd`] or a [`Chain`]. Sealed.
pub trait WireStart<K>: sealed::start::Sealed<K> + Sized {
    /// A wire from this start into `to`.
    fn connect<E: WireEnd<K>>(self, to: E) -> E::Joined {
        let (mut wires, from) = self.start();
        wires.wires.push(WireDecl {
            from,
            to: to.end(),
            unfaded: false,
            initial: None,
        });
        E::joined(wires)
    }
}

/// A value a wire can arrive at: a [`Mention`], a `&`[`NodeDecl`], an
/// [`InputEnd`] or `MainOutput`. Sealed.
pub trait WireEnd<K>: sealed::end::Sealed<K> {}

impl<K> sealed::start::Sealed<K> for Mention<K> {
    fn start(self) -> (Chain<K>, OutputEnd<K>) {
        (Chain::empty(), OutputEnd::of(self.0, OutPort::Main))
    }
}

impl<K: Clone, C> sealed::start::Sealed<K> for &NodeDecl<K, C> {
    fn start(self) -> (Chain<K>, OutputEnd<K>) {
        (
            Chain::empty(),
            OutputEnd::of(self.key().clone(), OutPort::Main),
        )
    }
}

impl<K> sealed::start::Sealed<K> for OutputEnd<K> {
    fn start(self) -> (Chain<K>, OutputEnd<K>) {
        (Chain::empty(), self)
    }
}

impl<K: Clone> sealed::start::Sealed<K> for Chain<K> {
    fn start(self) -> (Chain<K>, OutputEnd<K>) {
        let tail = self
            .wires
            .last()
            .and_then(|last| last.to.key.clone())
            .expect("a chain's last wire arrives at a node");
        (self, OutputEnd::of(tail, OutPort::Main))
    }
}

impl<K> WireStart<K> for Mention<K> {}
impl<K: Clone, C> WireStart<K> for &NodeDecl<K, C> {}
impl<K> WireStart<K> for OutputEnd<K> {}
impl<K: Clone> WireStart<K> for Chain<K> {}

impl<K> sealed::end::Sealed<K> for Mention<K> {
    type Joined = Chain<K>;
    fn end(self) -> InputEnd<K> {
        InputEnd::of(self.0, InPort::Main)
    }
    fn joined(wires: Chain<K>) -> Chain<K> {
        wires
    }
}

impl<K: Clone, C> sealed::end::Sealed<K> for &NodeDecl<K, C> {
    type Joined = Chain<K>;
    fn end(self) -> InputEnd<K> {
        InputEnd::of(self.key().clone(), InPort::Main)
    }
    fn joined(wires: Chain<K>) -> Chain<K> {
        wires
    }
}

impl<K> sealed::end::Sealed<K> for InputEnd<K> {
    type Joined = Connected<K>;
    fn end(self) -> InputEnd<K> {
        self
    }
    fn joined(chain: Chain<K>) -> Connected<K> {
        chain.ended()
    }
}

impl<K> WireEnd<K> for Mention<K> {}
impl<K: Clone, C> WireEnd<K> for &NodeDecl<K, C> {}
impl<K> WireEnd<K> for InputEnd<K> {}

/// Wires made so far toward a node, which may go on: `a.connect(&b)` is a
/// chain, and `.connect(&c)` adds `b` into `c`. A part of a patch.
#[must_use = "a wire is part of a patch only once `Session::patch` takes it"]
#[derive(Clone, Debug)]
pub struct Chain<K> {
    wires: Vec<WireDecl<K>>,
}

impl<K> Chain<K> {
    /// The chain's wires, ended: what a wire into a port gives.
    pub(crate) fn ended(self) -> Connected<K> {
        Connected { wires: self.wires }
    }

    /// The start of a chain: no wire yet, which `connect` makes the first.
    fn empty() -> Self {
        Self { wires: Vec::new() }
    }
}

/// Wires that end at a port or at the main output. A part of a patch.
#[must_use = "a wire is part of a patch only once `Session::patch` takes it"]
#[derive(Clone, Debug)]
pub struct Connected<K> {
    wires: Vec<WireDecl<K>>,
}

/// The options of a wire's last wire, shared by [`Chain`] and [`Connected`].
macro_rules! last_wire_options {
    ($($ty:ident),+) => {$(
        impl<K> $ty<K> {
            /// Makes the last wire connect, and later disconnect, with no
            /// fade: at the next block boundary.
            pub fn unfaded(mut self) -> Self {
                self.last().unfaded = true;
                self
            }

            /// Gives the last wire a gain lane at `gain`. An initial, as a
            /// config's is: a patch that changes it writes the new value, and a
            /// `set` on the wire's [`GainTarget`](crate::GainTarget) wins
            /// until then. Only a wire that carries a gain takes it: a bus or
            /// event wire is refused by the patch.
            pub fn initial_gain(mut self, gain: Gain) -> Self {
                self.last().initial = Some(gain);
                self
            }

            fn last(&mut self) -> &mut WireDecl<K> {
                self.wires.last_mut().expect("a wire has its first wire")
            }
        }

        impl<K, G: PatchTarget> IntoPatch<K, G> for $ty<K> {
            fn add_to(self, patch: &mut Declarations<K, G>, _: Sealed) {
                patch.wires.extend(self.wires);
            }
        }
    )+};
}

last_wire_options!(Chain, Connected);
