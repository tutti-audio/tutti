//! A variable-ratio windowed-sinc resampler: a read head that moves `r` input frames per
//! output frame, `r` set afresh on every call.
//!
//! It reads the input as a band-limited signal, `x(t) = sum_m x[m] K(t - m)`, at the
//! positions `p_n = p_0 + r n` of the output frames. `K` is a [`WindowedSinc`] with its
//! cutoff at Nyquist (so `K(m) = 0` at every other integer and the kernel reads an input
//! frame back unchanged where the head sits on it) and a Blackman-Harris window over
//! `CROSSINGS` zero crossings each side: `TAPS` input frames per output frame. The
//! kernel is tabulated once, control-side, at `PHASES` fractional offsets, and a read
//! interpolates linearly between the two nearest rows, so a call allocates nothing.
//!
//! # Ratio
//!
//! The ratio is a [`ReadRate`]: input frames consumed per output frame. It is read within
//! [`VARIABLE_MIN_RATE`]..=[`VARIABLE_MAX_RATE`] (0.95 to 1.05, 50 000 ppm either side of unity,
//! fifty times the drift of a pair of free-running audio clocks) and a ratio that is not
//! finite is unity. It is constant within a call and may differ between calls; the head is a
//! [`ReadPosition`], a `2^-32` frame count, so a head moved a million times keeps its
//! place. A ratio of exactly 1 reads every frame at phase 0: the output is the input
//! delayed by [`VARIABLE_LATENCY`] frames, to the round-off of the table's zeros.
//!
//! # Input and output
//!
//! A call is driven by its output: [`process`](VariableResampler::process) fills every
//! frame of the output view, and [`input_needed`](VariableResampler::input_needed) says how
//! many input frames that takes at a ratio, so a ring-fed caller peeks that many, calls
//! `process`, and pops the [`Samples`] it returns. The count returned never exceeds the
//! count needed and is at most one less than it; the frame between is offered again by the
//! next call. The resampler keeps its own history of the 63 frames before the head.
//!
//! # Spec figures
//!
//! For ratios within the range, in units of the input Nyquist frequency `f_N = fs / 2`:
//!
//! | figure | value |
//! |---|---|
//! | passband edge | [`VARIABLE_PASSBAND_EDGE`] (`0.85 f_N`; 20.4 kHz at 48 kHz) |
//! | passband ripple | at most [`VARIABLE_PASSBAND_RIPPLE`] dB to the edge |
//! | stopband attenuation | at least [`VARIABLE_STOPBAND_ATTENUATION`] dB for the images of a component at or under the edge (everything at or above `2 f_N` minus the edge) |
//! | latency | [`VARIABLE_LATENCY`] input frames |
//!
//! An image is the mirror a sampled signal has about `fs`: a component at `f` under the
//! edge has its image at `2 f_N - f`, which the kernel attenuates. A component between the
//! edge and `f_N` is not guaranteed: at ratios over 1 the part above `f_N / r` folds
//! into the output band.

use tutti_buffers::{ChannelLayout, Float, Planes, PlanesMut};
use tutti_units::{ReadPosition, ReadRate, SampleSpan, Samples};

use super::{ResampleError, WindowedSinc};
use crate::CosineWindow;

/// Zero crossings of the kernel each side of its centre.
const CROSSINGS: usize = 32;
/// Input frames read per output frame.
const TAPS: usize = 2 * CROSSINGS;
/// Fractional offsets the kernel is tabulated at; a read interpolates linearly between two.
const PHASES: usize = 512;
/// The input frames kept before the head: the window less the frame it ends on.
const HISTORY: usize = TAPS - 1;

