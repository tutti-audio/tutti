// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use crate::render::Render;
use tutti_buffers::{Float, I24, Sample};
use tutti_sf2::{LoopMode, Pcm24, SamplePool};
use tutti_units::{ReadPosition, SampleRate, Semitones};

// The read position is a fixed-point number for speed: an `i64` whose lower 24 bits are
// the fraction and the rest the whole frames. A fixed-point variable is suffixed "_fp".

/// A sample pool read as integers of its own width, a point at a time as data of the
/// reader's float: the integer-to-float edge, `Sample::convert_to` from the stored format.
trait Points: Copy {
    fn at<F: Float>(self, index: usize) -> F;
}

impl Points for &[i16] {
    fn at<F: Float>(self, index: usize) -> F {
        self[index].convert_to()
    }
}

impl Points for Pcm24<'_> {
    fn at<F: Float>(self, index: usize) -> F {
        // The high word shifted over a byte is within the 24-bit range, so `of` does not panic.
        let word = i32::from(self.high()[index]) << 8 | i32::from(self.low()[index]);
        I24::of(word).convert_to()
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Oscillator {
    synthesizer_sample_rate: i32,

    loop_mode: LoopMode,
    start: i32,
    end: i32,
    start_loop: i32,
    end_loop: i32,
    root_key: i32,

    tune: Semitones,
    /// Semitones of pitch change a semitone of key moves (generator 56 over 100).
    pitch_change_scale: f64,
    sample_rate_ratio: f64,

    looping: bool,

    position_fp: i64,
}

impl Oscillator {
    const FRAC_BITS: i32 = 24;
    const FRAC_UNIT: i64 = 1_i64 << Oscillator::FRAC_BITS;

    pub(crate) fn new(settings: &Render) -> Self {
        Self {
            synthesizer_sample_rate: settings.hz(),
            loop_mode: LoopMode::NoLoop,
            start: 0,
            end: 0,
            start_loop: 0,
            end_loop: 0,
            root_key: 0,
            tune: Semitones::new(0.0),
            pitch_change_scale: 0.0,
            sample_rate_ratio: 0.0,
            looping: false,
            position_fp: 0,
        }
    }

    pub(crate) fn start(
        &mut self,
        loop_mode: LoopMode,
        sample_rate: SampleRate,
        start: i32,
        end: i32,
        start_loop: i32,
        end_loop: i32,
        root_key: i32,
        coarse_tune: i32,
        fine_tune: i32,
        scale_tuning: i32,
    ) {
        self.loop_mode = loop_mode;
        self.start = start;
        self.end = end;
        self.start_loop = start_loop;
        self.end_loop = end_loop;
        self.root_key = root_key;

        self.tune = Semitones::new(f64::from(coarse_tune) + 0.01 * f64::from(fine_tune));
        self.pitch_change_scale = 0.01 * f64::from(scale_tuning);
        // Both rates are whole hertz: a header states its rate as a `u32`.
        self.sample_rate_ratio = sample_rate.get() / f64::from(self.synthesizer_sample_rate);
        self.looping = self.loop_mode != LoopMode::NoLoop;
        self.position_fp = (start as i64) << Oscillator::FRAC_BITS;
    }

    pub(crate) fn release(&mut self) {
        if self.loop_mode == LoopMode::LoopUntilNoteOff {
            self.looping = false;
        }
    }

    /// Fills `block` from the pool at the pitch `pitch` (a key number in semitones): the
    /// position advances by the sample's rate over the synthesizer's and the pitch's ratio
    /// to the root key, in fixed point, so no float accumulates.
    pub(crate) fn process<F: Float>(
        &mut self,
        data: SamplePool<'_>,
        block: &mut [F],
        pitch: Semitones,
    ) -> bool {
        let pitch_change = (pitch - Semitones::new(f64::from(self.root_key)))
            * self.pitch_change_scale
            + self.tune;
        let pitch_ratio = self.sample_rate_ratio * pitch_change.to_pitch_ratio();
        // The pool's width is matched once per block, not per point.
        match data {
            SamplePool::Pcm16(points) => self.fill_block(points, block, pitch_ratio),
            SamplePool::Pcm24(points) => self.fill_block(points, block, pitch_ratio),
        }
    }

    fn fill_block<P: Points, F: Float>(
        &mut self,
        data: P,
        block: &mut [F],
        pitch_ratio: f64,
    ) -> bool {
        let pitch_ratio_fp = (Oscillator::FRAC_UNIT as f64 * pitch_ratio) as i64;

        if self.looping {
            self.fill_block_continuous(data, block, pitch_ratio_fp)
        } else {
            self.fill_block_no_loop(data, block, pitch_ratio_fp)
        }
    }

    /// The linear interpolation between the points at `index` and `index2`, at the
    /// position's fraction: the fraction is the position's top bits as data of the
    /// float, and the points are the pool's own values.
    #[inline]
    fn interpolate<P: Points, F: Float>(&self, data: P, index: usize, index2: usize) -> F {
        let fraction_fp = (self.position_fp & (Oscillator::FRAC_UNIT - 1)) as u64;
        let (_, a) = F::split_position(ReadPosition::from_count(
            fraction_fp << (ReadPosition::FRACTION_BITS as i32 - Oscillator::FRAC_BITS),
        ));
        let x1: F = data.at(index);
        let x2: F = data.at(index2);
        x1 + a * (x2 - x1)
    }

    fn fill_block_no_loop<P: Points, F: Float>(
        &mut self,
        data: P,
        block: &mut [F],
        pitch_ratio_fp: i64,
    ) -> bool {
        for t in 0..block.len() {
            let index = (self.position_fp >> Oscillator::FRAC_BITS) as usize;
            if index >= self.end as usize {
                if t > 0 {
                    block[t..].fill(F::ZERO);
                    return true;
                } else {
                    return false;
                }
            }

            block[t] = self.interpolate(data, index, index + 1);

            self.position_fp += pitch_ratio_fp;
        }

        true
    }

    fn fill_block_continuous<P: Points, F: Float>(
        &mut self,
        data: P,
        block: &mut [F],
        pitch_ratio_fp: i64,
    ) -> bool {
        let end_loop_fp = (self.end_loop as i64) << Oscillator::FRAC_BITS;
        let loop_length = (self.end_loop - self.start_loop) as i64;
        let loop_length_fp = loop_length << Oscillator::FRAC_BITS;

        for sample in block.iter_mut() {
            if self.position_fp >= end_loop_fp {
                self.position_fp -= loop_length_fp;
            }

            let index1 = (self.position_fp >> Oscillator::FRAC_BITS) as usize;
            let mut index2 = index1 + 1;
            if index2 >= self.end_loop as usize {
                index2 -= loop_length as usize;
            }

            *sample = self.interpolate(data, index1, index2);

            self.position_fp += pitch_ratio_fp;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_fixtures::sf2::{Inst, Preset, bytes, with_pool};
    use tutti_sf2::SoundFont;

    /// A font of 2100 points: `smpl` words that walk the 16-bit range and
    /// `sm24` bytes that walk the byte range.
    fn font(low: bool) -> (SoundFont, Vec<i32>) {
        let high: Vec<i16> = (0..2100).map(|i| (i * 31 - 32_000) as i16).collect();
        let low_bytes: Vec<u8> = (0..2100).map(|i| (i * 7 % 256) as u8).collect();
        let smpl: Vec<u8> = high.iter().flat_map(|p| p.to_le_bytes()).collect();
        let file = bytes(
            &[Preset {
                name: "p",
                bank: 0,
                program: 0,
                zones: vec![(vec![], 0)],
            }],
            &[Inst {
                name: "i",
                zones: vec![vec![]],
            }],
        );
        let file = with_pool(&file, (2, 4), Some(&smpl), low.then_some(&low_bytes[..]));
        let font = SoundFont::open(file).unwrap();
        let points = high
            .iter()
            .zip(&low_bytes)
            .map(|(&h, &l)| {
                if low {
                    i32::from(h) * 256 + i32::from(l)
                } else {
                    i32::from(h)
                }
            })
            .collect();
        (font, points)
    }

    /// The first `count` frames of a no-loop read at a pitch ratio of 1: the
    /// position steps by exactly one point from point 0, so every read sits on
    /// a point and the interpolation has no fraction.
    fn read<F: Float>(pool: SamplePool<'_>, count: usize) -> Vec<F> {
        let rate = SampleRate::SR_44K1;
        let mut oscillator = Oscillator::new(&Render::new(rate));
        oscillator.start(LoopMode::NoLoop, rate, 0, 2000, 0, 0, 60, 0, 0, 100);
        let mut block = vec![F::ZERO; count];
        assert!(oscillator.process(pool, &mut block, Semitones::new(60.0)));
        block
    }

    /// A 24-bit point is `(smpl << 8) | sm24` over 2^23: on a point of the pool
    /// the read is that figure exactly, in either float (a 24-bit integer has 24
    /// significant bits, which `f32` holds, and 2^-23 is a power of two).
    /// Spec: sf2.sdta.sm24.low-byte
    #[test]
    fn a_24_bit_point_reads_as_its_value_over_2_to_the_23() {
        let (font, points) = font(true);
        assert_eq!(font.bits_per_sample(), 24);
        for (t, &sample) in read::<f32>(font.sample_pool(), 1500).iter().enumerate() {
            assert_eq!(
                f64::from(sample),
                f64::from(points[t]) / 8_388_608.0,
                "f32 frame {t}"
            );
        }
        for (t, &sample) in read::<f64>(font.sample_pool(), 1500).iter().enumerate() {
            assert_eq!(sample, f64::from(points[t]) / 8_388_608.0, "f64 frame {t}");
        }
    }

    /// The 16-bit pool reads a point as its value over 2^15, in either float.
    #[test]
    fn a_16_bit_point_reads_as_its_value_over_2_to_the_15() {
        let (font, points) = font(false);
        assert_eq!(font.bits_per_sample(), 16);
        for (t, &sample) in read::<f32>(font.sample_pool(), 1500).iter().enumerate() {
            assert_eq!(
                f64::from(sample),
                f64::from(points[t]) / 32_768.0,
                "f32 frame {t}"
            );
        }
        for (t, &sample) in read::<f64>(font.sample_pool(), 1500).iter().enumerate() {
            assert_eq!(sample, f64::from(points[t]) / 32_768.0, "f64 frame {t}");
        }
    }

    /// A read over 10 s at 192 kHz at a pitch whose ratio is irrational: the position is an
    /// integer count of `2^-24` frames advanced by the ratio truncated to that grid, so it
    /// is within `N 2^-24` frames of `N r` (`N` the frames, `r` the ratio) however long it
    /// runs, where an `f32` position at 1.92 million frames steps by 1/8 frame.
    /// The pool's ramp (`31 i - 32000` over `2^15` at point `i`) is linear, so the
    /// `f32` sample at a position `p` of the grid is `(31 p - 32000) / 2^15` to the
    /// interpolation's rounding: the difference `x2 - x1` rounds (relative `u =
    /// 2^-24`), the product with the fraction rounds, and the sum rounds, `3 u` of a
    /// full-scale value.
    #[test]
    fn the_fixed_point_position_after_ten_seconds_is_within_its_truncation_of_the_phase() {
        let rate = SampleRate::SR_192K;
        let (pool_font, _) = font(false);
        let pool = pool_font.sample_pool();
        let mut oscillator = Oscillator::new(&Render::new(rate));
        // Looped over 100..1900, so the position stays in the 2100-point pool; the pool's
        // sample rate is the render's, so the ratio is the pitch's alone.
        oscillator.start(
            LoopMode::Continuous,
            rate,
            0,
            2000,
            100,
            1900,
            60,
            0,
            0,
            100,
        );
        let pitch = Semitones::new(67.0);
        let ratio = Semitones::new(7.0).to_pitch_ratio();
        let frames = 1_920_000_usize;
        let mut block = [0_f32; 8];
        for _ in 0..frames / block.len() {
            assert!(oscillator.process(pool, &mut block, pitch));
        }
        let position = oscillator.position_fp as f64 / Oscillator::FRAC_UNIT as f64;
        // The loop wraps by whole frames, so the position is `N r` folded into 100..1900.
        let want = 100.0 + (frames as f64 * ratio - 100.0).rem_euclid(1800.0);
        let drift = frames as f64 / Oscillator::FRAC_UNIT as f64;
        assert!(
            (position - want).abs() <= drift,
            "position {position} against {want}, drift bound {drift}"
        );
        // The last sample was read one step (the ratio on the `2^-24` grid) before `position`.
        let step = (Oscillator::FRAC_UNIT as f64 * ratio) as i64;
        let read = (oscillator.position_fp - step) as f64 / Oscillator::FRAC_UNIT as f64;
        assert!(
            read > 100.0 && read < 1898.0,
            "the last read {read} is not near a wrap"
        );
        let want_sample = (31.0 * read - 32_000.0) / 32_768.0;
        let bound = 3.0 * f64::from(f32::EPSILON) / 2.0;
        let got = f64::from(block[7]);
        eprintln!(
            "position {position}, want {want}, sample {got}, want {want_sample}, bound {bound:e}"
        );
        assert!(
            (got - want_sample).abs() <= bound,
            "{got} against {want_sample}"
        );
    }
}
