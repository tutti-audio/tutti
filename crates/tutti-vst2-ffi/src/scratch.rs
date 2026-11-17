//! Pre-allocated scratch buffers for VST2 process calls.
//!
//! The `vst` crate's `AudioBuffer` wants contiguous raw pointer tables, which a
//! caller's planes cannot supply: the channel slices are not adjacent in memory. These buffers are allocated once at load time and reused
//! on every block, which is what keeps the audio thread allocation-free.
//!
//! Both widths are resident. VST 2.4 has two render entry points —
//! `processReplacing` and `processReplacingF64` — and the caller picks per
//! block, not per instance: the caller picks per block
//! (`PluginAudio` is either width) and the scratch does not know at construction which it will
//! be asked for; growing a buffer on the first f64 block would allocate on
//! the audio thread.

use tutti_buffers::{Float, Mono, MonoMut, Planes, PlanesMut, Sample};
use tutti_plugin_types::{ChannelLayout, Samples, convert_channel};

use crate::host::RenderHandle;

/// Pre-allocated audio buffers that `Vst2Processor::process` renders through.
///
/// VST2 wants contiguous pointer tables, which a caller's planes cannot
/// supply, so each block is converted into these buffers and back out. One per
/// activation, sized from the pin counts the plugin has then and the block size
/// of the [`Prepare`](tutti_plugin_types::Prepare), and reused for every block:
/// after construction nothing here allocates, so it is safe on the audio thread.
///
/// Carrying both costs `block_size * (in + out) * 12` bytes over one width —
/// at a 512-sample block and stereo I/O, 24 KiB.
///
/// The pointer tables behind the plugin's buffers are private, and a render is reached only
/// through `render_piece`, which stages the block, check its length and render in
/// one step. Safe code cannot read the tables or render a block that was not staged:
///
/// ```compile_fail,E0616
/// # use tutti_vst2_ffi::scratch::RenderScratch;
/// # use tutti_plugin_types::ChannelLayout;
/// let scratch = RenderScratch::new(ChannelLayout::from(2u8), ChannelLayout::from(2u8), 64);
/// let _ = &scratch.output_ptrs;
/// ```
///
/// ```compile_fail,E0624
/// # use tutti_vst2_ffi::scratch::RenderScratch;
/// # use tutti_plugin_types::ChannelLayout;
/// # fn go(handle: &mut tutti_vst2_ffi::host::RenderHandle) {
/// let mut scratch = RenderScratch::new(ChannelLayout::from(2u8), ChannelLayout::from(2u8), 64);
/// scratch.render_f32(handle, 1 << 20);
/// # }
/// ```
pub struct RenderScratch {
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
    input_ptrs: Vec<*const f32>,
    output_ptrs: Vec<*mut f32>,
    inputs_f64: Vec<Vec<f64>>,
    outputs_f64: Vec<Vec<f64>>,
    input_ptrs_f64: Vec<*const f64>,
    output_ptrs_f64: Vec<*mut f64>,
    /// The most frames a render may be asked for: what every Vec was sized to.
    block_size: usize,
}

impl std::fmt::Debug for RenderScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderScratch")
            .field("input_ptrs", &self.input_ptrs)
            .field("output_ptrs", &self.output_ptrs)
            .field("input_ptrs_f64", &self.input_ptrs_f64)
            .field("output_ptrs_f64", &self.output_ptrs_f64)
            .finish_non_exhaustive()
    }
}

// SAFETY: the raw pointers point into the owned `Vec<Vec<_>>` fields within the same
// struct, whose heap blocks move with it, so moving the struct to another thread moves
// what they point at. Not `Sync`: nothing shares one, and an argument that "callers
// serialize" is not one for `Sync`.
unsafe impl Send for RenderScratch {}

impl RenderScratch {
    /// Creates scratch buffers for the given channel counts, allocating both
    /// sample widths up front.
    ///
    /// Call once at load time, never on the audio thread: this is the only
    /// allocation on the render path. `block_size` is the maximum block in
    /// frames; a process call may render fewer, never more.
    #[must_use]
    pub fn new(num_inputs: ChannelLayout, num_outputs: ChannelLayout, block_size: usize) -> Self {
        let inputs: Vec<Vec<f32>> = (0..num_inputs.count())
            .map(|_| vec![0.0f32; block_size])
            .collect();
        let outputs: Vec<Vec<f32>> = (0..num_outputs.count())
            .map(|_| vec![0.0f32; block_size])
            .collect();
        let input_ptrs: Vec<*const f32> = inputs.iter().map(|v| v.as_ptr()).collect();
        let output_ptrs: Vec<*mut f32> = outputs.iter().map(|v| v.as_ptr() as *mut f32).collect();

        let inputs_f64: Vec<Vec<f64>> = (0..num_inputs.count())
            .map(|_| vec![0.0f64; block_size])
            .collect();
        let outputs_f64: Vec<Vec<f64>> = (0..num_outputs.count())
            .map(|_| vec![0.0f64; block_size])
            .collect();
        let input_ptrs_f64: Vec<*const f64> = inputs_f64.iter().map(|v| v.as_ptr()).collect();
        let output_ptrs_f64: Vec<*mut f64> =
            outputs_f64.iter().map(|v| v.as_ptr() as *mut f64).collect();

        Self {
            inputs,
            outputs,
            input_ptrs,
            output_ptrs,
            inputs_f64,
            outputs_f64,
            input_ptrs_f64,
            output_ptrs_f64,
            block_size,
        }
    }