/// The slowest read, 0.95 input frames per output frame.
pub const VARIABLE_MIN_RATE: ReadRate = ReadRate::new(0.95);
/// The fastest read, 1.05 input frames per output frame.
pub const VARIABLE_MAX_RATE: ReadRate = ReadRate::new(1.05);
/// The latency in input frames at any ratio in range: the output at `n` is the input at
/// `r n - VARIABLE_LATENCY`.
pub const VARIABLE_LATENCY: Samples = Samples::new(CROSSINGS);
/// The frequency, as a fraction of the input Nyquist frequency, up to which the gain is
/// within [`VARIABLE_PASSBAND_RIPPLE`] of unity.
pub const VARIABLE_PASSBAND_EDGE: f64 = 0.85;
/// The most the gain departs from unity below [`VARIABLE_PASSBAND_EDGE`], in dB.
pub const VARIABLE_PASSBAND_RIPPLE: f64 = 0.001;
/// The least attenuation of the images of a component under [`VARIABLE_PASSBAND_EDGE`], in dB.
pub const VARIABLE_STOPBAND_ATTENUATION: f64 = 100.0;

/// A resampler of any width whose ratio is set per call; see the [module](self).
///
/// Built on the control side ([`new`](Self::new) allocates the table and the history); every
/// method after it allocates nothing.
#[derive(Clone, Debug)]
pub struct VariableResampler<F: Float> {
    /// `PHASES` rows of `2 * TAPS`: the `TAPS` weights at the row's offset, then their
    /// differences to the next row's.
    table: Vec<F>,
    /// The `HISTORY` frames before the head, plane after plane.
    history: Vec<F>,
    /// A window that straddles the history and the input, gathered whole.
    window: Vec<F>,
    channels: usize,
    /// The next window's first frame (counted in the history followed by the input) and the
    /// fraction of a frame the head sits past its centre's.
    head: ReadPosition,
}

impl<F: Float> VariableResampler<F> {
    /// A resampler of `width` channels, silent, its head at the first frame.
    #[must_use]
    pub fn new(width: ChannelLayout) -> Self {
        let channels = usize::from(width.count());
        let kernel = kernel();
        // Row `p`, tap `k` reads the input `k - CROSSINGS + 1` frames from the frame the
        // head is past, at offset `p / PHASES` frames: the kernel at that distance.
        let row = |p: usize| -> Vec<f64> {
            let offset = p as f64 / PHASES as f64;
            let mut w: Vec<f64> = (0..TAPS)
                .map(|k| kernel.weight(offset + (CROSSINGS - 1) as f64 - k as f64))
                .collect();
            let sum: f64 = w.iter().sum();
            w.iter_mut().for_each(|w| *w /= sum);
            w
        };
        let mut table = Vec::with_capacity(PHASES * 2 * TAPS);
        let mut below = row(0);
        for p in 0..PHASES {
            let above = row(p + 1);
            table.extend(below.iter().map(|&w| F::from_design(w)));
            table.extend(
                below
                    .iter()
                    .zip(&above)
                    .map(|(&b, &a)| F::from_design(a - b)),
            );
            below = above;
        }
        Self {
            table,
            history: vec![F::ZERO; channels * HISTORY],
            window: vec![F::ZERO; TAPS],
            channels,
            head: ReadPosition::START,
        }
    }

    /// Silences the history and puts the head back at the first frame.
    pub fn reset(&mut self) {
        self.history.fill(F::ZERO);
        self.head = ReadPosition::START;
    }

    /// The delay in input frames: [`VARIABLE_LATENCY`].
    #[must_use]
    pub const fn latency(&self) -> Samples {
        VARIABLE_LATENCY
    }

    /// The channel count.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The input frames [`process`](Self::process) reads to fill `output` frames at `ratio`
    /// from here: the call fails with fewer, and the extra frames it reads (at most one
    /// over what it consumes) are offered again by the next call.
    #[must_use]
    pub fn input_needed(&self, output: Samples, ratio: ReadRate) -> Samples {
        let Some(last) = output.get().checked_sub(1) else {
            return Samples::new(0);
        };
        let step = step_of(ratio);
        let read = advanced(self.head, step, last).frames().get() + 1;
        let moved = advanced(self.head, step, output.get()).frames().get();
        Samples::new(read.max(moved))
    }

