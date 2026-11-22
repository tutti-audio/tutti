//! [`HrtfBinauralNode`] — the binaural graph node, 2 in / 2 out.
//!
//! Construction needs a measured HRIR sphere, so [`HrtfBinauralConfig::new`]
//! takes the parsed dataset.

use std::sync::Arc;
use tutti_buffers::{BusTopology, MonoMut, StereoPlanes};
use tutti_dsp::{FftFloat, HRTF_LATENCY, HRTF_STEP, HrirSphere, HrtfBinaural};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_rt::Param;
use tutti_units::Latency;
use tutti_units::UnitParam;
use tutti_units::{Azimuth, Direction, Elevation, Mix, SampleRate, Samples, Tail};

use crate::last_good::LastGood;
use crate::spatial::target::AngleSmoother;

// The params an [`HrtfBinauralNode`] lets the graph modulate, in port order.
tutti_graph::declare_params!([F: FftFloat] HrtfBinauralNode<F>, n {
    Azimuth => Azimuth = &n.azimuth,
    Elevation => Elevation = &n.elevation,
    Wet => Mix = &n.blend,
});

/// A binaural panner for headphones: FFT convolution against a measured HRIR
/// sphere, stereo in, stereo out.
///
/// Build it from an [`HrtfBinauralConfig`] over the parsed dataset. The input pair is
/// folded to mono, placed at the commanded bearing and height, and blended
/// against the (equally delayed) dry mono by the blend ([`UnitParam::Wet`]:
/// 1.0, the default, is full HRTF, 0.0 the dry mono).
/// It is a graph node (`tutti_graph::Node` and [`ParamNode`]); inserting it
/// hands back its [`ParamSet`] ([`UnitParam::Azimuth`],
/// [`Elevation`](tutti_units::UnitParam::Elevation) and [`Wet`](tutti_units::UnitParam::Wet)), which
/// move the source lock-free while it renders, and a graph's control lanes
/// may drive all three.
/// Position changes are de-zippered over 50 ms, and each step of the ramp
/// crossfades between the filters on either side of it. The filter for a
/// direction is blended across the sphere face that direction crosses. A
/// non-finite bearing or height (NaN, ±∞) is ignored: the source stays at the
/// last finite one, and the next finite position moves it as usual.
///
/// Output lags input by 128 samples (one convolution step), on both the wet
/// and the dry half. The node declares this as latency, so the graph's delay
/// compensation lines the other paths up with it. Rendering a block
/// allocates nothing, takes no lock and cannot panic on any sphere
/// [`HrirSphere::from_bytes`] accepted; preparing at a new rate resamples the dataset and allocates.
#[derive(Debug)]
pub struct HrtfBinauralNode<F: FftFloat> {
    /// The sphere the renderer is prepared from.
    sphere: Arc<HrirSphere>,
    /// The renderer, built at the graph's rate by `prepare`; until then the node is silent.
    panner: Option<HrtfBinaural<F>>,
    /// The de-zipper, stepped once per convolution step: the node's, the
    /// convolver renders the direction it returns.
    aim: AngleSmoother,
    /// Bearing, wrapped onto the circle when read.
    azimuth: Param<Azimuth>,
    /// Height, clamped to the poles when read.
    elevation: Param<Elevation>,
    /// The blend between the HRTF-rendered signal and the dry mono centre,
    /// `0..1`. Not the VBAP panner's width, a mid/side
    /// [`StereoWidth`](tutti_units::StereoWidth).
    blend: Param<Mix>,
    /// The last finite azimuth and elevation the cells held. The de-zipper
    /// smoother is recursive, so one NaN or ±∞ coordinate would leave it NaN
    /// for good; a non-finite write reads as unchanged instead.
    good: Held,
}

/// The last finite value each control cell held.
#[derive(Clone, Copy, Debug)]
struct Held {
    azimuth: LastGood<f64>,
    elevation: LastGood<f64>,
    blend: LastGood<f64>,
}

