//! Pre-allocated scratch buffers for VST2 process calls.
//!
//! The `vst` crate's `AudioBuffer` wants contiguous raw pointer tables, which a
//! caller's planes cannot supply: the channel slices are not adjacent in memory.
//! These buffers are allocated once at load time and reused on every block, which
//! is what keeps the audio thread allocation-free.
//!
//! VST 2.4 has two render entry points, `processReplacing` and
//! `processReplacingF64`, and a plugin is told one precision at activation
//! (`effSetProcessPrecision`). The scratch is built for that format and holds
//! rows of that width only: a block of the other width is converted at the
//! scratch by the [`Sample`] rule, and `processReplacingF64` is entered only
//! through a scratch built for [`FloatWidth::F64`].

use num_traits::Float as VstFloat;
use tutti_buffers::FloatWidth;
use tutti_buffers::{Float, Planes, PlanesMut, Sample};
use tutti_plugin_types::{ChannelLayout, Samples};

use crate::buffer::AudioBuffer;
use crate::host::RenderHandle;

/// Pre-allocated audio buffers that `Vst2Processor::process` renders through.
///
/// VST2 wants contiguous pointer tables, which a caller's planes cannot
/// supply, so each block is converted into these buffers and back out. One per
/// activation, sized from the pin counts the plugin has then, the block size
/// of the [`Prepare`](tutti_plugin_types::Prepare) and its negotiated format,
/// and reused for every block: after construction nothing here allocates, so
/// it is safe on the audio thread.
///
/// The pointer tables behind the plugin's buffers are private, and a render is
/// reached only through `render_piece`, which stages the block, checks its
/// length and renders in one step. Safe code cannot read the tables or render a
/// block that was not staged:
///
/// ```compile_fail,E0616
/// # use tutti_vst2_ffi::RenderScratch;
/// # use tutti_buffers::FloatWidth;
/// # use tutti_plugin_types::ChannelLayout;
/// let scratch = RenderScratch::new(
///     ChannelLayout::from(2u8),
///     ChannelLayout::from(2u8),
///     64,
///     FloatWidth::F32,
/// );
/// let _ = &scratch.rows;
/// ```
///
/// ```compile_fail,E0603
/// use tutti_vst2_ffi::scratch::Rows;
/// ```
pub struct RenderScratch {
    rows: Format,
    /// The most frames a render may be asked for: what every Vec was sized to.
    block_size: usize,
}

/// The rows of the width the plugin was activated at.
enum Format {
    F32(Rows<f32>),
    F64(Rows<f64>),
}

/// One width's channel rows and the pointer tables the plugin is handed.
struct Rows<T> {
    inputs: Vec<Vec<T>>,
    outputs: Vec<Vec<T>>,
    input_ptrs: Vec<*const T>,
    output_ptrs: Vec<*mut T>,
}

impl std::fmt::Debug for RenderScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderScratch")
            .field("format", &self.format())
            .field("block_size", &self.block_size)
            .finish_non_exhaustive()
    }
}

// SAFETY: the raw pointers point into the owned `Vec<Vec<_>>` fields within the same
// struct, whose heap blocks move with it, so moving the struct to another thread moves
// what they point at. Not `Sync`: nothing shares one, and an argument that "callers
// serialize" is not one for `Sync`.
unsafe impl Send for RenderScratch {}

impl RenderScratch {
    /// Creates scratch buffers for the given channel counts at `format`'s width.
    ///
    /// Call once at load time, never on the audio thread: this is the only
    /// allocation on the render path. `block_size` is the maximum block in
    /// frames; a process call may render fewer, never more. `format` is
    /// [`FloatWidth::F64`] only for a plugin that installed
    /// `processReplacingF64` and was told 64-bit precision
    /// ([`Widths::negotiate`](tutti_plugin_types::Widths::negotiate)).
    #[must_use]
    pub fn new(
        num_inputs: ChannelLayout,
        num_outputs: ChannelLayout,
        block_size: usize,
        format: FloatWidth,
    ) -> Self {
        let (i, o) = (
            usize::from(num_inputs.count()),
            usize::from(num_outputs.count()),
        );
        let rows = match format {
            FloatWidth::F32 => Format::F32(Rows::new(i, o, block_size)),
            FloatWidth::F64 => Format::F64(Rows::new(i, o, block_size)),
        };
        Self { rows, block_size }
    }

    /// The width the plugin is entered at.
    #[must_use]
    pub fn format(&self) -> FloatWidth {
        match self.rows {
            Format::F32(_) => FloatWidth::F32,
            Format::F64(_) => FloatWidth::F64,
        }
    }

