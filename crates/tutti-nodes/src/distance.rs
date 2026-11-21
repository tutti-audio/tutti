//! [`DistanceNode`]: a source at a distance, as the delay `r / c`, the level `r_ref / r` and
//! optional air absorption.

use core::num::NonZeroU16;
use tutti_buffers::{BusTopology, ChannelLayout, Float, MaxBlock, Planes, PlanesMut};
use tutti_dsp::{DISTANCE_GLIDE, Distance, DistanceControls, DistanceSpec};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, Metres, SampleRate, Samples, Speed, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`DistanceNode`] is made from: its width, the farthest distance it can place a
/// source at, and where its distance starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DistanceConfig {
    width: NonZeroU16,
    spec: DistanceSpec,
    controls: DistanceControls,
}

impl DistanceConfig {
    /// A distance node of `width` channels for sources up
    /// to `max` away (it sets the delay line's length), at the default distance of 1 m, with a
    /// reference of 1 m, the speed of sound in air and no air absorption.
    #[must_use]
    pub fn new(width: NonZeroU16, max: Metres) -> Self {
        Self {
            width,
            spec: DistanceSpec::new(max),
            controls: DistanceControls::DEFAULT,
        }
    }

    /// The distance at which the level is 1 (at least a millimetre): a source nearer is not
    /// louder.
    #[must_use]
    pub fn reference(self, reference: Metres) -> Self {
        Self {
            spec: self.spec.reference(reference),
            ..self
        }
    }

    /// The speed of the wave the delay is computed from.
    #[must_use]
    pub fn speed(self, speed: Speed) -> Self {
        Self {
            spec: self.spec.speed(speed),
            ..self
        }
    }

    /// Air absorption: a low-pass whose cutoff is `cutoff` at the reference distance and
    /// falls as the reference over the distance.
    #[must_use]
    pub fn air_absorption(self, cutoff: Hz) -> Self {
        Self {
            spec: self.spec.air_absorption(cutoff),
            ..self
        }
    }

    /// The distance it starts at.
    #[must_use]
    pub fn distance(mut self, distance: Metres) -> Self {
        self.controls.distance = distance;
        self
    }
}

/// A source at a distance, of any width: every channel is delayed by `r / c`, scaled by
/// `r_ref / r` (held at 1 within the reference) and, if configured, low-passed by the air
/// ([`Distance`]).
///
/// The distance (`UnitParam::Distance`, a [`Metres`]) is the path length of the sound that
/// arrives now. The delay glides to a new distance in at least [`DISTANCE_GLIDE`] frames and
/// at most `DELAY_SLEW` frames of delay a frame, so a jump never clicks (the first distance
/// after a prepare or a reset lands at once, with nothing in the line to bend), and a distance that
/// moves is a Doppler shift: it grows at `u` metres a second, a sine of `f` comes out at
/// `f (1 - u / c)`. The air's cutoff is `fc_ref r_ref / r`.
///
/// No latency: the delay is musical, not processing. It rings for the longest delay in
/// flight and the air's ring-out. A [`KernelNode`]. Made from a [`DistanceConfig`]
/// (`DistanceNode::from`).
///
/// # Panics
///
/// `DistanceNode::from` panics past 16 channels, where the node's shape is built: a distance
/// is one bus.
pub type DistanceNode<F> = KernelNode<DistanceKernel<F>>;

impl<F: Float> From<DistanceConfig> for DistanceNode<F> {
    fn from(config: DistanceConfig) -> Self {
        Self::with_kernel(
            DistanceKernel::new(
                ChannelLayout::from_count(config.width.get()),
                config.spec,
                SampleRate::SR_48K,
                config.controls,
            ),
            DistanceSet,
            config.controls,
        )
    }
}

/// The distance's control: how far the source is.
#[derive(Clone, Copy, Debug)]
pub struct DistanceSet;

control_set! {
    DistanceSet(_s): DistanceControls = DistanceControls::DEFAULT;
    live: (all);
    Distance => Metres = distance { whole };

    fn defaults(&self) -> DistanceControls {
        DistanceControls::DEFAULT
    }
}

/// The node's [`Kernel`]: one [`Distance`] over the width.
#[derive(Clone, Debug)]
pub struct DistanceKernel<F: Float> {
    width: ChannelLayout,
    spec: DistanceSpec,
    /// The controls last retargeted to, which `prepare` rebuilds from.
    controls: DistanceControls,
    /// Whether the next retarget is the first since the kernel was built or reset: nothing
    /// is sounding then, so the delay lands on the distance at once.
    fresh: bool,
    distance: Distance<F>,
}

impl<F: Float> DistanceKernel<F> {
    /// A distance of `width` channels for `spec` at `rate`, at rest on `controls`.
    fn new(
        width: ChannelLayout,
        spec: DistanceSpec,
        rate: SampleRate,
        controls: DistanceControls,
    ) -> Self {
        Self {
            width,
            spec,
            controls,
            fresh: true,
            distance: Distance::new(spec, width, rate, &controls),
        }
    }
}

impl<F: Float> Kernel for DistanceKernel<F> {
    type Float = F;
    type Set = DistanceSet;

    /// The delay glides over at least this many frames, so a design is as fine as the glide.
    const GRID: usize = DISTANCE_GLIDE.get();

    /// `width` channels in and out, `Moderate`: a delay read per sample and channel.
    fn shape(&self) -> Shape {
        let bus = BusTopology::unassigned(self.width)
            .expect("a distance node is at most 16 channels wide");
        Shape::through(bus).with_cost(Cost::Moderate)
    }

    /// Builds the delay lines at `rate`: the one allocation, here and not in `render`.
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        *self = Self::new(self.width, self.spec, rate, self.controls);
    }

    fn reset(&mut self) {
        self.fresh = true;
        self.distance.reset();
    }

    /// The delay glides to the distance, except the first one after a build or a reset,
    /// which it lands on: no sound is in the line to bend.
    fn retarget(&mut self, controls: &DistanceControls) {
        self.controls = *controls;
        self.distance.retarget(controls);
        if self.fresh {
            self.distance.land();
            self.fresh = false;
        }
    }

    fn render(
        &mut self,
        input: Planes<'_, F>,
        _: ControlSignals<'_, F>,
        output: PlanesMut<'_, '_, F>,
    ) {
        self.distance.process(input, output);
    }

    /// No latency: the delay is the source's distance, not the node's processing.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// The tail at the farthest distance: the longest over every control.
    fn tail(&self) -> Tail {
        Tail::Finite(self.distance.tail_ceiling())
    }

    fn tail_of(&mut self, controls: &DistanceControls) -> Tail {
        Tail::Finite(self.distance.tail(controls))
    }
}

#[cfg(test)]
mod tests;