impl<F: FftFloat> Clone for HrtfBinauralNode<F> {
    /// Shares the controls (a clone is the fork's template, which must read
    /// the cells as they are when the fork is taken) and the parsed sphere;
    /// the renderer gets its own, cleared, streaming state.
    fn clone(&self) -> Self {
        Self {
            sphere: self.sphere.clone(),
            panner: self.panner.as_ref().map(|p| p.fresh(self.aim.current())),
            aim: self.aim.clone(),
            azimuth: self.azimuth.clone(),
            elevation: self.elevation.clone(),
            blend: self.blend.clone(),
            good: self.good,
        }
    }
}

/// What an [`HrtfBinauralNode`] is made from: the parsed HRIR sphere; [`From`] makes the node,
/// aimed straight ahead at full HRTF blend.
///
/// The sphere is parsed once ([`HrirSphere::from_bytes`], which checks every index, length
/// and value, so nothing malformed reaches the audio thread) and shared: clones and forks
/// read the one sphere. The node resamples it to the graph's rate in
/// [`prepare`](Node::prepare), held to at most
/// [`MAX_PREPARED_RATE`](tutti_dsp::MAX_PREPARED_RATE), and renders silence until then.
#[derive(Clone, Debug)]
pub struct HrtfBinauralConfig {
    sphere: Arc<HrirSphere>,
}

impl HrtfBinauralConfig {
    /// A renderer over `sphere` (for example an embedded IRCAM `.bin`, in the format
    /// `hrir_sphere_builder` writes).
    #[must_use]
    pub fn new(sphere: Arc<HrirSphere>) -> Self {
        Self { sphere }
    }
}

impl<F: FftFloat> From<HrtfBinauralConfig> for HrtfBinauralNode<F> {
    fn from(config: HrtfBinauralConfig) -> Self {
        Self {
            sphere: config.sphere,
            panner: None,
            // The graph's rate arrives in `prepare`, which re-seats the smoother on it.
            aim: AngleSmoother::new(SampleRate::SR_48K, Samples::new(HRTF_STEP)),
            azimuth: Param::new(Azimuth::FRONT),
            elevation: Param::new(Elevation::LEVEL),
            blend: Param::new(Mix::WET),
            good: Held {
                azimuth: LastGood::fixed(Azimuth::FRONT.get()),
                elevation: LastGood::fixed(Elevation::LEVEL.get()),
                blend: LastGood::fixed(Mix::WET.get()),
            },
        }
    }
}

impl<F: FftFloat> HrtfBinauralNode<F> {
    /// The position cells, read once (`driven` is the block's azimuth and
    /// elevation where a control signal drives them): a non-finite coordinate held at
    /// the last finite one, the bearing wrapped and the height clamped.
    #[inline]
    fn placement(&mut self, [azimuth, elevation]: [Option<F>; 2]) -> Direction {
        // A driven value is the graph's `F` control signal; the angles are `f64`, so
        // it widens here, once per block.
        let azimuth = self.good.azimuth.block(azimuth, self.azimuth.load().get());
        let elevation = self
            .good
            .elevation
            .block(elevation, self.elevation.load().get());
        Direction::new(
            Azimuth::new(azimuth).wrap(),
            Elevation::new_clamped(elevation),
        )
    }
}