    /// Stages the piece `start..start + len` of the caller's block, renders it through
    /// `render` and converts the outputs back: the one way in, so the pointer tables handed to
    /// the plugin are always the ones just staged, and `len` is checked against the block size.
    ///
    /// The entry point is the scratch's [`format`](Self::format): `processReplacing` over `f32`
    /// rows, or `processReplacingF64` over `f64` rows. A block of the other width is converted
    /// at the scratch by the [`Sample`] rule, so a 64-bit block for a 32-bit plugin is narrowed
    /// and widened back: lossy, deliberately. A caller channel the scratch lacks is not
    /// rendered, and a scratch input the caller lacks is silence.
    ///
    /// # Panics
    ///
    /// If `len` exceeds the block size the scratch was made for, or the piece is not inside
    /// the caller's block.
    pub fn render_piece<S: Float>(
        &mut self,
        render: &mut RenderHandle,
        inputs: Planes<'_, S>,
        outputs: &mut PlanesMut<'_, '_, S>,
        start: usize,
        len: usize,
    ) {
        self.check_block(len);
        let piece = Samples::new(start)..Samples::new(start + len);
        // A block with no input channels has no frames to slice: it stages silence.
        let inputs = if inputs.channels() == 0 {
            Planes::default()
        } else {
            inputs.slice(piece.clone())
        };
        let mut outputs = outputs.slice_mut(piece);
        match &mut self.rows {
            Format::F32(rows) => rows.render(&inputs, &mut outputs, len, |b| render.process(b)),
            Format::F64(rows) => rows.render(&inputs, &mut outputs, len, |b| render.process_f64(b)),
        }
    }

    /// Refuses a render longer than the Vecs behind the pointer tables.
    fn check_block(&self, num_samples: usize) {
        assert!(
            num_samples <= self.block_size,
            "a render of {num_samples} frames exceeds the scratch's block size {}",
            self.block_size
        );
    }
}

impl<T: Float + VstFloat> Rows<T> {
    fn new(num_inputs: usize, num_outputs: usize, block_size: usize) -> Self {
        let inputs: Vec<Vec<T>> = (0..num_inputs)
            .map(|_| vec![T::SILENCE; block_size])
            .collect();
        let outputs: Vec<Vec<T>> = (0..num_outputs)
            .map(|_| vec![T::SILENCE; block_size])
            .collect();
        let input_ptrs = inputs.iter().map(|v| v.as_ptr()).collect();
        let output_ptrs = outputs.iter().map(|v| v.as_ptr().cast_mut()).collect();
        Self {
            inputs,
            outputs,
            input_ptrs,
            output_ptrs,
        }
    }

