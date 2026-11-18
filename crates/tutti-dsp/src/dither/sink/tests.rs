use super::*;
use tutti_buffers::I24;

/// Collects what reaches it, and refuses on demand.
#[derive(Default)]
struct Collect<S> {
    got: Vec<S>,
    writes: usize,
    refuse_after: Option<usize>,
}

impl<S: Sample> Sink for Collect<S> {
    type Sample = S;
    type Error = std::io::Error;
    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }
    fn write(&mut self, block: Interleaved<'_, S>) -> Result<(), SinkFailed> {
        if self.refuse_after.is_some_and(|n| self.writes >= n) {
            return Err(SinkFailed);
        }
        self.writes += 1;
        self.got.extend_from_slice(block.samples());
        Ok(())
    }
    fn finalize(self) -> Result<(), Self::Error> {
        Err(std::io::Error::other("inner finalize"))
    }
}

fn stereo<F: Float>(samples: &[F]) -> Interleaved<'_, F> {
    Interleaved::new(samples, ChannelLayout::STEREO).unwrap()
}

fn signal(n: usize) -> Vec<f32> {
    (0..n).map(|i| ((i % 97) as f32 / 97.0) - 0.5).collect()
}

fn tpdf_i16() -> DitherSink<Collect<i16>, f32> {
    DitherSink::new(Collect::default(), Some(DitherShape::Triangular))
}

/// One LSB is `2^-(bits - 1)`, from the format's step: 16 bit is 2^-15, 24 bit
/// is 2^-23, and a float format has none.
#[test]
fn the_lsb_is_the_step_of_the_integer_format() {
    assert_eq!(lsb_of::<i16>().0, 1.0 / 32768.0);
    assert_eq!(lsb_of::<I24>().0, 1.0 / 8_388_608.0);
    assert_eq!(lsb_of::<f32>(), Amplitude::SILENT);
}

/// A full-scale square, undithered into i16: `+1.0` clips to `MAX` and `-1.0`
/// is `MIN`, as the rule in `Sample` says, and the stage counts exactly the
/// values at or above `+1.0`, across chunks.
#[test]
fn a_full_scale_square_into_i16_is_max_or_min_and_counts_its_positive_peaks() {
    let frames = 3 * 1024 + 17;
    let square: Vec<f32> = (0..frames * 2)
        .map(|i| if (i / 2) % 8 < 4 { 1.0 } else { -1.0 })
        .collect();
    let positive = square.iter().filter(|&&s| s >= 1.0).count() as u64;
    let mut sink = DitherSink::<_, f32>::new(Collect::<i16>::default(), None);
    sink.write(stereo(&square)).unwrap();
    assert_eq!(sink.clipped(), positive);
    let got = sink.into_inner().got;
    assert_eq!(got.len(), square.len());
    for (out, s) in got.iter().zip(&square) {
        assert_eq!(*out, if *s >= 1.0 { 32767 } else { -32768 });
    }
}

/// `32767/32768` is the top step, not a clip; NaN reads as 0 and is not a clip.
#[test]
fn the_top_step_and_a_nan_are_not_clips() {
    let block = [32767.0 / 32768.0, f32::NAN, 0.0, -1.0];
    let mut sink = DitherSink::<_, f32>::new(Collect::<i16>::default(), None);
    sink.write(stereo(&block)).unwrap();
    assert_eq!(sink.clipped(), 0);
    assert_eq!(sink.into_inner().got, [32767, 0, 0, -32768]);
}

/// TPDF is `r1 - r2`, within one LSB, so a silent block rounds to -1, 0 or 1
/// and never strays past 2 LSB; its mean over many blocks is 0. The rounded
/// noise has the TPDF variance `lsb^2/6` plus the rounding's `lsb^2/12`, which
/// is `1/4` in LSBs squared, so the mean of `n` samples is within six standard
/// deviations, `6 * sqrt(1 / (4 n))`.
#[test]
fn tpdf_on_silence_stays_within_two_lsb_and_averages_to_zero() {
    let silence = vec![0.0f32; 4096 * 2];
    let mut sink = tpdf_i16();
    for _ in 0..64 {
        sink.write(stereo(&silence)).unwrap();
    }
    let got = sink.into_inner().got;
    assert!(got.iter().all(|v| v.abs() <= 2), "past 2 LSB");
    assert!(got.iter().any(|&v| v != 0), "the noise is audible");
    let n = got.len() as f64;
    let mean = got.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let bound = 6.0 * (0.25 / n).sqrt();
    assert!(mean.abs() <= bound, "mean {mean} past {bound}");
}

/// Frames in are frames out: a block longer than the scratch is split but
/// loses nothing.
#[test]
fn frames_in_are_frames_out() {
    for frames in [0usize, 1, 1023, 1024, 1025, 5000] {
        let mut sink = tpdf_i16();
        sink.write(stereo(&signal(frames * 2))).unwrap();
        assert_eq!(sink.into_inner().got.len(), frames * 2, "frames {frames}");
    }
}

/// The noise does not depend on how the writes are cut: one block of N equals
/// pieces of any size, across chunk boundaries too.
#[test]
fn the_noise_continues_across_writes() {
    let input = signal(3000 * 2);
    let mut whole = tpdf_i16();
    whole.write(stereo(&input)).unwrap();
    let mut cut = tpdf_i16();
    for piece in input.chunks(2 * 700 + 2) {
        cut.write(stereo(piece)).unwrap();
    }
    assert_eq!(whole.into_inner().got, cut.into_inner().got);
}

/// An f64 signal runs the same stage into i24, whose full-scale step is 2^-23.
#[test]
fn an_f64_signal_converts_into_i24_by_the_rule() {
    let block = [0.5f64, -0.5, 1.0, -1.0];
    let mut sink = DitherSink::<_, f64>::new(Collect::<I24>::default(), None);
    sink.write(stereo(&block)).unwrap();
    assert_eq!(sink.clipped(), 1);
    let got: Vec<i32> = sink.into_inner().got.iter().map(|v| v.get()).collect();
    assert_eq!(got, [4_194_304, -4_194_304, 8_388_607, -8_388_608]);
}

/// The inner sink's refusal passes through, and so does its finalize.
#[test]
fn the_inner_sinks_errors_pass_through() {
    let inner = Collect::<i16> {
        refuse_after: Some(0),
        ..Collect::default()
    };
    let mut sink = DitherSink::<_, f32>::new(inner, Some(DitherShape::Triangular));
    assert_eq!(sink.write(stereo(&signal(8))), Err(SinkFailed));
    let mut open = DitherSink::<_, f32>::new(Collect::<i16>::default(), None);
    assert_eq!(open.write(stereo(&signal(8))), Ok(()));
    let err = open.finalize().unwrap_err();
    assert_eq!(err.to_string(), "inner finalize");
}

/// A view of another width than the inner sink's fails the write, dithered or
/// not, and nothing reaches the inner sink.
#[test]
fn a_view_of_the_wrong_width_fails_the_write() {
    let mono = Interleaved::new(&[0.0f32; 8], ChannelLayout::MONO).unwrap();
    for shape in [Some(DitherShape::Triangular), None] {
        let mut sink = DitherSink::<_, f32>::new(Collect::<i16>::default(), shape);
        assert_eq!(sink.write(mono), Err(SinkFailed));
        assert_eq!(sink.into_inner().writes, 0);
    }
}
