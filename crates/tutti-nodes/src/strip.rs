//! The mixer strip — volume and stereo balance — as one addressable unit.
//!
//! A mixer bus applies two things to a summed signal: a fader and a balance.
//! [`BusStripNode`] is both in one node, addressed by [`UnitParam`] through
//! the [`ParamSet`](tutti_graph::ParamSet) it is inserted with, so a host sets it with the same
//! generic machinery it uses for a filter cutoff and needs no downcast. A
//! strip's mute is not here: muting is graph state, `NodeMode::Muted`
//! (`Graph::set_mode`), the same for every node.
//!
//! Every scalar lives in a cell the node's [`ParamSet`](tutti_graph::ParamSet) addresses by
//! [`UnitParam`] — the same shape every other node in this crate uses — so a
//! host has one param path to the strip, not a second set of handles beside
//! it.
//!
//! # Balance, not panning
//!
//! A panner is **mono**-to-stereo: one signal placed between two speakers. A
//! mixer strip *rebalances* signal that is already placed, and must leave it
//! untouched at centre. The law is [`Balance::gains`]: the side it moves away
//! from falls linearly and the other stays at unity.
//!
//! The strip applies it by **placement**. Every left/right pair of its bus
//! (front, side, back and height pairs alike) takes the law, the left speaker
//! the left gain and the right speaker the right one. A speaker is paired with
//! its [`mirror`](tutti_buffers::Speaker::mirror) when that speaker is on the bus,
//! found by the speaker and never by the channel index, so a 5.1 strip balances
//! `FL`/`FR` and `SL`/`SR` in whatever order its channels run. The centre, the
//! LFE and any speaker whose mirror is absent take the fader alone.
//!
//! A bus with no placement (unassigned channels), an ambisonic bus, a mono bus
//! and any bus with no mirrored pair have nothing to balance, so the strip does
//! not declare the param there: its shape has no balance port, its `ParamSet`
//! refuses the address and [`BusStripNode::balance`] is `None`. Those strips
//! are a fader.
//!
//! # Every control change is a ramp, never a step
//!
//! Volume and balance are read **once per block** and the gain they imply is
//! reached by a linear ramp across that block, starting from the gain the
//! previous block ended on. A step in gain is a step in the waveform — zipper
//! noise on a dragged fader — and the ramp is what removes it. The ramp ends
//! exactly on the new gain at the block's last sample. See
//! `BusStripNode::render`.

use crate::last_good::LastGood;
use tutti_buffers::{ChannelTopology, Float, MAX_CHANNELS};
use tutti_graph::{Cx, Inputs, Io, Node, Outputs, Prepare, Shape, Status};
use tutti_rt::Param;
use tutti_units::UnitParam;
use tutti_units::{Amplitude, Balance, Tail};

// The params a [`BusStripNode`] lets the graph modulate, in port order, and the
// [`BusStripConfig`] it is made from: the bus, and the volume at the start. The balance is
// present only on a bus with a left/right pair, which `From` decides.
tutti_graph::declare_params!([] BusStripNode, #[derive(Copy, PartialEq)] BusStripConfig, BusStripCells {
    required { input: ChannelTopology }
    params {
        Volume => volume: Amplitude = Amplitude::UNITY,
        Balance => balance: Option<Balance> = None,
    }
});

/// A mixer strip: volume and balance over `channels` audio ports.
///
/// Ports are `channels` audio inputs → `channels` outputs. Volume and balance
/// are modulatable by the graph, in that port order: a per-frame value on the
/// param port ([`Io::param`](tutti_graph::Io::param)) overrides its cell per
/// sample. A strip with no left/right pair has no balance port (see
/// [`balance`](Self::balance)).
/// There is deliberately no mute: it is `NodeMode::Muted`, which the graph
/// holds for any node.
///
/// Params, by address through its [`ParamSet`](tutti_graph::ParamSet):
///
/// - [`UnitParam::Volume`], a linear amplitude;
/// - [`UnitParam::Balance`], a [`Balance`] (`-1..1`), only on a bus with a
///   left/right pair.
///
/// The set refuses anything else, which is what lets a host push params
/// without knowing the node type.
///
/// Every control is read **once per block**; no cell is loaded per sample.
///
/// Made from a [`BusStripConfig`] ([`From`]): unity volume, centred.
///
/// ```
/// use tutti_buffers::ChannelTopology;
/// use tutti_nodes::{BusStripConfig, BusStripNode};
///
/// let strip = BusStripNode::from(BusStripConfig::new(ChannelTopology::STEREO));
/// assert_eq!(strip.topology(), ChannelTopology::STEREO);
/// ```
///
/// `Clone` shares the two cells: it is the template
/// [`tutti_graph::param_parts`] forks from ([`ParamNode::fork_fresh`](tutti_graph::ParamNode::fork_fresh) is the
/// copy that shares nothing).
#[derive(Clone, Debug)]
pub struct BusStripNode {
    /// The volume, and the balance where `topology` has a left/right pair to balance.
    cells: BusStripCells,
    /// The bus this strip runs on, in and out: the speakers its channels feed.
    ///
    /// Balance applies to the left/right pairs this placement has (see
    /// [`Side`]) and leaves every other channel at the fader alone.
    ///
    /// A placed bus is what lets a bus edge fold into or out of the strip: a mono
    /// source reaches both sides of a stereo strip, and a stereo strip lands on the
    /// front pair of a declared 5.1 main output.
    topology: ChannelTopology,
    /// Each channel's side of the balance, from `topology`'s placement: built
    /// once at construction, read once per block.
    sides: [Side; MAX_CHANNELS],
    /// Which params the previous block was modulated on (bit `k`). A change is not
    /// ramped by the strip: the graph declicks the fed value itself, and
    /// ramping the control gain across the same block would apply the gain
    /// twice for its length.
    last_fed: u8,
    /// The control-driven gains the previous block ended on — where this
    /// block's ramp starts. `None` before the first block and after
    /// [`reset`](Node::reset): with no previous output there is nothing to
    /// be continuous with, so the first block starts at its target.
    ///
    /// Per-node state, not shared with clones: it describes what *this* node
    /// last emitted.
    ramp_from: Option<StripGains>,
    /// The last finite volume read: a cell a host wrote a NaN or an infinity
    /// into reads as unchanged (see [`LastGood`]). Balance needs none: a NaN
    /// balance already reads as unity on both sides ([`Balance::gains`]).
    good: LastGood<f64>,
}