    /// Fills every frame of `output` from `input` at `ratio` and returns the input frames
    /// consumed, which the caller drops before the next call. `input` holds at least
    /// [`input_needed`](Self::input_needed) frames. No allocation.
    ///
    /// # Errors
    ///
    /// [`ResampleError::WidthMismatch`] for a view of another width and
    /// [`ResampleError::BufferMismatch`] for an input shorter than needed. Nothing is read
    /// or written then.
    pub fn process(
        &mut self,
        input: Planes<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
        ratio: ReadRate,
    ) -> Result<Samples, ResampleError> {
        for got in [input.channels(), output.channels()] {
            if got != self.channels {
                return Err(ResampleError::WidthMismatch {
                    expected: self.channels,
                    got,
                });
            }
        }
        let out_frames = output.frames();
        let needed = self.input_needed(out_frames, ratio);
        if input.frames() < needed {
            return Err(ResampleError::BufferMismatch {
                channels: self.channels,
                frames_in: needed.get(),
                frames_out: out_frames.get(),
            });
        }
        let step = step_of(ratio);
        let end = advanced(self.head, step, out_frames.get());
        let consumed = end.frames().get();
        for c in 0..self.channels {
            let x = input.plane(c);
            let history = &mut self.history[c * HISTORY..(c + 1) * HISTORY];
            let mut y = output.plane_mut(c);
            for (n, y) in y.samples_mut().iter_mut().enumerate() {
                let (first, fraction) = f64::split_position(advanced(self.head, step, n));
                let first = first.get();
                let window: &[F] = if first >= HISTORY {
                    &x.samples()[first - HISTORY..first - HISTORY + TAPS]
                } else {
                    for (k, w) in self.window.iter_mut().enumerate() {
                        let at = first + k;
                        *w = if at < HISTORY {
                            history[at]
                        } else {
                            x.samples()[at - HISTORY]
                        };
                    }
                    &self.window
                };
                *y = read(&self.table, window, fraction);
            }
            // The history for the next call: the last `HISTORY` frames of the old history
            // followed by the `consumed` input frames.
            let input = &x.samples()[..consumed];
            if consumed >= HISTORY {
                history.copy_from_slice(&input[consumed - HISTORY..]);
            } else {
                history.copy_within(consumed.., 0);
                history[HISTORY - consumed..].copy_from_slice(input);
            }
        }
        self.head = end.saturating_sub(Samples::new(consumed));
        Ok(Samples::new(consumed))
    }
}

/// The kernel: cutoff at Nyquist, so an input frame the head sits on reads back unchanged.
fn kernel() -> WindowedSinc {
    WindowedSinc::new(1.0, CROSSINGS as f64, CosineWindow::BLACKMAN_HARRIS)
        .expect("the cutoff and the crossing count are positive constants")
}

/// One output frame: the window dotted with the row `fraction` of a frame in, and with the
/// row's differences to the next, scaled by how far between the rows `fraction` falls.
#[inline]
fn read<F: Float>(table: &[F], window: &[F], fraction: f64) -> F {
    let scaled = fraction * PHASES as f64;
    let row = scaled as usize;
    let between = F::from_design(scaled - row as f64);
    let (weights, slopes) = table[row * 2 * TAPS..(row + 1) * 2 * TAPS].split_at(TAPS);
    let (mut value, mut slope) = (F::ZERO, F::ZERO);
    for ((&x, &w), &d) in window.iter().zip(weights).zip(slopes) {
        value += x * w;
        slope += x * d;
    }
    value + between * slope
}

/// The head's move per output frame, in `2^-32` frames.
fn step_of(ratio: ReadRate) -> u64 {
    let ratio = ratio
        .finite_or(ReadRate::UNITY)
        .clamp(VARIABLE_MIN_RATE, VARIABLE_MAX_RATE);
    ReadPosition::from_span(SampleSpan::clamped(ratio.get())).get()
}

/// `head` moved by `n` steps, saturating.
fn advanced(head: ReadPosition, step: u64, n: usize) -> ReadPosition {
    ReadPosition::from_count(head.get().saturating_add(step.saturating_mul(n as u64)))
}

#[cfg(test)]
mod tests;
