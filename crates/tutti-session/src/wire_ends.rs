//! Wires in a patch: what a node's output is connected to, by keys and ports.
//!
//! A wire is the kind of wire its two ends are, as in [`Graph::connect`](crate::Graph::connect): a
//! [`Mention`] or a `&`[`NodeDecl`] is a node's main bus
//! ([`MainBusWire`]), `output(i)` and `input(i)` are one channel
//! ([`ChannelWire`]), `bus_output(b)` and `bus_input(b)` a bus
//! ([`BusWire`]), `event_output(i)` and `event_input(i)` an event port
//! ([`EventWire`]). [`MainOutput`] ends the wires it does as an [`Inlet`](crate::Inlet).
//!
//! `a.connect(b)` is one wire. Toward a node it is a [`Chain`], which goes on
//! from that node's main bus: `a.connect(&b).connect(&c)` is `a` into `b` and
//! `b` into `c`. Toward a port or `MainOutput` it is a [`Connected`], which
//! ends. Both are parts of a patch, and their options apply to the last wire
//! only.

use std::marker::PhantomData;

use tutti_units::Gain;

use crate::patch::{Declarations, IntoPatch, NodeDecl, PatchTarget, WireDecl, WireEnds};
use crate::{BusWire, ChannelWire, EventWire, MainBusWire, MainOutput, Weighted, Wire};

pub(crate) mod sealed {
    /// What a start hands `connect`.
    pub(crate) mod start {
        use super::super::{Chain, OutputEnd};

        pub trait Sealed<K> {
            /// The kind of wire it starts.
            type Wire: crate::Wire;
            /// The wires made so far, and where the next one leaves from.
            fn start(self) -> (Chain<K>, OutputEnd<K, Self::Wire>);
        }
    }

    /// What an end hands `connect`.
    pub(crate) mod end {
        use super::super::{Chain, InputEnd};

        pub trait Sealed<K, W: crate::Wire> {
            /// What the wire into this end gives: a chain toward a node, a
            /// connection toward a port.
            type Joined;
            fn end(self) -> InputEnd<K, W>;
            fn joined(wires: Chain<K>) -> Self::Joined;
        }
    }
}

/// Which output of a node a wire leaves from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Out {
    Main,
    Channel(usize),
    Event(u16),
    Bus(u8),
}

/// Which input of a node a wire arrives at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum In {
    Main,
    Channel(usize),
    Event(u16),
    Bus(u8),
}

/// Where a wire of kind `W` leaves from: a declared key and one of its
/// outputs.
#[derive(Clone, Debug)]
pub struct OutputEnd<K, W: Wire> {
    key: K,
    port: Out,
    kind: PhantomData<fn() -> W>,
}

/// Where a wire of kind `W` arrives: a declared key and one of its inputs, or
/// the graph's main output.
#[derive(Clone, Debug)]
pub struct InputEnd<K, W: Wire> {
    /// `None` is the main output.
    key: Option<K>,
    port: In,
    kind: PhantomData<fn() -> W>,
}

impl<K, W: Wire> OutputEnd<K, W> {
    fn of(key: K, port: Out) -> Self {
        Self {
            key,
            port,
            kind: PhantomData,
        }
    }
}

impl<K, W: Wire> InputEnd<K, W> {
    fn of(key: Option<K>, port: In) -> Self {
        Self {
            key,
            port,
            kind: PhantomData,
        }
    }
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
    pub fn output(&self, i: usize) -> OutputEnd<K, ChannelWire> {
        OutputEnd::of(self.0.clone(), Out::Channel(i))
    }

    /// Event output `i`, as a start.
    #[must_use]
    pub fn event_output(&self, i: u16) -> OutputEnd<K, EventWire> {
        OutputEnd::of(self.0.clone(), Out::Event(i))
    }

    /// Output bus `bus`, as a start.
    #[must_use]
    pub fn bus_output(&self, bus: u8) -> OutputEnd<K, BusWire> {
        OutputEnd::of(self.0.clone(), Out::Bus(bus))
    }

    /// Input channel `i`, as an end.
    #[must_use]
    pub fn input(&self, i: usize) -> InputEnd<K, ChannelWire> {
        InputEnd::of(Some(self.0.clone()), In::Channel(i))
    }

    /// Event input `i`, as an end.
    #[must_use]
    pub fn event_input(&self, i: u16) -> InputEnd<K, EventWire> {
        InputEnd::of(Some(self.0.clone()), In::Event(i))
    }

    /// Input bus `bus`, as an end (a sidechain).
    #[must_use]
    pub fn bus_input(&self, bus: u8) -> InputEnd<K, BusWire> {
        InputEnd::of(Some(self.0.clone()), In::Bus(bus))
    }
}

impl<K: Clone, C> NodeDecl<K, C> {
    fn mention(&self) -> Mention<K> {
        Mention::new(self.key().clone())
    }