/// Which side of the balance a channel is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// A speaker with no mirror image: the fader alone.
    Mid,
    /// The left speaker of a pair: the balance's left gain.
    Left,
    /// The right speaker of a pair: the balance's right gain.
    Right,
}

/// The side of each channel of `topology`: a speaker is `Left` or `Right` when
/// its [`mirror`](tutti_buffers::Speaker::mirror) is also on the bus, and `Mid` otherwise. A bus
/// with no placement or an ambisonic one has no pair, so every channel is `Mid`.
/// Control side: runs once.
fn sides_of(topology: &ChannelTopology) -> [Side; MAX_CHANNELS] {
    let mut sides = [Side::Mid; MAX_CHANNELS];
    let Some(positions) = topology.positions() else {
        return sides;
    };
    for (side, &speaker) in sides.iter_mut().zip(positions) {
        if speaker.mirror().is_some_and(|m| positions.contains(&m)) {
            *side = if speaker.is_left() {
                Side::Left
            } else {
                Side::Right
            };
        }
    }
    sides
}

/// The gain on each kind of channel: the left and right of a balanced pair, and
/// every channel without a mirror (which takes volume but no balance).
///
/// The unit a ramp interpolates. Ramping the *gains* rather than the two
/// controls is what makes the ramp linear in the signal: `balance × volume`
/// with each factor ramped separately would be quadratic across the block.
#[derive(Clone, Copy, Debug, PartialEq)]
struct StripGains {
    left: Amplitude,
    right: Amplitude,
    rest: Amplitude,
}

impl StripGains {
    /// The gain for a channel on `side`.
    #[inline]
    fn on(self, side: Side) -> Amplitude {
        match side {
            Side::Left => self.left,
            Side::Right => self.right,
            Side::Mid => self.rest,
        }
    }
}

impl From<BusStripConfig> for BusStripNode {
    fn from(config: BusStripConfig) -> Self {
        let topology = config.input;
        let sides = sides_of(&topology);
        let config = config.balance(sides.contains(&Side::Left).then_some(Balance::CENTER));
        Self {
            cells: BusStripCells::from_config(&config),
            sides,
            topology,
            last_fed: 0,
            good: LastGood::fixed(1.0),
            ramp_from: None,
        }
    }
}

impl BusStripNode {
    /// The bus this strip runs on.
    pub fn topology(&self) -> ChannelTopology {
        self.topology
    }

    /// The audio channel count (its output count).
    #[inline]
    fn channels(&self) -> usize {
        self.topology.width().count() as usize
    }

    /// A handle on the shared volume cell, the one the strip's `ParamSet`
    /// addresses as `Volume`.
    ///
    /// The fader is a linear [`Amplitude`], unclamped: `1.0` is unity, `0.0` silent, and
    /// values above 1.0 amplify. It is read once per block and ramped to across it.
    #[must_use]
    pub fn volume(&self) -> Param<Amplitude> {
        self.cells.volume.handle()
    }

    /// A handle on the shared balance cell, the one the strip's `ParamSet`
    /// addresses as `Balance`; `None` on a bus with no left/right pair, which has
    /// nothing to balance.
    #[must_use]
    pub fn balance(&self) -> Option<Param<Balance>> {
        self.cells.balance.as_ref().map(Param::handle)
    }

    /// Gains for a fader at `volume` and a balance at `balance`.
    #[inline]
    fn gains_for(volume: Amplitude, balance: Balance) -> StripGains {
        // Cascading two gain stages multiplies them, which is exactly what
        // balance × fader is.
        let (l, r) = balance.gains();
        StripGains {
            left: l * volume.get(),
            right: r * volume.get(),
            rest: volume,
        }
    }

    /// The gains the **control cells** ask for — the ramp's target.
    ///
    /// A control the graph modulates (`fed`: volume, balance) contributes unity
    /// here, and its value comes from [`fed_gain`](Self::fed_gain) per
    /// sample instead.
    ///
    /// Reads two atomics; called once per block, never per sample.
    #[inline]
    fn control_gains(&mut self, fed: (bool, bool)) -> StripGains {
        let volume = match fed.0 {
            true => Amplitude::UNITY,
            false => Amplitude::new(self.good.read(self.cells.volume.load().get())),
        };
        let balance = match (fed.1, &self.cells.balance) {
            (false, Some(cell)) => cell.load(),
            _ => Balance::CENTER,
        };
        Self::gains_for(volume, balance)
    }

