use super::*;
use tutti_dsp::DitherShape;

/// Each depth maps 0 to 0, +1.0 to `MAX` (it clips), -1.0 to `MIN` (exact),
/// and clips anything beyond them to the rail: the rule in `Sample`.
#[test]
fn quantize_clips_to_the_rails_at_every_depth() {
    for (depth, bits) in [(IntDepth::Int16, 16u32), (IntDepth::Int24, 24)] {
        let max = (1i32 << (bits - 1)) - 1;
        assert_eq!(depth.quantize(0.0f32), 0, "{depth:?}");
        assert_eq!(depth.quantize(1.0f32), max, "{depth:?}");
        assert_eq!(depth.quantize(-1.0f32), -max - 1, "{depth:?}");
        assert_eq!(depth.quantize(1.5f32), max, "{depth:?}: above the rail");
        assert_eq!(depth.quantize(-1.5f32), -max - 1, "{depth:?}: below");
        assert_eq!(depth.quantize(f32::NAN), 0, "{depth:?}: NaN is silence");
    }
}

/// Rounding half to even at the scale `2^(n-1)`: 0.7 x 32768 = 22937.6 is
/// 22938, and the half-way 1.5 / 32768 is the even 2.
#[test]
fn quantization_rounds_half_to_even() {
    assert_eq!(IntDepth::Int16.quantize(0.7f64), 22938);
    assert_eq!(IntDepth::Int16.quantize(-0.7f64), -22938);
    assert_eq!(IntDepth::Int16.quantize(0.5 / 32768.0f64), 0);
    assert_eq!(IntDepth::Int16.quantize(1.5 / 32768.0f64), 2);
}

/// An integer in the depth's own format passes through unchanged.
#[test]
fn an_integer_of_the_depths_own_format_is_exact() {
    for v in [i16::MIN, -1, 0, 1, i16::MAX] {
        assert_eq!(IntDepth::Int16.quantize(v), i32::from(v));
    }
    assert_eq!(IntDepth::Int24.quantize(I24::of(-8_388_608)), -8_388_608);
}

#[test]
fn each_width_names_its_bits_and_integer_type() {
    let int = |depth| Pcm::Int(IntPcm::new(depth));
    assert_eq!(
        [
            int(IntDepth::Int16).bits(),
            int(IntDepth::Int24).bits(),
            Pcm::Float(FloatWidth::F32).bits(),
            Pcm::Float(FloatWidth::F64).bits()
        ],
        [16, 24, 32, 64]
    );
    assert_eq!(
        int(IntDepth::Int16).int_pcm(),
        Some(IntPcm::new(IntDepth::Int16))
    );
    assert_eq!(Pcm::Float(FloatWidth::F32).int_pcm(), None);
    assert_eq!(Pcm::Float(FloatWidth::F64).int_pcm(), None);
}

/// An integer file is dithered with triangular noise and not shaped unless told otherwise, and
/// the default is 24 bits.
#[test]
fn an_integer_file_defaults_to_triangular_dither_and_no_shaping() {
    let pcm = IntPcm::new(IntDepth::Int16);
    assert_eq!(pcm.dither_shape(), Some(DitherShape::Triangular));
    assert!(!pcm.is_noise_shaped());
    assert_eq!(Pcm::default(), Pcm::Int(IntPcm::new(IntDepth::Int24)));
    assert_eq!(pcm.no_dither().dither_shape(), None);
    assert_eq!(
        pcm.dither(DitherShape::Rectangular).dither_shape(),
        Some(DitherShape::Rectangular)
    );
    assert!(pcm.noise_shaping().is_noise_shaped());
}

/// One LSB is the format's step, `2^-(bits - 1)`: 16 bit is 2^-15, 24 bit is
/// 2^-23.
#[test]
fn the_lsb_is_one_step_of_the_depth() {
    assert_eq!(IntDepth::Int16.lsb().get(), 1.0 / 32768.0);
    assert_eq!(IntDepth::Int24.lsb().get(), 1.0 / 8_388_608.0);
}
