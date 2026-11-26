use super::*;
use tutti_buffers::ChannelSet;
use tutti_buffers::FloatWidth;
use tutti_codec::Encoding;
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_codec::ToFile;
use tutti_measure::Loudness;

fn reading(lufs: f64, true_peak: f64) -> Loudness {
    Loudness {
        lufs: Db::new(lufs),
        true_peak: Db::new(true_peak),
        range: Db::new(0.0),
    }
}

/// Peak mode targets TRUE peak, not sample peak — the whole reason it goes
/// through the R128 meter rather than a `fold(max, abs)` over the planes.
#[test]
fn peak_moves_the_true_peak_to_the_target() {
    let m = reading(-20.0, -6.0);
    let gain = Reading::Peak {
        measured: PeakReading::Placed(m),
        target: Db::new(-1.0),
    }
    .gain();
    assert!(
        (gain.get() - 5.0).abs() < 1e-5,
        "−6 dBTP to a −1 dBTP target is +5 dB, got {gain:?}"
    );
}

/// Loudness mode defers to `gain_to`, ceiling included.
#[test]
fn lufs_respects_the_true_peak_ceiling() {
    // +6 dB would put a −2 dBTP peak at +4, which is 5 dB over a −1 ceiling.
    let m = reading(-20.0, -2.0);
    let gain = Reading::Lufs {
        loudness: m,
        target: Db::new(-14.0),
        ceiling: Db::new(-1.0),
    }
    .gain();
    assert!(
        (gain.get() - 1.0).abs() < 1e-5,
        "the ceiling must pull the loudness gain back, got {gain:?}"
    );
}

/// `Normalize::lufs` carries the −1.0 dBTP ceiling, so callers do not have
/// to know the number.
#[test]
fn lufs_defaults_to_a_minus_one_dbtp_ceiling() {
    assert_eq!(
        Normalize::lufs(Db::new(-14.0)),
        Normalize::Lufs {
            target: Db::new(-14.0),
            ceiling: Db::new(-1.0)
        }
    );
}

mod headroom {
    use super::*;
    use tutti_buffers::ChannelLayout;
    use tutti_codec::{Format, Pcm};

    const RATE: SampleRate = SampleRate::SR_48K;

    fn config(pcm: Pcm) -> FileTarget {
        FileTarget::new(
            Encoding::new(Format::Wav(pcm))
                .channels(ChannelSet::default_for(ChannelLayout::STEREO)),
            ToFile::new("unused.wav"),
        )
    }

    /// An integer type of `depth`, dithered with `dither` (none for `None`) and noise-shaped
    /// when `shaped`.
    fn int(depth: IntDepth, dither: Option<DitherShape>, shaped: bool) -> Pcm {
        let pcm = IntPcm::new(depth);
        let pcm = match dither {
            Some(shape) => pcm.dither(shape),
            None => pcm.no_dither(),
        };
        Pcm::Int(if shaped { pcm.noise_shaping() } else { pcm })
    }

    /// dBFS of `1 - lsbs / 2^(bits - 1)`: `lsbs` LSBs below full scale.
    fn below_full_scale(bits: u32, lsbs: f64) -> f64 {
        Db::from_amplitude(Amplitude::new(1.0 - lsbs / f64::from(1u32 << (bits - 1)))).get()
    }

    /// An integer format keeps one LSB plus the dither's amplitude below full
    /// scale: none for no dither, half an LSB for rectangular, one for
    /// triangular.
    #[test]
    fn an_integer_format_keeps_an_lsb_and_its_dither_below_full_scale() {
        for (depth, bits) in [(IntDepth::Int16, 16), (IntDepth::Int24, 24)] {
            for (dither, lsbs) in [
                (None, 1.0),
                (Some(DitherShape::Rectangular), 1.5),
                (Some(DitherShape::Triangular), 2.0),
            ] {
                let pcm = int(depth, dither, false);
                let got = headroom_ceiling(&config(pcm), RATE).expect("quantizes");
                let want = below_full_scale(bits, lsbs);
                assert!(
                    (got.get() - want).abs() < 1e-12,
                    "{pcm:?} {dither:?}: {got:?} vs {want}"
                );
            }
        }
    }

    /// With noise shaping the ceiling drops by the feedback: the dither's amplitude plus half
    /// an LSB (the largest recorded error) times the filter's `Σ|c_k|`, designed for the
    /// file's rate, which is the resample target when there is one.
    #[test]
    fn noise_shaping_lowers_the_ceiling_by_the_feedback_bound() {
        for (shape, error_lsbs) in [
            (None, 0.5),
            (Some(DitherShape::Rectangular), 1.0),
            (Some(DitherShape::Triangular), 1.5),
        ] {
            let plain = config(int(IntDepth::Int16, shape, false));
            let shaped = config(int(IntDepth::Int16, shape, true));
            let gain = NoiseShaping::new(RATE).feedback_gain().get();
            let lsbs = 0.5 + error_lsbs + gain * error_lsbs;
            let got = headroom_ceiling(&shaped, RATE).expect("quantizes");
            assert!(
                (got.get() - below_full_scale(16, lsbs)).abs() < 1e-12,
                "{shape:?}: {got:?}"
            );
            assert!(got < headroom_ceiling(&plain, RATE).unwrap(), "{shape:?}");
        }
        let resampled = FileTarget::new(
            Encoding::new(Format::Wav(int(
                IntDepth::Int16,
                Some(DitherShape::Triangular),
                true,
            )))
            .resample(tutti_codec::Resample::to(SampleRate::SR_96K)),
            ToFile::new("unused.wav"),
        );
        let want = NoiseShaping::new(SampleRate::SR_96K).feedback_gain().get();
        let lsbs = 2.0 + want * 1.5;
        let got = headroom_ceiling(&resampled, RATE).unwrap();
        assert!((got.get() - below_full_scale(16, lsbs)).abs() < 1e-12);
    }