    /// Render one block: the one gain path.
    ///
    /// Reads the control cells **once**, then ramps linearly from the gains the
    /// previous call ended on to the ones just read, landing exactly on them at
    /// the last frame. A one-frame block's ramp is therefore a step.
    ///
    /// # The ramp is one block long, so its length is the caller's block size
    ///
    /// At the engine's 64-frame chunks that is ~1.3 ms at 48 kHz: long enough to
    /// turn a fader's step into a slope with no broadband click. A caller that
    /// renders one frame at a time gets a step.
    ///
    /// # Channel-major, so it vectorises along time
    ///
    /// Each channel is one pass over its two slices with a gain that is a
    /// function of the frame alone: no per-sample branch, channel lookup or
    /// slice fetch, so the compiler runs it in SIMD lanes. Every sample takes
    /// the same arithmetic in the same order as one gain per frame applied to
    /// every channel would, so the order of the loops changes no bit.
    #[inline]
    fn render<F: Float>(
        &mut self,
        inputs: &Inputs<'_, F>,
        outputs: &mut Outputs<'_, '_, F>,
        size: usize,
        volume: Option<&[F]>,
        balance: Option<&[F]>,
    ) {
        let fed = (volume.is_some(), balance.is_some());
        let fed_bits = u8::from(fed.0) | u8::from(fed.1) << 1;
        let target = self.control_gains(fed);
        let from = self.ramp_from.replace(target).unwrap_or(target);
        // A param that started or stopped being fed is not ramped here: the
        // graph declicks the fed value, and ramping the control gain from its
        // old share too would apply the gain twice across the block.
        let from = if fed_bits == self.last_fed {
            from
        } else {
            target
        };
        self.last_fed = fed_bits;
        // Per-block facts; each channel picks its loop once. The steady case skips the
        // interpolation outright, which also keeps its output bit-identical
        // to the unramped arithmetic.
        let ramping = from != target;
        let (volume, balance) = (volume.map(|v| &v[..size]), balance.map(|p| &p[..size]));

        for (c, &side) in self.sides[..self.channels()].iter().enumerate() {
            // The one place a gain meets a sample. A sample is a signal
            // value, not a gain, so the unit comes off the gain here rather
            // than the sample being wrapped.
            let (x, y) = (&inputs.get(c)[..size], &mut outputs.get(c)[..size]);
            let (start, end) = (from.on(side), target.on(side));
            // `(i + 1) / size`, not `(i + 1) * (1 / size)`: the division is
            // exact at the last frame (`size / size == 1`), so the ramp lands
            // on the target rather than an ulp beside it.
            // The endpoints cross into the sample width once per block, and the ramp runs in it.
            let (start, end) = (F::from_unit(start), F::from_unit(end));
            // The fraction is `(i + 1)` times `1 / size`, both in the sample width, and the last
            // frame is `1` itself, so the ramp lands on the target exactly.
            debug_assert!(u32::try_from(size).is_ok());
            let inv = F::from_design(1.0 / size as f64);
            let ramp = |i: usize| {
                let t = if i + 1 == size {
                    F::ONE
                } else {
                    F::from_count((i + 1) as u32) * inv
                };
                lerp(start, end, t)
            };
            let steady = end;
            match (ramping, volume.is_some() || balance.is_some()) {
                (false, false) => scale(x, y, |_| steady),
                (true, false) => scale(x, y, ramp),
                (false, true) => Self::scale_fed(x, y, |_| steady, volume, balance, side),
                (true, true) => Self::scale_fed(x, y, ramp, volume, balance, side),
            }
        }
    }

    /// [`scale`] by `base(i)` times a channel's [`fed_gain`](Self::fed_gain),
    /// with `side` a constant in each loop so only its side is computed.
    #[inline(always)]
    fn scale_fed<F: Float>(
        x: &[F],
        y: &mut [F],
        base: impl Fn(usize) -> F,
        volume: Option<&[F]>,
        balance: Option<&[F]>,
        side: Side,
    ) {
        match side {
            Side::Left => scale(x, y, |i| {
                base(i) * Self::fed_gain(volume, balance, i, Side::Left)
            }),
            Side::Right => scale(x, y, |i| {
                base(i) * Self::fed_gain(volume, balance, i, Side::Right)
            }),
            Side::Mid => scale(x, y, |i| {
                base(i) * Self::fed_gain(volume, balance, i, Side::Mid)
            }),
        }
    }

    /// A channel on `side`'s gain from the fed params at frame `i`, unity for
    /// one that is not fed: [`gains_for`](Self::gains_for), computing only what
    /// that side takes (a channel without a mirror, the fader alone).
    ///
    /// Not ramped: a fed value is already a signal in the sample width, and the graph that
    /// feeds it owns its continuity.
    #[inline(always)]
    fn fed_gain<F: Float>(volume: Option<&[F]>, balance: Option<&[F]>, i: usize, side: Side) -> F {
        // The fed signals are already the sample width: a fed value is used as it is, and the side
        // gains are `Balance::gains`' arithmetic in that width (a NaN balance reads as unity).
        let fader = volume.map_or(F::ONE, |v| v[i]);
        if side == Side::Mid {
            return fader;
        }
        let one = F::ONE;
        // Comparisons, not `clamp` and `min`: a NaN falls through to unity, and the selects
        // vectorise where the NaN-propagating forms do not.
        let at_most_one = |x: F| if x < one { x } else { one };
        let b = balance.map_or(F::ZERO, |b| {
            let b = b[i];
            if b < -one {
                -one
            } else if b > one {
                one
            } else {
                b
            }
        });
        let side_gain = match side {
            Side::Right => at_most_one(one + b),
            _ => at_most_one(one - b),
        };
        side_gain * fader
    }
}

/// `from` at `t = 0`, `to` at `t = 1` — and *exactly* `to` there.
///
/// Written as `from·(1−t) + to·t` rather than `from + (to−from)·t` for that
/// endpoint: the second form rounds, so a ramp to silence could end a few
/// ulps above zero and a strip faded to nothing would leak. Over the sample width's bare
/// gains, narrowed once per block: a per-frame `f64` ramp would not vectorise with the product.
#[inline(always)]
fn lerp<F: Float>(from: F, to: F, t: F) -> F {
    tutti_dsp::lerp(from, to, t)
}

/// `y = x · gain(frame)`, over one channel's block.
///
/// `#[inline(always)]` so each caller's `gain` is compiled into the loop and
/// the loop vectorises with it.
#[inline(always)]
fn scale<F: Float>(x: &[F], y: &mut [F], gain: impl Fn(usize) -> F) {
    for (i, (y, &x)) in y.iter_mut().zip(x).enumerate() {
        *y = x * gain(i);
    }
}

impl<F: Float> Node<F> for BusStripNode {
    /// `channels` in and out, volume then balance modulatable.
    /// Volume and balance are per-frame gains, so it stops
    /// with its input.
    fn shape(&self) -> Shape {
        Shape::through(self.topology)
            // Measured: steady and ramping are far under the `Light`/`Moderate` boundary.
            // With the volume fed per frame it is borderline: about 0.7 of
            // the way from a gain to `SvfFilterNode` on a log scale, the
            // boundary about 0.73. `Light`, as a wrong `Light` costs little.
            .cost(tutti_graph::Cost::Light)
            .tail(Tail::None)
            .param_addrs(self.declared_ports().as_slice())
    }