    /// Output channel `i`, as a start.
    #[must_use]
    pub fn output(&self, i: usize) -> OutputEnd<K, ChannelWire> {
        self.mention().output(i)
    }

    /// Event output `i`, as a start.
    #[must_use]
    pub fn event_output(&self, i: u16) -> OutputEnd<K, EventWire> {
        self.mention().event_output(i)
    }

    /// Output bus `bus`, as a start.
    #[must_use]
    pub fn bus_output(&self, bus: u8) -> OutputEnd<K, BusWire> {
        self.mention().bus_output(bus)
    }

    /// Input channel `i`, as an end.
    #[must_use]
    pub fn input(&self, i: usize) -> InputEnd<K, ChannelWire> {
        self.mention().input(i)
    }

    /// Event input `i`, as an end.
    #[must_use]
    pub fn event_input(&self, i: u16) -> InputEnd<K, EventWire> {
        self.mention().event_input(i)
    }

    /// Input bus `bus`, as an end (a sidechain).
    #[must_use]
    pub fn bus_input(&self, bus: u8) -> InputEnd<K, BusWire> {
        self.mention().bus_input(bus)
    }
}

/// The wire between two ends of one kind.
fn join<K, W: Wire>(from: OutputEnd<K, W>, to: InputEnd<K, W>) -> WireEnds<K> {
    match (from.port, to.key, to.port) {
        (Out::Main, to, In::Main) => WireEnds::Main(from.key, to),
        (Out::Channel(i), Some(to), In::Channel(j)) => WireEnds::Channel(from.key, i, to, j),
        (Out::Event(i), Some(to), In::Event(j)) => WireEnds::Event(from.key, i, to, j),
        (Out::Bus(b), Some(to), In::Bus(c)) => WireEnds::Bus(from.key, b, Some((to, c))),
        (Out::Bus(b), None, In::Main) => WireEnds::Bus(from.key, b, None),
        _ => unreachable!("an end's port is made for its kind, and both ends are of one"),
    }
}

/// A value a wire can leave from: a [`Mention`], a `&`[`NodeDecl`] (a main
/// bus), an [`OutputEnd`] or a [`Chain`]. Sealed.
pub trait WireStart<K>: sealed::start::Sealed<K> + Sized {
    /// A wire from this start into `to`, an end of the same kind of wire.
    fn connect<E>(self, to: E) -> <E as sealed::end::Sealed<K, Self::Wire>>::Joined
    where
        E: WireEnd<K, Self::Wire>,
    {
        let (mut chain, from) = self.start();
        chain.wires.push(WireDecl {
            ends: join(from, to.end()),
            unfaded: false,
            initial: None,
        });
        E::joined(chain)
    }
}

/// A value a wire of kind `W` can arrive at: a [`Mention`] or a
/// `&`[`NodeDecl`] (a main bus, `W = MainBusWire`), an [`InputEnd`] or
/// [`MainOutput`]. Sealed.
pub trait WireEnd<K, W: Wire>: sealed::end::Sealed<K, W> {}

impl<K> sealed::start::Sealed<K> for Mention<K> {
    type Wire = MainBusWire;
    fn start(self) -> (Chain<K>, OutputEnd<K, MainBusWire>) {
        (Chain::empty(), OutputEnd::of(self.0, Out::Main))
    }
}

impl<K: Clone, C> sealed::start::Sealed<K> for &NodeDecl<K, C> {
    type Wire = MainBusWire;
    fn start(self) -> (Chain<K>, OutputEnd<K, MainBusWire>) {
        (Chain::empty(), OutputEnd::of(self.key().clone(), Out::Main))
    }
}

impl<K, W: Wire> sealed::start::Sealed<K> for OutputEnd<K, W> {
    type Wire = W;
    fn start(self) -> (Chain<K>, OutputEnd<K, W>) {
        (Chain::empty(), self)
    }
}

impl<K: Clone> sealed::start::Sealed<K> for Chain<K> {
    type Wire = MainBusWire;
    fn start(self) -> (Chain<K>, OutputEnd<K, MainBusWire>) {
        let tail = self
            .wires
            .last()
            .and_then(|last| last.ends.sink().cloned())
            .expect("a chain's last wire arrives at a node");
        (self, OutputEnd::of(tail, Out::Main))
    }
}

impl<K> WireStart<K> for Mention<K> {}
impl<K: Clone, C> WireStart<K> for &NodeDecl<K, C> {}
impl<K, W: Wire> WireStart<K> for OutputEnd<K, W> {}
impl<K: Clone> WireStart<K> for Chain<K> {}

impl<K> sealed::end::Sealed<K, MainBusWire> for Mention<K> {
    type Joined = Chain<K>;
    fn end(self) -> InputEnd<K, MainBusWire> {
        InputEnd::of(Some(self.0), In::Main)
    }
    fn joined(chain: Chain<K>) -> Chain<K> {
        chain
    }
}