    /// Stages the piece `start..start + len` of the caller's block, renders it through
    /// `render` and converts the outputs back: the one way in, so the pointer tables handed to
    /// the plugin are always the ones just staged, and `len` is checked against the block size.
    ///
    /// `double` picks the entry point: `processReplacingF64` over the `f64` scratch, which only
    /// a plugin known to install it may take (the vendor's `process_f64` zeroes the outputs when
    /// it installed none, so reaching it unconditionally would render silence rather than fall
    /// back), or `processReplacing` over the `f32` scratch. A block of the other width is
    /// converted at the scratch by the [`Sample`] rule, so a 64-bit block
    /// for a 32-bit plugin is narrowed and widened back: lossy, deliberately. A caller channel
    /// the scratch lacks is not rendered, and a scratch input the caller lacks is silence.
    ///
    /// # Panics
    ///
    /// If `len` exceeds the block size the scratch was made for, or the piece is not inside
    /// the caller's block.
    pub fn render_piece<S: Float>(
        &mut self,
        render: &mut RenderHandle,
        double: bool,
        inputs: Planes<'_, S>,
        outputs: &mut PlanesMut<'_, '_, S>,
        start: usize,
        len: usize,
    ) {
        self.check_block(len);
        let piece = Samples(start)..Samples(start + len);
        // A block with no input channels has no frames to slice: it stages silence.
        let inputs = if inputs.channels() == 0 {
            Planes::default()
        } else {
            inputs.slice(piece.clone())
        };
        let mut outputs = outputs.slice_mut(piece);
        if double {
            stage(&mut self.inputs_f64, &inputs, len);
            zero(&mut self.outputs_f64, len);
            self.refresh_ptrs_f64();
            self.render_f64(render, len);
            emit(&self.outputs_f64, &mut outputs, len);
        } else {
            stage(&mut self.inputs, &inputs, len);
            zero(&mut self.outputs, len);
            self.refresh_ptrs();
            self.render_f32(render, len);
            emit(&self.outputs, &mut outputs, len);
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

    /// Renders the first `num_samples` of the staged block through `render`, in `f32`.
    fn render_f32(&mut self, render: &mut RenderHandle, num_samples: usize) {
        use crate::buffer::AudioBuffer;

        self.check_block(num_samples);
        // SAFETY: the tables hold `len()` channel pointers into the scratch's own Vecs, each
        // `block_size >= num_samples` elements long (`check_block`, just above; the Vecs are
        // never resized, so the pointers taken from them stay valid, and `prepare_*` refreshes
        // them anyway). The output Vecs are
        // distinct allocations from the inputs and from each other, and the
        // scratch is borrowed mutably for the whole call, so nothing else
        // touches them.
        let mut buffer = unsafe {
            AudioBuffer::from_raw(
                self.input_ptrs.len(),
                self.output_ptrs.len(),
                self.input_ptrs.as_ptr(),
                self.output_ptrs.as_mut_ptr(),
                num_samples,
            )
        };
        render.process(&mut buffer);
    }

    /// [`render_f32`](Self::render_f32) against `processReplacingF64`.
    ///
    /// Only called once `supports_f64` is known true: the vendor's
    /// `process_f64` zeroes the outputs when the plugin installed no f64
    /// entry point, so reaching it unconditionally would render silence
    /// rather than fall back.
    fn render_f64(&mut self, render: &mut RenderHandle, num_samples: usize) {
        use crate::buffer::AudioBuffer;

        self.check_block(num_samples);
        // SAFETY: as `render_f32`, against the f64 tables and Vecs.
        let mut buffer = unsafe {
            AudioBuffer::from_raw(
                self.input_ptrs_f64.len(),
                self.output_ptrs_f64.len(),
                self.input_ptrs_f64.as_ptr(),
                self.output_ptrs_f64.as_mut_ptr(),
                num_samples,
            )
        };
        render.process_f64(&mut buffer);
    }

    /// Pointers into the Vecs can become stale after any mutation — refresh
    /// them before each `vst_buffer.from_raw` call.
    ///
    /// In practice the Vec never reallocates (same shape, same capacity),
    /// so this is paranoia. Cheap paranoia.
    fn refresh_ptrs(&mut self) {
        for (ptr, ch) in self.input_ptrs.iter_mut().zip(&self.inputs) {
            *ptr = ch.as_ptr();
        }
        for (ptr, ch) in self.output_ptrs.iter_mut().zip(&mut self.outputs) {
            *ptr = ch.as_mut_ptr();
        }
    }

    /// [`refresh_ptrs`](Self::refresh_ptrs) for the f64 tables.
    fn refresh_ptrs_f64(&mut self) {
        for (ptr, ch) in self.input_ptrs_f64.iter_mut().zip(&self.inputs_f64) {
            *ptr = ch.as_ptr();
        }
        for (ptr, ch) in self.output_ptrs_f64.iter_mut().zip(&mut self.outputs_f64) {
            *ptr = ch.as_mut_ptr();
        }
    }
}

/// Converts the first `len` frames of each caller channel into the scratch row of the same
/// index: a row the caller has no channel for is silence, a caller channel past the rows is not
/// staged.
fn stage<T: Sample, S: Sample>(rows: &mut [Vec<T>], from: &Planes<'_, S>, len: usize) {
    for (c, row) in rows.iter_mut().enumerate() {
        match from.get_plane(c) {
            Some(plane) => convert_channel(plane, MonoMut::new(&mut row[..len]))
                .expect("the piece and the row are `len` frames"),
            None => row[..len].fill(T::SILENCE),
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
            convert_channel(Mono::new(&row[..len]), plane)
                .expect("the piece and the row are `len` frames");
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
        RenderScratch::new(layout, layout, 64).check_block(65);
    }

    /// A render at the block size is accepted.
    #[test]
    fn a_render_at_the_block_size_is_accepted() {
        let layout = ChannelLayout::from(2u8);
        RenderScratch::new(layout, layout, 64).check_block(64);
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
        let mut piece = outs.slice_mut(Samples(0)..Samples(4));
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
