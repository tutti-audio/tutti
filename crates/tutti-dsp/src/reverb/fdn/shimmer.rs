//! [`Shimmer`]: a pitch shift inside the feedback of a feedback delay network.
//!
//! The lines' feedback `f = A G d` (the mix `A` of the network, the absorption `G`, the lines'
//! outputs `d`) is written back into the lines. The shimmer takes the unit vector `u` of equal
//! entries, `u_j = 1/sqrt(n)`, and replaces the share `k` of the feedback's component along
//! it, `s = u . f`, with that component shifted:
//!
//! ```text
//! f'  =  f  +  k u (q / B  -  s)        q = delay(P(s))
//! ```
//!
//! `P` is one mono fixed-rate [`PhaseVocoder`], so a single shifter serves every line, and the
//! component is the mean of the feedback, which every line shares. The shifter runs once a
//! [`CHUNK`] on the chunk's `s`, so `q` is `s` delayed by the chunk and the vocoder's latency,
//! which is the loop delay of the shifted path ([`Shimmer::delay`]); the delay is what lets
//! a block of `s` be handed to the vocoder at once.
//!
//! # The loop gain
//!
//! The feedback splits along `u` and its complement, and `f'` leaves the complement alone: it
//! acts on `s` as `T s = (1 - k) s + (k / B) delay(P(s))`. A delay is an isometry of `l2`, so
//!
//! ```text
//! |T|  <=  (1 - k) + k |P| / B  <=  1 - k (1 - 1 / B)  <  1     for k > 0,
//! ```
//!
//! given `|P| <= 1`, and the whole loop is the mix `A` and the absorption `G` after `T`: its
//! gain is at most the absorption's. The bound on the vocoder `P` is the overlap-add's: a frame
//! `w x_k` keeps its energy through the unit-modulus phase rotation (Parseval), the
//! synthesis window and the division by the window energy `E` of the overlap bound each output
//! sample by `y^2 <= (sum_k f_k^2) / E` (Cauchy and Schwarz on the frames that cover it), and
//! the frames cover an input sample `r E` times at a shift of ratio `r` (they step `hop / r`),
//! which the read of the finished output at step `r` takes back (`1 / r` of the frames, or `r`
//! times as many): `|P| <= 1` up to the cubic read, whose gain is at most 1 at every fractional
//! offset and frequency (Catmull-Rom), and the rounding of a frame's start to a whole sample.
//! [`GAIN_BOUND`] `= B` is that figure with a margin; the tests measure `|P|` over noise,
//! sines, impulses, chirps and bursts at every shift and hold it to 1.
//!
//! `k` is at most [`Feedback::MAX_STABLE`](tutti_units::Feedback::MAX_STABLE), so the loop keeps `0.01` of the unshifted
//! component, and a send the control wrote past it is held there where the bank takes it.

use tutti_buffers::{ChannelLayout, Planes, PlanesMut};
use tutti_units::{Cents, SampleRate, Samples};

use crate::reverb::CHUNK;
use crate::{
    ColaGeometry, FftFloat, HopSize, PhaseVocoder, PhaseVocoderControls, StftGeometry, WindowLength,
};

/// The frames the analysis steps by: the window is four of them (a Hann window at the overlap
/// that sums to unity), so a shift of a hop of 512 frames is 11 ms of frame at 48 kHz.
const HOP: HopSize = HopSize::of(Samples::new(512));

/// The factor `B` the shifted feedback is divided by on its way back into the lines: above the
/// vocoder's gain over `l2`, which is 1 (see the module docs), so the shifted path is a
/// contraction by `1 / B` and the loop's gain stays below 1 for any send.
pub(crate) const GAIN_BOUND: f64 = 1.05;

/// The shifter of one shimmering [`FdnBank`](crate::FdnBank): a mono fixed-rate vocoder and the
/// chunk of the lines' summed feedback it is handed, with the shifted chunk it gave back.
#[derive(Debug)]
pub struct Shimmer<F: FftFloat> {
    rate: SampleRate,
    pitch: Cents,
    vocoder: PhaseVocoder<F>,
    /// The chunk of `s` being collected.
    send: Vec<F>,
    /// The shifted chunk of the one before, which the chunk being run reads.
    back: Vec<F>,
}

/// A fork is a shifter of its own: a new vocoder, silent, at the shift last set.
impl<F: FftFloat> Clone for Shimmer<F> {
    fn clone(&self) -> Self {
        let mut shimmer = Self::new(self.rate);
        shimmer.retarget(self.pitch);
        shimmer
    }
}

impl<F: FftFloat> Shimmer<F> {
    /// A silent shifter at `rate`, at no shift. Allocates.
    pub(crate) fn new(rate: SampleRate) -> Self {
        let window = WindowLength::new(Samples::new(4 * HOP.get().get()))
            .expect("four hops of at least one frame is a window");
        let geometry = ColaGeometry::try_from(StftGeometry::new(rate, window, HOP))
            .expect("a window of four hops is a Hann window at the overlap that sums to unity");
        Self {
            rate,
            pitch: Cents::new(0.0),
            vocoder: PhaseVocoder::fixed_rate(ChannelLayout::MONO, geometry),
            send: vec![F::ZERO; CHUNK],
            back: vec![F::ZERO; CHUNK],
        }
    }

    /// The frames from a sample of the feedback to its shifted copy re-entering the lines: the
    /// chunk it is collected in and the vocoder's latency.
    pub(crate) fn delay(&self) -> Samples {
        Samples::new(CHUNK) + self.vocoder.latency()
    }

    /// Shifts by `pitch` from the next hop on.
    pub(crate) fn retarget(&mut self, pitch: Cents) {
        self.pitch = pitch;
        self.vocoder.retarget(&PhaseVocoderControls {
            pitch,
            ..PhaseVocoderControls::UNITY
        });
    }

    /// Back to silence.
    pub(crate) fn reset(&mut self) {
        self.vocoder.reset();
        self.send.fill(F::ZERO);
        self.back.fill(F::ZERO);
    }

    /// The shifted feedback at frame `i` of the chunk being run.
    pub(crate) fn back(&self, i: usize) -> F {
        self.back[i]
    }

    /// Records the feedback component `s` of frame `i` of the chunk being run.
    pub(crate) fn push(&mut self, i: usize, s: F) {
        self.send[i] = s;
    }

    /// Hands the finished chunk to the vocoder: its output is the next chunk's `back`.
    pub(crate) fn end_chunk(&mut self) {
        let send = [&self.send[..]];
        let mut back = [&mut self.back[..]];
        let input = Planes::new(&send).expect("one plane of a chunk");
        let output = PlanesMut::new(&mut back).expect("one plane of a chunk");
        let _ = self.vocoder.process(input, output);
    }
}

#[cfg(test)]
mod tests;