    /// Stages `len` frames of `inputs`, hands the plugin the tables through `enter` and emits
    /// the outputs into `outputs`. `len` is at most the frames every row was sized to.
    fn render<S: Float>(
        &mut self,
        inputs: &Planes<'_, S>,
        outputs: &mut PlanesMut<'_, '_, S>,
        len: usize,
        enter: impl FnOnce(&mut AudioBuffer<'_, T>),
    ) {
        stage(&mut self.inputs, inputs, len);
        zero(&mut self.outputs, len);
        self.refresh_ptrs();
        // SAFETY: the tables hold one pointer per channel row, and each row is at least `len`
        // elements long (`stage` and `zero` just sliced `..len` of every row, and the caller
        // checked `len` against the block size). The rows are never resized, so the pointers
        // stay valid; the output rows are distinct allocations from the inputs and from each
        // other, and `self` is borrowed mutably for the whole call, so nothing else touches
        // them.
        let mut buffer = unsafe {
            AudioBuffer::from_raw(
                self.input_ptrs.len(),
                self.output_ptrs.len(),
                self.input_ptrs.as_ptr(),
                self.output_ptrs.as_mut_ptr(),
                len,
            )
        };
        enter(&mut buffer);
        emit(&self.outputs, outputs, len);
    }

    /// Points each table entry at its row again, before each `from_raw`.
    fn refresh_ptrs(&mut self) {
        for (ptr, row) in self.input_ptrs.iter_mut().zip(&self.inputs) {
            *ptr = row.as_ptr();
        }
        for (ptr, row) in self.output_ptrs.iter_mut().zip(&mut self.outputs) {
            *ptr = row.as_mut_ptr();
        }
    }
}

/// Converts the first `len` frames of each caller channel into the scratch row of the same
/// index: a row the caller has no channel for is silence, a caller channel past the rows is not
/// staged.
fn stage<T: Sample, S: Sample>(rows: &mut [Vec<T>], from: &Planes<'_, S>, len: usize) {
    for (c, row) in rows.iter_mut().enumerate() {
        let row = &mut row[..len];
        match from.get_plane(c) {
            Some(plane) => {
                for (d, &s) in row.iter_mut().zip(plane.samples()) {
                    *d = s.convert_to();
                }
            }
            None => row.fill(T::SILENCE),
        }
    }
}

/// Silences the first `len` frames of each scratch row.
fn zero<T: Sample>(rows: &mut [Vec<T>], len: usize) {
    for row in rows {
        row[..len].fill(T::SILENCE);
    }
}

/// Converts the first `len` frames of each scratch row into the caller channel of the same
/// index; a caller channel past the rows keeps what it held.
fn emit<T: Sample, S: Sample>(rows: &[Vec<T>], to: &mut PlanesMut<'_, '_, S>, len: usize) {
    for (c, row) in rows.iter().enumerate() {
        if let Some(plane) = to.get_plane_mut(c) {
            for (d, &s) in plane.into_samples_mut().iter_mut().zip(&row[..len]) {
                *d = s.convert_to();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A render longer than the block the Vecs were sized for is refused**, before the
    /// plugin sees pointers that would run past them.
    #[test]
    #[should_panic(expected = "exceeds the scratch's block size")]
    fn a_render_past_the_block_size_is_refused() {
        let layout = ChannelLayout::from(2u8);
        RenderScratch::new(layout, layout, 64, FloatWidth::F32).check_block(65);
    }

    /// A render at the block size is accepted.
    #[test]
    fn a_render_at_the_block_size_is_accepted() {
        let layout = ChannelLayout::from(2u8);
        RenderScratch::new(layout, layout, 64, FloatWidth::F32).check_block(64);
    }

    /// **The scratch holds the width it was built for**, and only that one.
    #[test]
    fn the_scratch_is_built_for_one_format() {
        let layout = ChannelLayout::from(2u8);
        for format in [FloatWidth::F32, FloatWidth::F64] {
            assert_eq!(
                RenderScratch::new(layout, layout, 8, format).format(),
                format
            );
        }
    }

    /// Scratch rows holding a recognizable ramp, so an emit can be checked sample by sample.
    fn ramp_rows(channels: usize, block: usize) -> Vec<Vec<f32>> {
        (0..channels)
            .map(|c| (0..block).map(|i| (c * 100 + i) as f32).collect())
            .collect()
    }

    /// Emitting a block converts each row into the caller channel of its index, at the caller's
    /// width: an f64 caller reads the f32 rows widened exactly.
    #[test]
    fn emit_widens_the_rows_into_a_64_bit_caller() {
        let rows = ramp_rows(2, 4);
        let (mut l, mut r) = ([-1.0f64; 4], [-1.0f64; 4]);
        let mut chans = [&mut l[..], &mut r[..]];
        let mut outs = PlanesMut::new(&mut chans).expect("equal channels");
        emit(&rows, &mut outs, 4);
        assert_eq!(l, [0.0, 1.0, 2.0, 3.0]);
        assert_eq!(r, [100.0, 101.0, 102.0, 103.0]);
    }

    /// Only the first `len` frames of the rows are emitted: a caller block longer than the piece
    /// keeps what lay past it.
    #[test]
    fn emit_leaves_the_caller_frames_past_the_piece_alone() {
        let rows = ramp_rows(1, 8);
        let mut l = [-1.0f32; 8];
        let mut chans = [&mut l[..]];
        let mut outs = PlanesMut::new(&mut chans).expect("one channel");
        let mut piece = outs.slice_mut(Samples::new(0)..Samples::new(4));
        emit(&rows, &mut piece, 4);
        assert_eq!(l, [0.0, 1.0, 2.0, 3.0, -1.0, -1.0, -1.0, -1.0]);
    }

    /// More caller channels than the scratch holds: the extras are skipped, not indexed into.
    #[test]
    fn extra_caller_channels_are_ignored() {
        let rows = ramp_rows(1, 4);
        let (mut a, mut b) = ([-1.0f32; 4], [-1.0f32; 4]);
        let mut chans = [&mut a[..], &mut b[..]];
        let mut outs = PlanesMut::new(&mut chans).expect("equal channels");
        emit(&rows, &mut outs, 4);
        assert_eq!(a, [0.0, 1.0, 2.0, 3.0]);
        assert_eq!(b, [-1.0; 4], "channel 1 has no scratch behind it");
    }

    /// A scratch input the caller has no channel for is silence, not the previous block's samples.
    #[test]
    fn a_scratch_input_without_a_caller_channel_is_silence() {
        let mut rows = vec![vec![9.0f32; 4], vec![9.0f32; 4]];
        let src = [0.5f32; 4];
        let chans = [&src[..]];
        stage(&mut rows, &Planes::new(&chans).expect("one channel"), 4);
        assert_eq!(rows[0], [0.5; 4]);
        assert_eq!(rows[1], [0.0; 4]);
    }

    /// Staging into the f64 rows carries a value that has no f32 representation through
    /// unchanged: staging through an f32 row would lose it before the plugin was entered.
    #[test]
    fn staging_f64_into_f64_rows_does_not_narrow() {
        // Needs 53 bits of mantissa: exactly representable in f64, rounds in f32.
        const EXACT: f64 = 1.0 + f64::EPSILON;
        assert_ne!(
            EXACT as f32 as f64, EXACT,
            "the fixture is vacuous unless this value rounds in f32"
        );
        let mut rows = vec![vec![0.0f64; 4]];
        let src = [EXACT; 4];
        let chans = [&src[..]];
        stage(&mut rows, &Planes::new(&chans).expect("one channel"), 4);
        assert_eq!(rows[0], [EXACT; 4]);
    }
}