    /// Nothing is rate-dependent: the ramp is one block long, whatever the
    /// rate.
    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let size = io.frames();
        let (volume, balance) = (
            self.declared_param::<{ UnitParam::Volume as u16 }, _>(&io)
                .frames(),
            self.declared_param::<{ UnitParam::Balance as u16 }, _>(&io)
                .frames(),
        );
        let (inputs, mut outputs) = io.split();
        self.render(&inputs, &mut outputs, size, volume, balance);
        Status::Modified
    }

    /// Forget the previous block's gains, so the next block starts at its
    /// target rather than ramping from a signal that is no longer playing.
    fn reset(&mut self) {
        self.ramp_from = None;
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE, tick};
    use tutti_buffers::Planes;
    use tutti_buffers::{
        AmbisonicFormat, AmbisonicOrder, Bed, ChannelLayout, ChannelTopology, Speaker, Speaker::*,
    };
    use tutti_graph::ParamNode;
    use tutti_graph::contract::{assert_param_fork, drive};

    /// The strip on `bus`.
    fn strip(bus: ChannelTopology) -> BusStripNode {
        BusStripNode::from(BusStripConfig::new(bus))
    }

    /// The engine's bus at `n` channels.
    fn width(n: u16) -> ChannelTopology {
        ChannelTopology::default_for(ChannelLayout::from(n)).expect("a test width fits a bus")
    }

    /// Left, right, centre: a placed bus of three.
    fn lcr() -> ChannelTopology {
        ChannelTopology::new([FrontLeft, FrontRight, FrontCenter]).expect("three speakers fit a bus")
    }

    /// The strip measured as a `Light` node, at every width.
    #[test]
    fn the_strip_is_light() {
        for topology in [ChannelTopology::MONO, ChannelTopology::STEREO] {
            let strip = strip(topology);
            assert_eq!(Node::<f32>::shape(&strip).cost, tutti_graph::Cost::Light);
        }
    }

    /// Set a param the way a host does — by address, through the node's
    /// `ParamSet` — rather than by calling the setter, so the test covers the
    /// path `AudioParam<U, P>` uses. `false` if the strip refused it.
    fn set_param(strip: &BusStripNode, param: UnitParam, value: f32) -> bool {
        ParamNode::<f32>::param_set(strip).set(param, f64::from(value))
    }

    fn set_balance(strip: &BusStripNode, balance: f64) {
        // The param word is `f32`.
        assert!(set_param(strip, UnitParam::Balance, balance as f32));
    }

    fn tick2(strip: &mut BusStripNode, l: f32, r: f32) -> (f32, f32) {
        let mut out = [0.0f32; 2];
        tick(strip, &[l, r], &mut out);
        (out[0], out[1])
    }

    /// A fork starts from the values last set through the strip's
    /// `ParamSet` and shares no cell with it.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(strip(ChannelTopology::STEREO));
    }

    /// A strip must be transparent until somebody moves it. An equal-power
    /// law would attenuate a centred signal by 3 dB.
    #[test]
    fn unity_at_defaults() {
        let mut s = strip(ChannelTopology::STEREO);
        assert_eq!(tick2(&mut s, 0.5, -0.25), (0.5, -0.25));
    }

    #[test]
    fn volume_scales_both_channels() {
        let mut s = strip(ChannelTopology::STEREO);
        s.volume().store(Amplitude::new(0.5));
        assert_eq!(tick2(&mut s, 1.0, 1.0), (0.5, 0.5));
    }

    #[test]
    fn balance_attenuates_the_far_channel_only() {
        let mut s = strip(ChannelTopology::STEREO);
        set_balance(&s, -1.0); // hard left
        assert_eq!(tick2(&mut s, 1.0, 1.0), (1.0, 0.0));
        set_balance(&s, 1.0); // hard right
        assert_eq!(tick2(&mut s, 1.0, 1.0), (0.0, 1.0));
        // Halfway: the near channel stays at unity, the far one is halved.
        set_balance(&s, -0.5);
        let (l, r) = tick2(&mut s, 1.0, 1.0);
        assert!((l - 1.0).abs() < 1e-6);
        assert!((r - 0.5).abs() < 1e-6);
    }

    /// Out-of-range balance saturates rather than amplifying: without the clamp,
    /// a balance of -2 would give the left channel a gain of 3.
    #[test]
    fn balance_saturates_out_of_range() {
        let mut s = strip(ChannelTopology::STEREO);
        set_balance(&s, -2.0);
        assert_eq!(tick2(&mut s, 1.0, 1.0), (1.0, 0.0));
        set_balance(&s, 2.0);
        assert_eq!(tick2(&mut s, 1.0, 1.0), (0.0, 1.0));
    }

    #[test]
    fn set_dispatches_each_unit_param() {
        let s = strip(ChannelTopology::STEREO);

        assert!(set_param(&s, UnitParam::Volume, 0.25));
        assert_eq!(s.volume().load().get(), 0.25);

        assert!(set_param(&s, UnitParam::Balance, -1.0));
        assert_eq!(s.balance().unwrap().load().get(), -1.0);
    }

    /// A strip refuses params it does not own, and they change nothing — the
    /// property that lets the generic reconciler push any param without
    /// dispatching on node type.
    #[test]
    fn ignores_params_it_does_not_own() {
        let mut s = strip(ChannelTopology::STEREO);
        assert!(!set_param(&s, UnitParam::Cutoff, 8000.0));
        assert!(!set_param(&s, UnitParam::Drive, 4.0));
        assert_eq!(tick2(&mut s, 1.0, 1.0), (1.0, 1.0));
    }

    /// The shape declares volume then balance on a bus with a pair, and never
    /// changes the arity.
    #[test]
    fn the_shape_declares_volume_then_balance() {
        let s = strip(width(6));
        assert_eq!(
            Node::<f32>::shape(&s).params.as_slice(),
            &[UnitParam::Volume, UnitParam::Balance][..]
        );
        assert_eq!(
            (
                Node::<f32>::shape(&s).audio_in().count(),
                Node::<f32>::shape(&s).audio_out().count()
            ),
            (6, 6)
        );
        s.volume().store(Amplitude::new(0.5));
        assert_eq!(
            Node::<f32>::param_base(&s, 0),
            Some(0.5),
            "volume's base is its control"
        );
        assert_eq!(
            Node::<f32>::param_base(&s, 1),
            Some(0.0),
            "balance's base is centre"
        );
    }

    /// A bus with nothing to balance declares no balance port and no balance
    /// param: its shape is the fader alone and `param_base` has no port 1.
    #[test]
    fn a_bus_without_a_pair_declares_volume_only() {
        for bus in pairless_buses() {
            let s = strip(bus);
            assert_eq!(
                Node::<f32>::shape(&s).params.as_slice(),
                &[UnitParam::Volume][..],
                "{bus:?}"
            );
            assert_eq!(Node::<f32>::param_base(&s, 0), Some(1.0));
            assert_eq!(
                Node::<f32>::param_base(&s, 1),
                None,
                "{bus:?}: no balance port"
            );
            assert!(s.balance().is_none(), "{bus:?}");
        }
    }

    /// A fed volume overrides the atomic per sample.
    #[test]
    fn a_fed_volume_overrides_the_atomic() {
        let mut s = strip(ChannelTopology::STEREO);
        s.volume().store(Amplitude::new(1.0));
        let mut out = [0.0f32; 2];
        crate::test_support::tick_fed(&mut s, &[1.0, 1.0], &[Some(0.5)], &mut out);
        assert_eq!(out, [0.5, 0.5]);
    }

    /// Starting or stopping a feed is not ramped by the strip, so the gain
    /// is never applied twice across the block: the fed value takes over
    /// from its first frame, where the graph's declick puts it at the base.
    #[test]
    fn starting_a_feed_does_not_apply_the_gain_twice() {
        let mut s = strip(ChannelTopology::STEREO);
        s.volume().store(Amplitude::new(0.5));
        let x = [1.0f32; 16];
        let steady = drive(&mut s, RATE, Planes::new(&[&x, &x]).unwrap(), &[None, None]);
        assert!(steady[0].iter().all(|&y| y == 0.5));
        let held = [0.5f32; 16];
        let fed = drive(
            &mut s,
            RATE,
            Planes::new(&[&x, &x]).unwrap(),
            &[Some(&held), None],
        );
        assert!(
            fed[0].iter().all(|&y| y == 0.5),
            "the fed volume alone, from its first frame: {:?}",
            fed[0]
        );
        let back = drive(&mut s, RATE, Planes::new(&[&x, &x]).unwrap(), &[None, None]);
        assert!(back[0].iter().all(|&y| y == 0.5), "and the control again");
    }

    /// A clone shares the cells: it is the template `param_parts` forks from,
    /// and a fork taken later must read the cells as they are then.
    #[test]
    fn clone_shares_atomics() {
        let original = strip(ChannelTopology::STEREO);
        let clone = original.clone();
        clone.volume().store(Amplitude::new(0.1));
        set_balance(&clone, 0.5);
        assert_eq!(original.volume().load().get(), 0.1);
        assert_eq!(original.balance().unwrap().load().get(), 0.5);
    }

    /// A channel with no mirror is faded but not balanced: the centre has no
    /// left/right axis to sit on.
    #[test]
    fn extra_channels_are_faded_but_not_balanced() {
        let mut s = strip(lcr());
        s.volume().store(Amplitude::new(0.5));
        set_balance(&s, -1.0);
        let mut out = [0.0f32; 3];
        tick(&mut s, &[1.0, 1.0, 1.0], &mut out);
        assert_eq!(out[0], 0.5); // near channel: unity balance × volume
        assert_eq!(out[1], 0.0); // far channel: balanced away
        assert_eq!(out[2], 0.5); // no balance applied, volume only
    }

    /// One block renders what the same frames do one at a time, while the
    /// controls hold: the ramp is the only thing block length changes.
    #[test]
    fn a_block_matches_its_frames() {
        let mut framed = strip(ChannelTopology::STEREO);
        framed.volume().store(Amplitude::new(0.6));
        set_balance(&framed, 0.25);
        let mut block = ParamNode::<f32>::fork_fresh(&framed);

        const N: usize = 8;
        let samples: [(f32, f32); N] = [
            (0.4, -0.8),
            (1.0, 1.0),
            (-0.5, 0.5),
            (0.0, 0.0),
            (0.25, 0.75),
            (-1.0, -1.0),
            (0.1, -0.1),
            (0.9, 0.3),
        ];

        // Per-frame reference.
        let mut ticked = [[0.0f32; 2]; N];
        for (i, &(l, r)) in samples.iter().enumerate() {
            tick(&mut framed, &[l, r], &mut ticked[i]);
        }

        // One block over the same input.
        let l: Vec<f32> = samples.iter().map(|s| s.0).collect();
        let r: Vec<f32> = samples.iter().map(|s| s.1).collect();
        let out = drive(&mut block, RATE, Planes::new(&[&l, &r]).unwrap(), &[]);
        for (i, frame) in ticked.iter().enumerate() {
            assert!((out[0][i] - frame[0]).abs() < 1e-6, "L @ {i}");
            assert!((out[1][i] - frame[1]).abs() < 1e-6, "R @ {i}");
        }
    }

    /// One `process` call over a constant (DC) stereo input, returned as frames.
    ///
    /// DC because it makes the output *be* the gain curve: any step in the gain
    /// is a step of the same size in the waveform.
    fn process_dc(strip: &mut BusStripNode, size: usize, l: f32, r: f32) -> Vec<[f32; 2]> {
        let (lv, rv) = (vec![l; size], vec![r; size]);
        let out = drive(strip, RATE, Planes::new(&[&lv, &rv]).unwrap(), &[]);
        (0..size).map(|i| [out[0][i], out[1][i]]).collect()
    }

    /// A fader moved from 1 to 1/4 ramps a DC block along `1 - (3/4)(i + 1)/N`, landing on 1/4
    /// exactly at the last frame, at both widths. Each sample is `from (1 - t) + to t` with the
    /// fraction `t`, both products and the sum rounded once, so it is off the line by at most
    /// four half-ulps of the larger endpoint: `bound`, `4 * 2^-24` at `f32` and `4 * 2^-53` at
    /// `f64`.
    fn ramp_is_the_line_in<F: Float + core::fmt::Debug>(bound: f64) {
        const N: usize = 1000;
        let mut s = strip(ChannelTopology::MONO);
        let ones = vec![F::ONE; N];
        let steady: Vec<Vec<F>> = drive(&mut s, RATE, Planes::new(&[&ones]).unwrap(), &[]);
        assert!(steady[0].iter().all(|&v| v == F::ONE));
        s.volume().store(Amplitude::new(0.25));
        let out = drive(&mut s, RATE, Planes::new(&[&ones]).unwrap(), &[]);
        let mut worst = 0.0f64;
        for (i, &y) in out[0].iter().enumerate() {
            let line = 1.0 - 0.75 * (i + 1) as f64 / N as f64;
            worst = worst.max((Into::<f64>::into(y) - line).abs());
        }
        assert!(worst < bound, "worst {worst:e} vs bound {bound:e}");
        assert_eq!(out[0][N - 1], F::lit(0.25), "the ramp lands on the target");
    }

    #[test]
    fn the_f32_ramp_is_the_line_to_four_half_ulps_and_lands_exactly() {
        ramp_is_the_line_in::<f32>(4.0 * 2f64.powi(-24));
    }

    #[test]
    fn the_f64_ramp_is_the_line_to_four_half_ulps_and_lands_exactly() {
        ramp_is_the_line_in::<f64>(4.0 * 2f64.powi(-53));
    }

    /// Largest sample-to-sample jump across `frames`, on either channel.
    fn largest_step(frames: &[[f32; 2]]) -> f32 {
        frames
            .windows(2)
            .flat_map(|w| [(w[1][0] - w[0][0]).abs(), (w[1][1] - w[0][1]).abs()])
            .fold(0.0, f32::max)
    }

    /// A fader pulled to zero ramps to silence across one block instead of
    /// stepping to it, and is silent from the end of that block on; pushed
    /// back up it ramps the same way.
    ///
    /// A fader that stepped would make the move on a full-scale signal a
    /// full-scale step — a click.
    #[test]
    fn a_fader_pulled_to_zero_ramps_instead_of_clicking() {
        const BLOCK: usize = 64;
        // A linear ramp from 1 to 0 over 64 frames moves 1/64 per frame. The
        // bound leaves room for rounding and nothing near a real step.
        const MAX_STEP: f32 = 1.5 / BLOCK as f32;

        let mut s = strip(ChannelTopology::STEREO);
        let steady = process_dc(&mut s, BLOCK, 1.0, 1.0);
        assert!(steady.iter().all(|f| *f == [1.0, 1.0]));

        s.volume().store(Amplitude::new(0.0));
        let fading = process_dc(&mut s, BLOCK, 1.0, 1.0);
        let mut seam = vec![*steady.last().unwrap()];
        seam.extend_from_slice(&fading);
        assert!(
            largest_step(&seam) <= MAX_STEP,
            "the fader must ramp, not step: largest jump {}",
            largest_step(&seam)
        );
        assert_eq!(
            *fading.last().unwrap(),
            [0.0, 0.0],
            "the ramp ends on silence at the block's last frame, not near it"
        );
        let silent = process_dc(&mut s, BLOCK, 1.0, 1.0);
        assert!(
            silent.iter().all(|f| *f == [0.0, 0.0]),
            "silent from the next block on"
        );

        s.volume().store(Amplitude::new(1.0));
        let rising = process_dc(&mut s, BLOCK, 1.0, 1.0);
        let mut seam = vec![*silent.last().unwrap()];
        seam.extend_from_slice(&rising);
        assert!(
            largest_step(&seam) <= MAX_STEP,
            "the fader back up must ramp too: largest jump {}",
            largest_step(&seam)
        );
        assert_eq!(*rising.last().unwrap(), [1.0, 1.0]);
    }

    /// A fader move is reached by a linear ramp across one block, starting from
    /// where the last block ended — not by a step, and not per sample from the
    /// atomic.
    #[test]
    fn volume_change_ramps_across_one_block() {
        const BLOCK: usize = 32;
        let mut s = strip(ChannelTopology::STEREO);
        let _ = process_dc(&mut s, BLOCK, 1.0, 1.0);

        s.volume().store(Amplitude::new(0.5));
        let ramp = process_dc(&mut s, BLOCK, 1.0, 1.0);
        let first = ramp[0][0];
        assert!(
            (first - (1.0 - 0.5 / BLOCK as f32)).abs() < 1e-6,
            "the first frame moves one step from the old level, got {first}"
        );
        for pair in ramp.windows(2) {
            assert!(pair[1][0] < pair[0][0], "monotone ramp down");
            assert_eq!(pair[1][0], pair[1][1], "both channels ramp together");
        }
        assert_eq!(ramp[BLOCK - 1], [0.5, 0.5], "lands exactly on the target");

        let settled = process_dc(&mut s, BLOCK, 1.0, 1.0);
        assert!(settled.iter().all(|f| *f == [0.5, 0.5]));
    }

    /// The first block after construction — or after `reset` — has no previous
    /// output to be continuous with, so it starts *at* its target. A strip built
    /// at zero must not fade in from unity for a block before going silent.
    #[test]
    fn a_fresh_or_reset_strip_starts_at_its_target() {
        let mut s = strip(ChannelTopology::STEREO);
        s.volume().store(Amplitude::new(0.0));
        assert!(
            process_dc(&mut s, 16, 1.0, 1.0)
                .iter()
                .all(|f| *f == [0.0, 0.0])
        );

        Node::<f32>::reset(&mut s);
        s.volume().store(Amplitude::new(0.25));
        assert!(
            process_dc(&mut s, 16, 1.0, 1.0)
                .iter()
                .all(|f| *f == [0.25, 0.25])
        );
    }

    /// Every channel, the ones past the balanced pair included, follows the
    /// linear ramp `from·(1−t) + to·t` at `t = (i+1)/n`, at a block length
    /// no SIMD width divides (so a vectorised loop's tail is covered), and
    /// lands exactly on the target.
    #[test]
    fn every_channel_ramps_linearly_past_the_simd_width() {
        const N: usize = 67;
        let mut s = strip(lcr());
        set_balance(&s, -0.5);
        let ones = [1.0f32; N];
        let _ = drive(
            &mut s,
            RATE,
            Planes::new(&[&ones, &ones, &ones]).unwrap(),
            &[],
        );
        s.volume().store(Amplitude::new(0.5));
        let out = drive(
            &mut s,
            RATE,
            Planes::new(&[&ones, &ones, &ones]).unwrap(),
            &[],
        );
        // Balance at -0.5: left takes the fader, right half of it, the rest
        // the fader.
        let (from, to) = ([1.0f32, 0.5, 1.0], [0.5f32, 0.25, 0.5]);
        for (c, y) in out.iter().enumerate() {
            for (i, &y) in y.iter().enumerate() {
                let t = (i + 1) as f32 / N as f32;
                let want = from[c] + (to[c] - from[c]) * t;
                assert!(
                    (y - want).abs() < 1e-6,
                    "channel {c} frame {i}: {y} against {want}"
                );
            }
            assert_eq!(y[N - 1], to[c], "channel {c} lands on the target");
        }
    }

    /// A fader ramp under a fed balance is the product of the two:
    /// `ramp(volume)[i] · balance(fed[i])` on each channel, frame by frame.
    #[test]
    fn a_fader_ramp_under_a_fed_pan_is_their_product() {
        const N: usize = 37;
        let mut s = strip(lcr());
        let ones = [1.0f32; N];
        let fed_balance = [0.5f32; N];
        let fed = [None, Some(&fed_balance[..])];
        let _ = drive(
            &mut s,
            RATE,
            Planes::new(&[&ones, &ones, &ones]).unwrap(),
            &fed,
        );
        s.volume().store(Amplitude::new(0.5));
        let out = drive(
            &mut s,
            RATE,
            Planes::new(&[&ones, &ones, &ones]).unwrap(),
            &fed,
        );
        // Balance at 0.5: left halved, right at unity, the rest unbalanced.
        let balance = [0.5f32, 1.0, 1.0];
        for (c, (y, &bal)) in out.iter().zip(&balance).enumerate() {
            for (i, &y) in y.iter().enumerate() {
                let t = (i + 1) as f32 / N as f32;
                let want = (1.0 - 0.5 * t) * bal;
                assert!(
                    (y - want).abs() < 1e-6,
                    "channel {c} frame {i}: {y} against {want}"
                );
            }
            assert_eq!(y[N - 1], 0.5 * bal, "channel {c} lands on the target");
        }
    }

    /// One frame of ones through a strip on `topology` at `balance` and
    /// `volume`, read back by the speaker each channel feeds.
    fn gain_by_speaker(topology: ChannelTopology, balance: f64, volume: f64) -> Vec<(Speaker, f32)> {
        let mut s = strip(topology);
        s.volume().store(Amplitude::new(volume));
        set_balance(&s, balance);
        let n = topology.width().count() as usize;
        let mut out = vec![0.0f32; n];
        tick(&mut s, &vec![1.0f32; n], &mut out);
        topology
            .positions()
            .expect("a placed bus")
            .iter()
            .copied()
            .zip(out)
            .collect()
    }

    fn of(gains: &[(Speaker, f32)], speaker: Speaker) -> f32 {
        gains
            .iter()
            .find(|(s, _)| *s == speaker)
            .expect("the speaker is on the bus")
            .1
    }

    /// On a stereo strip the output is the input times the linear balance law
    /// times the fader, exactly, at the ends, the middle and between.
    #[test]
    fn a_stereo_strip_is_input_times_balance_law_times_volume() {
        for balance in [-1.0f64, -0.5, 0.0, 0.5, 1.0] {
            let (gl, gr) = Balance::new(balance).gains();
            let volume = 0.5f64;
            let mut s = strip(ChannelTopology::STEREO);
            s.volume().store(Amplitude::new(volume));
            set_balance(&s, balance);
            let (l, r) = tick2(&mut s, 0.75, -0.25);
            // The samples are `f32`: each gain narrows into the product.
            assert_eq!(l, 0.75 * (gl.get() * volume) as f32, "left at {balance}");
            assert_eq!(r, -0.25 * (gr.get() * volume) as f32, "right at {balance}");
        }
    }

    /// Balance +0.5 on a 5.1 strip halves FL and SL and leaves FR and SR at
    /// unity, while the centre and the LFE take the fader alone.
    #[test]
    fn five_one_balances_front_and_side_pairs_and_spares_centre_and_lfe() {
        let g = gain_by_speaker(ChannelTopology::from(Bed::Surround51), 0.5, 0.5);
        assert_eq!(of(&g, FrontLeft), 0.25);
        assert_eq!(of(&g, SideLeft), 0.25);
        assert_eq!(of(&g, FrontRight), 0.5);
        assert_eq!(of(&g, SideRight), 0.5);
        assert_eq!(of(&g, FrontCenter), 0.5);
        assert_eq!(of(&g, LowFrequency), 0.5);
    }

    /// The pairing follows the speaker a channel feeds, not its index: a 5.1
    /// bus in another order balances the same speakers.
    #[test]
    fn pairing_is_by_speaker_not_by_channel_index() {
        let reordered = ChannelTopology::new([
            SideRight,
            FrontCenter,
            FrontLeft,
            LowFrequency,
            FrontRight,
            SideLeft,
        ])
        .expect("six speakers fit a bus");
        let g = gain_by_speaker(reordered, 0.5, 0.5);
        assert_eq!(of(&g, FrontLeft), 0.25);
        assert_eq!(of(&g, SideLeft), 0.25);
        assert_eq!(of(&g, FrontRight), 0.5);
        assert_eq!(of(&g, SideRight), 0.5);
        assert_eq!(of(&g, FrontCenter), 0.5);
        assert_eq!(of(&g, LowFrequency), 0.5);
        // Channel 0 is a right speaker here, and channel 1 the centre.
        assert_eq!(g[0], (SideRight, 0.5));
        assert_eq!(g[1], (FrontCenter, 0.5));
    }

    /// Every pair of 7.1.4 takes the law: front, side, back, top front and
    /// top back.
    #[test]
    fn seven_one_four_balances_every_pair() {
        let g = gain_by_speaker(ChannelTopology::from(Bed::Surround714), -0.5, 1.0);
        for left in [FrontLeft, SideLeft, BackLeft, TopFrontLeft, TopBackLeft] {
            assert_eq!(of(&g, left), 1.0, "{left:?} is the near side");
        }
        for right in [
            FrontRight,
            SideRight,
            BackRight,
            TopFrontRight,
            TopBackRight,
        ] {
            assert_eq!(of(&g, right), 0.5, "{right:?} is the far side");
        }
        assert_eq!(of(&g, FrontCenter), 1.0);
        assert_eq!(of(&g, LowFrequency), 1.0);
    }

    /// Buses with no mirrored pair: mono, unassigned, ambisonic, a lone `FL`.
    fn pairless_buses() -> [ChannelTopology; 4] {
        [
            ChannelTopology::MONO,
            ChannelTopology::unassigned(ChannelLayout::from(6u16)).expect("six fit a bus"),
            ChannelTopology::Ambisonic(AmbisonicFormat::ambix(AmbisonicOrder::of(1))),
            ChannelTopology::new([FrontLeft, FrontCenter]).expect("two speakers fit a bus"),
        ]
    }

    /// Setting a balance on a bus with no pair is refused by address, changes
    /// nothing, and the strip stays a fader.
    #[test]
    fn a_bus_without_a_pair_refuses_the_balance() {
        for bus in pairless_buses() {
            let n = bus.width().count() as usize;
            let mut s = strip(bus);
            s.volume().store(Amplitude::new(0.5));
            assert!(
                !set_param(&s, UnitParam::Balance, -1.0),
                "{bus:?} refuses the param"
            );
            assert!(
                set_param(&s, UnitParam::Volume, 0.5),
                "{bus:?} keeps the fader"
            );
            let mut out = vec![0.0f32; n];
            tick(&mut s, &vec![1.0f32; n], &mut out);
            assert!(out.iter().all(|&y| y == 0.5), "{bus:?}: {out:?}");
        }
    }

    /// A quad spelled with sides and one spelled with backs are both a
    /// left/right pair of pairs: the counterpart spelling balances too.
    #[test]
    fn a_quad_is_balanced_whichever_surround_pair_it_names() {
        for rear in [[SideLeft, SideRight], [BackLeft, BackRight]] {
            let quad = ChannelTopology::new([FrontLeft, FrontRight, rear[0], rear[1]])
                .expect("four speakers fit a bus");
            let g = gain_by_speaker(quad, 0.5, 1.0);
            assert_eq!(of(&g, FrontLeft), 0.5, "{rear:?}");
            assert_eq!(of(&g, rear[0]), 0.5, "{rear:?}");
            assert_eq!(of(&g, FrontRight), 1.0, "{rear:?}");
            assert_eq!(of(&g, rear[1]), 1.0, "{rear:?}");
        }
    }

    /// Front-of-centre and top-side pairs take the law too: a 7.1-wide bus
    /// (`FL FR FC LFE BL BR FLC FRC`) and a bus with the top-side pair.
    #[test]
    fn front_of_centre_and_top_side_pairs_are_balanced() {
        let wide = ChannelTopology::new([
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            FrontLeftOfCenter,
            FrontRightOfCenter,
        ])
        .expect("eight speakers fit a bus");
        let g = gain_by_speaker(wide, 0.5, 1.0);
        for left in [FrontLeft, BackLeft, FrontLeftOfCenter] {
            assert_eq!(of(&g, left), 0.5, "{left:?}");
        }
        for right in [FrontRight, BackRight, FrontRightOfCenter] {
            assert_eq!(of(&g, right), 1.0, "{right:?}");
        }
        assert_eq!(of(&g, FrontCenter), 1.0);
        assert_eq!(of(&g, LowFrequency), 1.0);

        let tops = ChannelTopology::new([TopSideLeft, TopSideRight]).expect("two fit a bus");
        let g = gain_by_speaker(tops, -0.5, 1.0);
        assert_eq!(of(&g, TopSideLeft), 1.0);
        assert_eq!(of(&g, TopSideRight), 0.5);
    }

    /// A balance move ramps a side-pair channel linearly and lands exactly on
    /// its target: `from·(1-t) + to·t` at `t = (i+1)/n`, with the far side
    /// of a 5.1 strip going from unity to `1 + b` of the fader.
    #[test]
    fn a_balance_move_ramps_a_side_pair_linearly() {
        const N: usize = 67;
        let mut s = strip(ChannelTopology::from(Bed::Surround51));
        let ones = [1.0f32; N];
        let ins: Vec<&[f32]> = vec![&ones; 6];
        let _ = drive(&mut s, RATE, Planes::new(&ins).unwrap(), &[]);
        set_balance(&s, 0.5);
        let out = drive(&mut s, RATE, Planes::new(&ins).unwrap(), &[]);
        // 5.1 order: FL FR FC LFE SL SR. The left sides fall from 1 to 0.5.
        for c in [0usize, 4] {
            for (i, &y) in out[c].iter().enumerate() {
                let t = (i + 1) as f32 / N as f32;
                let want = 1.0 * (1.0 - t) + 0.5 * t;
                assert!(
                    (y - want).abs() < 1e-6,
                    "channel {c} frame {i}: {y} against {want}"
                );
            }
            assert_eq!(out[c][N - 1], 0.5, "channel {c} lands on the target");
        }
        for c in [1usize, 2, 3, 5] {
            assert!(out[c].iter().all(|&y| y == 1.0), "channel {c} holds unity");
        }
    }
}
