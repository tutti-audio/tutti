//! [`PitchShifterNode`]: a pitch shift in cents over a phase vocoder, at the input's duration.

use tutti_buffers::{BusTopology, ChannelLayout, MaxBlock, Planes, PlanesMut};
use tutti_dsp::{
    ColaGeometry, FftFloat, HopSize, PhaseVocoder, PhaseVocoderControls, StftGeometry, WindowLength,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Cents, SampleRate, Samples, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`PitchShifterNode`] is made from: its width, its geometry and its starting shift.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PitchShifterConfig {
    width: ChannelLayout,
    hop: HopSize,
    controls: PhaseVocoderControls,
}

impl PitchShifterConfig {
    /// The hop a pitch shifter starts at: 512 frames, a window of 2048.
    pub const DEFAULT_HOP: HopSize = HopSize::of(Samples::new(512));

    /// A pitch shifter of `width` channels (an empty layout is taken as mono) at no shift, on a
    /// hop of [`DEFAULT_HOP`](Self::DEFAULT_HOP).
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: ChannelLayout::from_count(width.into().count().max(1)),
            hop: Self::DEFAULT_HOP,
            controls: PhaseVocoderControls::UNITY,
        }
    }

    /// The shift it starts at: up when positive, down when negative, within two octaves either
    /// way (a farther one is held there).
    #[must_use]
    pub fn pitch(mut self, pitch: Cents) -> Self {
        self.controls.pitch = pitch;
        self
    }

    /// The frames the analysis steps by: the window is four of them, a Hann window at the
    /// overlap that sums to unity. A longer hop resolves lower pitches and smears attacks, and
    /// is more latency: the window (four hops) plus a lead of about four more, so a hop of 256
    /// frames is 43 ms at 48 kHz, 512 (the default) 86 ms, and 1024 172 ms.
    #[must_use]
    pub fn hop(mut self, hop: HopSize) -> Self {
        self.hop = hop;
        self
    }
}

/// A pitch shifter of any width: every frequency moves by the same number of cents and the
/// duration stays ([`PhaseVocoder`]).
///
/// One vocoder serves every channel, so the channels share one phase solution: a stereo image
/// survives the shift, and equal channels stay equal bit for bit. A sine at `f` comes out at
/// `f * 2^(cents / 1200)`. The shift (`UnitParam::Pitch`, a [`Cents`]) is read once per block
/// and applies at the next hop with no flush, so a change leaves no hole.
///
/// # Latency
///
/// The output is the input delayed by the latency of a fixed-rate vocoder
/// ([`PhaseVocoder::fixed_rate`]): the window, a frame, and the lead that makes `n` frames in
/// always `n` frames out. That is the node's processing latency, so a path beside it is
/// compensated by exactly the delay it has, and the graph's `NodeMix` blends the shifted
/// signal with the aligned input.
///
/// It rings for the frames of a window read at the slowest rate of a shift down.
///
/// A [`KernelNode`]. Made from a [`PitchShifterConfig`]
/// (`PitchShifterNode::from`).
///
/// # Panics
///
/// `PitchShifterNode::from` panics past 16 channels, where the node's shape is built: a pitch
/// shifter is one bus.
pub type PitchShifterNode<F> = KernelNode<PitchShifterKernel<F>>;

impl<F: FftFloat> From<PitchShifterConfig> for PitchShifterNode<F> {
    fn from(config: PitchShifterConfig) -> Self {
        Self::with_kernel(
            PitchShifterKernel::new(
                config.width,
                config.hop,
                SampleRate::SR_48K,
                config.controls,
            ),
            PitchShifterSet,
            config.controls,
        )
    }
}

/// The shifter's control: the shift.
#[derive(Clone, Copy, Debug)]
pub struct PitchShifterSet;

control_set! {
    PitchShifterSet(_s): PhaseVocoderControls = PhaseVocoderControls::UNITY;
    live: (all);
    Pitch => Cents = pitch { whole };

    fn defaults(&self) -> PhaseVocoderControls {
        PhaseVocoderControls::UNITY
    }
}

/// The shifter as a [`Kernel`]: one fixed-rate [`PhaseVocoder`] over the width.
#[derive(Debug)]
pub struct PitchShifterKernel<F: FftFloat> {
    width: ChannelLayout,
    hop: HopSize,
    rate: SampleRate,
    /// The controls the vocoder was last retargeted to, which a fork starts from.
    controls: PhaseVocoderControls,
    vocoder: PhaseVocoder<F>,
}

impl<F: FftFloat> PitchShifterKernel<F> {
    /// A shifter of `width` channels on a window of four `hop`s at `rate`, retargeted to
    /// `controls`.
    fn new(
        width: ChannelLayout,
        hop: HopSize,
        rate: SampleRate,
        controls: PhaseVocoderControls,
    ) -> Self {
        let geometry = StftGeometry::new(rate, Self::window(hop), hop);
        let geometry = ColaGeometry::try_from(geometry)
            .expect("a window of four hops is a Hann window at the overlap that sums to unity");
        let mut kernel = Self {
            width,
            hop,
            rate,
            controls,
            vocoder: PhaseVocoder::fixed_rate(width, geometry),
        };
        kernel.vocoder.retarget(&controls);
        kernel
    }

    /// The window of four `hop`s.
    fn window(hop: HopSize) -> WindowLength {
        WindowLength::new(Samples::new(4 * hop.get().get()))
            .expect("four hops of at least one frame is a window")
    }
}

/// A fork is a shifter of its own: a new vocoder at the controls last retargeted, silent.
impl<F: FftFloat> Clone for PitchShifterKernel<F> {
    fn clone(&self) -> Self {
        Self::new(self.width, self.hop, self.rate, self.controls)
    }
}

impl<F: FftFloat> Kernel for PitchShifterKernel<F> {
    type Float = F;
    type Set = PitchShifterSet;

    /// The shift applies at a hop, so a design is as fine as the stream's smallest block grid.
    const GRID: usize = 64;

    /// `width` channels in and out, `Heavy`: an FFT pair per channel per hop.
    fn shape(&self) -> Shape {
        let bus = BusTopology::unassigned(self.width)
            .expect("a pitch shifter is at most 16 channels wide");
        Shape::through(bus).with_cost(Cost::Heavy)
    }

    /// Builds the vocoder at `rate`: the one allocation, here and not in `render`.
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        *self = Self::new(self.width, self.hop, rate, self.controls);
    }

    fn reset(&mut self) {
        self.vocoder.reset();
    }

    fn retarget(&mut self, controls: &PhaseVocoderControls) {
        self.controls = *controls;
        self.vocoder.retarget(controls);
    }

    /// Runs the vocoder over the block: `n` frames in give `n` frames out.
    fn render(
        &mut self,
        input: Planes<'_, F>,
        _: ControlSignals<'_, F>,
        output: PlanesMut<'_, '_, F>,
    ) {
        let _ = self.vocoder.process(input, output);
    }

    fn latency(&self) -> Samples {
        self.vocoder.latency()
    }

    /// Past the delay, a window read at the slowest rate: a frame's window is four hops of the
    /// stretched time and a shift down two octaves reads it at a quarter of the speed, which is
    /// four windows; one window more is the margin.
    fn tail(&self) -> Tail {
        Tail::Finite(Samples::new(5 * 4 * self.hop.get().get()))
    }
}

#[cfg(test)]
mod tests;