    /// A target above the limit is pulled to it, one under it is left, the
    /// loudness target is never touched (only its ceiling), and a format that
    /// does not quantize has no limit.
    #[test]
    fn a_target_above_the_limit_is_pulled_to_it() {
        let int16 = config(int(IntDepth::Int16, Some(DitherShape::Triangular), false));
        let limit = Db::new(below_full_scale(16, 2.0));
        let near = |a: Db, b: Db| (a.get() - b.get()).abs() < 1e-12;

        let Normalize::Peak { target } = Normalize::peak(Db::new(0.0)).within(&int16, RATE) else {
            unreachable!()
        };
        assert!(near(target, limit), "{target:?}");
        assert_eq!(
            Normalize::peak(Db::new(-1.0)).within(&int16, RATE),
            Normalize::peak(Db::new(-1.0))
        );

        let Normalize::Lufs { target, ceiling } = Normalize::Lufs {
            target: Db::new(-14.0),
            ceiling: Db::new(0.5),
        }
        .within(&int16, RATE) else {
            unreachable!()
        };
        assert_eq!(target, Db::new(-14.0));
        assert!(near(ceiling, limit), "{ceiling:?}");
        assert_eq!(
            Normalize::lufs(Db::new(-14.0)).within(&int16, RATE),
            Normalize::lufs(Db::new(-14.0)),
            "the default -1 dBTP ceiling is under the limit"
        );

        let float = config(Pcm::Float(FloatWidth::F32));
        assert_eq!(
            Normalize::peak(Db::new(0.0)).within(&float, RATE),
            Normalize::peak(Db::new(0.0))
        );
    }
}

mod measurer {
    use super::*;
    use crate::control::{CancelToken, Progress};
    use crate::{Outcome, Rendered};
    use tutti_buffers::{Bed, ChannelTopology};
    use tutti_measure::{LoudnessConfig, measure_loudness};
    use tutti_units::{Amplitude, Hz, Samples};

    const RATE: SampleRate = SampleRate::SR_48K;

    /// A 1 kHz sine of `secs` seconds, as planes: amplitude `amp` on the left
    /// and half of it on the right, so a measurement that put a sample on the
    /// wrong channel reads something else.
    fn sine(secs: usize, amp: f64) -> Rendered {
        let plane = |amp: f64| -> Vec<f32> {
            tutti_fixtures::signal::sine(
                Hz::new(1_000.0),
                Amplitude::new(amp),
                RATE,
                Samples::new(secs * 48_000),
            )
        };
        Rendered::new(vec![plane(amp), plane(amp / 2.0)], RATE, 0).expect("equal planes")
    }

    /// `measure` of `rendered` under a control that reports to `report`,
    /// which may cancel `token`.
    fn measure(
        rendered: &Rendered,
        token: &CancelToken,
        report: &mut dyn FnMut(&Progress),
    ) -> Outcome<Loudness> {
        let mut control = Control::new(token).on_progress(report);
        control.weigh(Weights::Normalized);
        let outcome = Normalize::peak(Db::new(-1.0))
            .measure(
                rendered,
                &Bed::Stereo.topology(),
                Samples::new(0),
                rendered.frames(),
                &mut control,
            )
            .expect("a measurable sine");
        match outcome {
            Outcome::Finished(reading) => {
                Outcome::Finished(reading.loudness().expect("stereo is placed"))
            }
            Outcome::Cancelled => Outcome::Cancelled,
        }
    }

    /// The chunk is one second of audio, at least a frame.
    #[test]
    fn a_chunk_is_at_most_a_second() {
        assert_eq!(chunk_frames(RATE), Samples::new(48_000));
        assert_eq!(chunk_frames(SampleRate::SR_44K1), Samples::new(44_100));
        assert_eq!(chunk_frames(SampleRate::of(0.2)), Samples::new(1));
        // A fractional rate rounds up: the second is held whole.
        assert_eq!(chunk_frames(SampleRate::of(44_100.5)), Samples::new(44_101));
    }

