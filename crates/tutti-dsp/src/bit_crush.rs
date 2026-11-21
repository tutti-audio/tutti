//! [`BitCrusher`]: a sample-and-hold followed by a quantizer.
//!
//! Each frame is one of two steps, in this order:
//!
//! 1. **Hold.** Every `N` frames the input is captured, and the captured value is the
//!    output's source for the `N` frames from the capture on. `N` is the
//!    [`HoldFactor`] read by [`hold_frames`] (rounded, ties to even). The count of
//!    frames since the capture is continuous across blocks and shared by every channel,
//!    so all channels capture on the same frames: frame `k` of the stream reads
//!    input frame `floor(k / N) N`.
//! 2. **Quantize.** The held value `x` becomes `y = clamp(round(x q) / q, -1, 1)`
//!    with `q = 2^(b-1)` for the [`BitDepth`] `b` ([`quantize`]).
//!
//! The quantizer is applied to the held value every frame, so a depth that moves while a
//! sample is held changes what is heard of it at once.

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, InputOutput};
use tutti_units::{BitDepth, HoldFactor};

/// The word length a quantizer keeps by default, and the one a depth that is not finite
/// reads as: 8 bits.
pub(crate) const DEFAULT_BIT_DEPTH: BitDepth = BitDepth::new(8.0);

/// The frames one held sample lasts at `hold`: the factor rounded to the nearest whole
/// number, ties to even (`2.4` is 2, `2.6` is 3, `2.5` is 2 and `3.5` is 4), and at least 1.
/// A factor above the largest count saturates there, as `+inf` does (a hold that never
/// captures again); NaN reads as 1, no hold.
#[must_use]
pub fn hold_frames(hold: HoldFactor) -> usize {
    let hold = hold.get();
    if hold.is_nan() {
        1
    } else {
        // A float to integer cast saturates, so a huge factor and `+inf` hold for the
        // longest span, and a factor under 1 reads as 1.
        (hold.round_ties_even() as usize).max(1)
    }
}

/// The levels per unit of amplitude, `q = 2^(b-1)`, of `bits` read within
/// [`BitDepth::MIN`] and [`BitDepth::MAX`] (a depth that is not finite reads as
/// [`DEFAULT_BIT_DEPTH`]). Whole depths are exact powers of two.
fn levels(bits: BitDepth) -> f64 {
    let b = if bits.get().is_finite() {
        BitDepth::new_clamped(bits.get()).get()
    } else {
        DEFAULT_BIT_DEPTH.get()
    };
    if b.fract() == 0.0 {
        f64::from(1u32 << (b as u32 - 1))
    } else {
        (b - 1.0).exp2()
    }
}

/// `x` quantized to `bits`: `clamp(round(x q) / q, -1, 1)` with `q = 2^(b-1)`.
///
/// The rounding is half to even. A whole depth `b` has the steps `j / q` for the integers
/// `j` in `-q..=q`: `2^b + 1` levels over `-1..=1`, with `0` one of them. A fractional
/// depth keeps the same rule with a real `q`, so its steps are `j / q` for the integers
/// `j` with `|j / q| <= 1`, and the ends `-1` and `1` too where `round(q) / q` is above 1.
/// The clamp holds a value past `+-1` at the end level. NaN stays NaN, and an infinity is
/// an end level. `q` is solved in `f64` and crosses to `F` once.
#[must_use]
pub fn quantize<F: Float>(x: F, bits: BitDepth) -> F {
    crush(x, F::from_design(levels(bits)))
}

/// [`quantize`] with `q` already read from the depth.
#[inline(always)]
fn crush<F: Float>(x: F, q: F) -> F {
    ((x * q).round_ties_even() / q).clamp(-F::ONE, F::ONE)
}

/// The controls of a [`BitCrusher`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BitCrusherControls {
    /// The word length of the quantizer, read within [`BitDepth::MIN`] and
    /// [`BitDepth::MAX`].
    pub bits: BitDepth,
    /// The frames each held sample lasts, read by [`hold_frames`].
    pub hold: HoldFactor,
}

impl BitCrusherControls {
    /// 8 bits and no hold.
    pub const DEFAULT: Self = Self {
        bits: DEFAULT_BIT_DEPTH,
        hold: HoldFactor::NONE,
    };
}

/// A bit crusher of any width in the float `F`: one held value per channel, one frame count
/// for all.
///
/// Built on the control side; nothing it holds grows. Its controls land whole.
#[derive(Clone, Debug)]
pub struct BitCrusher<F: Float> {
    controls: BitCrusherControls,
    /// Frames since the last capture; the next frame captures when it is 0 or has reached
    /// the hold.
    since: usize,
    /// The held input of each channel: a per-sample value in `F`.
    held: Vec<F>,
}

impl<F: Float> BitCrusher<F> {
    /// A crusher of `width` channels with `controls`, silent: its first frame captures.
    #[must_use]
    pub fn new(width: ChannelLayout, controls: BitCrusherControls) -> Self {
        Self {
            controls,
            since: 0,
            held: vec![F::ZERO; usize::from(width.count())],
        }
    }

    /// Takes `controls` from the next frame on, the held values kept. No allocation.
    pub fn retarget(&mut self, controls: &BitCrusherControls) {
        self.controls = *controls;
    }

    /// Back to silence: the next frame captures.
    pub fn reset(&mut self) {
        self.held.fill(F::ZERO);
        self.since = 0;
    }

    /// Runs `input` into `output`, every channel on the same capture frames. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process(&mut self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        let width = self.held.len();
        expect_widths(block, width, width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let hold = hold_frames(self.controls.hold);
        let q = F::from_design(levels(self.controls.bits));
        let start = self.since;
        let mut end = start;
        for (c, held) in self.held.iter_mut().enumerate().take(width) {
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            let y = &mut plane.samples_mut()[..frames];
            let mut since = start;
            for (y, &x) in y.iter_mut().zip(x) {
                if since >= hold {
                    since = 0;
                }
                if since == 0 {
                    *held = x;
                }
                since += 1;
                *y = crush(*held, q);
            }
            end = since;
        }
        if width > 0 {
            self.since = end;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