/// A graph node: stereo in, stereo out.
///
/// # Latency and tail
///
/// The step's delay, 128 samples, is declared as the node's processing
/// latency, true of both outputs at every blend, so the graph's delay
/// compensation lines the rest of the mix up with it. The tail excludes that
/// delay: it is the response length less one, a fixed size at a rate, so it
/// is exact rather than an estimate.
///
/// # Reset clears time, not placement
///
/// [`reset`](Node::reset) clears the input history, the step in flight and
/// the de-zipper ramp. Position and blend are caller-set configuration and
/// survive, and the ramp is seated on the commanded position, so the first
/// block after a reset already renders there.
impl<F: FftFloat> Node<F> for HrtfBinauralNode<F> {
    fn shape(&self) -> Shape {
        Shape::through(BusTopology::STEREO)
            .param_addrs(self.declared_ports().as_slice())
            .latency(Latency::new(Samples::new(HRTF_LATENCY)))
            .tail(match self.panner.as_ref().map(HrtfBinaural::ring_out) {
                // The response length follows the graph's rate, known at `prepare`.
                None => Tail::Unbounded,
                Some(0) => Tail::None,
                Some(n) => Tail::Finite(Samples::new(n)),
            })
            // An exception to the nearest-class rule. The FFT work lands all at once in the
            // block that completes a 128-frame step: at 64-frame blocks that
            // block carries twice the mean, past the Heavy boundary for a
            // moving source with 512-tap HRIRs, and the schedule must price
            // the worst block, not the mean. Longer HRIRs cost more.
            .cost(tutti_graph::Cost::Heavy)
    }

    /// Resamples the sphere to the graph's rate (allocates; control thread).
    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate();
        let target = self.placement([None; 2]);
        self.aim.set_sample_rate(rate);
        let aim = &mut self.aim;
        let mut seat = || {
            aim.reset_to_target(target);
            aim.current()
        };
        match &mut self.panner {
            Some(panner) => panner.prepare(rate, seat),
            None => self.panner = Some(HrtfBinaural::new(self.sphere.clone(), rate, seat())),
        }
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        // The cells are the one copy of the commanded position and blend; the
        // renderer reads them here, once per block, a control signal that drives one
        // overriding it at the block's last frame (`ParamInput::last_frame`).
        let target = self.placement([
            self.declared_param::<{ UnitParam::Azimuth as u16 }, _>(&io)
                .last_frame(),
            self.declared_param::<{ UnitParam::Elevation as u16 }, _>(&io)
                .last_frame(),
        ]);
        // `Mix::new_clamped` passes a NaN through, so the blend is held too.
        let blend = Mix::new(
            self.good.blend.block(
                self.declared_param::<{ UnitParam::Wet as u16 }, _>(&io)
                    .last_frame(),
                self.blend.load().get(),
            ),
        );
        let size = io.frames();
        let (inputs, mut outputs) = io.split();
        let (left, right) = (&inputs.get(0)[..size], &inputs.get(1)[..size]);
        let Some(panner) = self.panner.as_mut() else {
            // Not prepared: no renderer yet, so silence.
            for out in outputs.iter_mut().take(2) {
                out[..size].fill(F::ZERO);
            }
            return Status::Modified;
        };
        let mut outs = outputs.iter_mut();
        // The shape declares two outputs, so both are always present.
        if let (Some(out_left), Some(out_right)) = (outs.next(), outs.next()) {
            let aim = &mut self.aim;
            if let Ok(input) = StereoPlanes::new(left, right) {
                panner.process(
                    blend,
                    input,
                    (
                        MonoMut::new(&mut out_left[..size]),
                        MonoMut::new(&mut out_right[..size]),
                    ),
                    || aim.step(target),
                );
            } else {
                // The two inputs are one length, so this is unreachable; silence, never stale.
                out_left[..size].fill(F::ZERO);
                out_right[..size].fill(F::ZERO);
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        let target = self.placement([None; 2]);
        self.aim.reset_to_target(target);
        if let Some(panner) = &mut self.panner {
            panner.reset_state(self.aim.current());
        }
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

impl<F: FftFloat> ParamNode<F> for HrtfBinauralNode<F> {
    /// The bearing, height and blend.
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// A clone (a rebuilt renderer) with its three cells detached at their
    /// values now, and reset.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.azimuth.detach();
        fork.elevation.detach();
        fork.blend.detach();
        Node::<F>::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl<F: FftFloat> IntoNode<F> for HrtfBinauralNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests;