    /// The chunked reading of a known sine is its analytic loudness, and the
    /// one-shot reading of the same buffer (`measure_loudness`, which this
    /// crate used before) is the same figure to within the measurement's own
    /// stated tolerance (0.01 LU, `tutti-measure`'s streaming test).
    ///
    /// Derived from BS.1770, not from the implementation: a sine of amplitude
    /// a on the left and a/2 on the right reads `-0.691 + 10 log10(G * 0.625 *
    /// a^2)` LUFS, and K-weighting's gain G at 997 Hz is +0.691 dB, which
    /// is what the constant cancels, so it reads `20 log10 a` plus
    /// `10 log10 0.625` (the mean squares, a^2/2 and a^2/8, sum to 0.625 a^2): -22.04 LUFS at a = 0.1. Its true peak is the larger
    /// amplitude, -20 dBTP. The tolerance on both (0.1) covers the gain at
    /// 1 kHz not being the one at 997 Hz.
    #[test]
    fn the_chunked_reading_is_the_analytic_one_and_the_one_shot_one() {
        let rendered = sine(5, 0.1);
        let chunked = measure(&rendered, &CancelToken::new(), &mut |_| {})
            .into_finished()
            .expect("not cancelled");

        let analytic_lufs =
            Db::from_amplitude(Amplitude::new(0.1)).get() + Db::from_power(0.625).get();
        assert!(
            (chunked.lufs.get() - analytic_lufs).abs() < 0.1,
            "{:?} vs {analytic_lufs}",
            chunked.lufs
        );
        assert!(
            (chunked.true_peak.get() - -20.0).abs() < 0.1,
            "{:?}",
            chunked.true_peak
        );

        let one_shot = measure_loudness(
            &LoudnessConfig::new(Bed::Stereo.topology()),
            Interleaved::new(&rendered.interleaved(), rendered.layout()).unwrap(),
            RATE,
        )
        .expect("meterable");
        assert!(
            (chunked.lufs.get() - one_shot.lufs.get()).abs() < 0.01,
            "{:?} vs {:?}",
            chunked.lufs,
            one_shot.lufs
        );
        assert!((chunked.true_peak.get() - one_shot.true_peak.get()).abs() < 0.01);
        assert!((chunked.range.get() - one_shot.range.get()).abs() < 0.01);
    }

    /// Where the stream is cut does not move the reading: 777-frame spans
    /// (not a divisor of anything here) read what one-second ones do.
    #[test]
    fn the_cut_does_not_move_the_reading() {
        let rendered = sine(3, 0.25);
        let whole = measure(&rendered, &CancelToken::new(), &mut |_| {})
            .into_finished()
            .expect("not cancelled");
        let mut fine = Measurer::new(
            RATE,
            &Bed::Stereo.topology(),
            Normalize::peak(Db::new(-1.0)),
        )
        .expect("meterable");
        let frames = rendered.frames().get();
        let mut at = 0;
        while at < frames {
            let end = (at + 777).min(frames);
            fine.feed(rendered.planes(), at..end).unwrap();
            at = end;
        }
        let fine = fine.finish().loudness().expect("stereo is placed");
        assert!((fine.lufs.get() - whole.lufs.get()).abs() < 0.01);
        assert!((fine.true_peak.get() - whole.true_peak.get()).abs() < 0.01);
    }

    /// A cancel between chunks stops the measurement there: the callback
    /// cancels on the second chunk's reading, and no third chunk is fed.
    #[test]
    fn a_cancel_between_chunks_stops_the_measurement() {
        let rendered = sine(5, 0.1);
        let token = CancelToken::new();
        let mut reported = 0;
        let outcome = measure(&rendered, &token, &mut |p| {
            reported += 1;
            if p.frames().is_some_and(|(done, _)| done.get() >= 2 * 48_000) {
                token.cancel();
            }
        });
        assert_eq!(outcome, Outcome::Cancelled);
        // The entry is `begin`'s, not `measure`'s; chunks 1 and 2 are the only
        // readings that are, and the cadence delivers every 4096 frames, so a
        // chunk is one reading.
        assert_eq!(reported, 2, "two chunks were measured, then it stopped");
    }

    /// A topology of another width than the audio is a named error before any
    /// frame is fed, not a buffer error from inside the meter.
    #[test]
    fn a_topology_of_another_width_is_refused_by_name() {
        let rendered = sine(1, 0.1);
        let err = Normalize::peak(Db::new(-1.0))
            .gain_for_rendered(&rendered, &Bed::Surround51.topology())
            .expect_err("stereo audio is not 5.1");
        assert!(
            matches!(
                err,
                crate::RenderError::TopologyWidth { topology, rendered: r }
                    if topology == Bed::Surround51.width() && r == rendered.layout()
            ),
            "{err:?}"
        );
    }

    /// Lufs needs the placement and says so; a peak does not.
    #[test]
    fn lufs_refuses_an_unplaced_topology_and_peak_does_not() {
        let rendered = sine(2, 0.1);
        let unplaced = ChannelTopology::unassigned(rendered.layout()).unwrap();
        let err = Normalize::lufs(Db::new(-23.0))
            .gain_for_rendered(&rendered, &unplaced)
            .expect_err("two unassigned channels have no angles");
        assert!(
            matches!(
                err,
                crate::RenderError::Unmeasurable {
                    reason: Unmeasured::Placement,
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(
            Normalize::peak(Db::new(-1.0))
                .gain_for_rendered(&rendered, &unplaced)
                .is_ok()
        );
    }
}