impl<K: Clone, C> sealed::end::Sealed<K, MainBusWire> for &NodeDecl<K, C> {
    type Joined = Chain<K>;
    fn end(self) -> InputEnd<K, MainBusWire> {
        InputEnd::of(Some(self.key().clone()), In::Main)
    }
    fn joined(chain: Chain<K>) -> Chain<K> {
        chain
    }
}

impl<K, W: Wire> sealed::end::Sealed<K, W> for InputEnd<K, W> {
    type Joined = Connected<K, W>;
    fn end(self) -> InputEnd<K, W> {
        self
    }
    fn joined(chain: Chain<K>) -> Connected<K, W> {
        chain.ended()
    }
}

/// What ends the wires the main output is an [`Inlet`](crate::Inlet)(crate::Inlet) of (a
/// main bus and a bus edge). A trait of its own so that the main output names
/// nothing of the wire ends, only the blanket impls below do.
trait MainEnd {}

impl MainEnd for MainOutput {}

macro_rules! main_end {
    ($($wire:ty),+) => {$(
        impl<K, T: MainEnd> sealed::end::Sealed<K, $wire> for T {
            type Joined = Connected<K, $wire>;
            fn end(self) -> InputEnd<K, $wire> {
                InputEnd::of(None, In::Main)
            }
            fn joined(chain: Chain<K>) -> Connected<K, $wire> {
                chain.ended()
            }
        }

        impl<K, T: MainEnd> WireEnd<K, $wire> for T {}
    )+};
}

main_end!(MainBusWire, BusWire);

impl<K> WireEnd<K, MainBusWire> for Mention<K> {}
impl<K: Clone, C> WireEnd<K, MainBusWire> for &NodeDecl<K, C> {}
impl<K, W: Wire> WireEnd<K, W> for InputEnd<K, W> {}

/// Wires made so far toward a node, which may go on from that node's main
/// bus: `a.connect(&b)` is a chain, and `.connect(&c)` adds `b` into `c`. A
/// part of a patch.
#[must_use = "a wire is part of a patch only once `Session::patch` takes it"]
#[derive(Clone, Debug)]
pub struct Chain<K> {
    wires: Vec<WireDecl<K>>,
}

/// Wires that end at a port or at the main output, the last of kind `W`. A
/// part of a patch.
#[must_use = "a wire is part of a patch only once `Session::patch` takes it"]
#[derive(Clone, Debug)]
pub struct Connected<K, W: Wire> {
    wires: Vec<WireDecl<K>>,
    kind: PhantomData<fn() -> W>,
}

impl<K> Chain<K> {
    /// The start of a chain: no wire yet, which `connect` makes the first.
    fn empty() -> Self {
        Self { wires: Vec::new() }
    }

    /// The chain's wires, ended by a wire of kind `W`.
    fn ended<W: Wire>(self) -> Connected<K, W> {
        Connected {
            wires: self.wires,
            kind: PhantomData,
        }
    }
}

/// The options of a wire's last wire, shared by [`Chain`] and [`Connected`].
macro_rules! last_wire_options {
    ([$($g:tt)*] $ty:ty) => {
        impl<$($g)*> $ty {
            /// Makes the last wire connect, and later disconnect, with no
            /// fade: at the next block boundary. An event wire is never
            /// faded, so it changes nothing there.
            pub fn unfaded(mut self) -> Self {
                self.last().unfaded = true;
                self
            }

            fn last(&mut self) -> &mut WireDecl<K> {
                self.wires.last_mut().expect("a wire has its first wire")
            }
        }
    };
}

last_wire_options!([K] Chain<K>);
last_wire_options!([K, W: Wire] Connected<K, W>);

impl<K> Chain<K> {
    /// Gives the last wire a gain lane at `gain`: an initial, as a config's
    /// is. A patch that changes it writes the new value, and a `set` on the
    /// wire's [`GainTarget`](crate::GainTarget) wins until then.
    pub fn initial_gain(mut self, gain: Gain) -> Self {
        self.last().initial = Some(gain);
        self
    }
}

impl<K, W: Weighted> Connected<K, W> {
    /// Gives the last wire a gain lane at `gain`: an initial, as a config's
    /// is. A patch that changes it writes the new value, and a `set` on the
    /// wire's [`GainTarget`](crate::GainTarget) wins until then.
    pub fn initial_gain(mut self, gain: Gain) -> Self {
        self.last().initial = Some(gain);
        self
    }
}

impl<K, G: PatchTarget> IntoPatch<K, G> for Chain<K> {
    fn add_to(self, patch: &mut Declarations<K, G>, _: crate::patch::Sealed) {
        patch.wires.extend(self.wires);
    }
}

impl<K, W: Wire, G: PatchTarget> IntoPatch<K, G> for Connected<K, W> {
    fn add_to(self, patch: &mut Declarations<K, G>, _: crate::patch::Sealed) {
        patch.wires.extend(self.wires);
    }
}
