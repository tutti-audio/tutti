//! `Sample`'s format constants: width, kind and the run-time `SampleFormat`.

use tutti_buffers::{I24, Sample, SampleFormat};

/// What a format says about itself, through the trait alone.
fn describe<S: Sample>() -> (u32, bool, SampleFormat) {
    (S::BITS, S::IS_FLOAT, S::FORMAT)
}

#[test]
fn each_format_states_its_width_kind_and_variant() {
    assert_eq!(describe::<i8>(), (8, false, SampleFormat::I8));
    assert_eq!(describe::<i16>(), (16, false, SampleFormat::I16));
    assert_eq!(describe::<I24>(), (24, false, SampleFormat::I24));
    assert_eq!(describe::<i32>(), (32, false, SampleFormat::I32));
    assert_eq!(describe::<f32>(), (32, true, SampleFormat::F32));
    assert_eq!(describe::<f64>(), (64, true, SampleFormat::F64));
}

/// The bits a value occupies are the bytes it is stored in: an integer
/// format's width is eight bits a byte, `I24` included.
#[test]
fn the_bits_are_the_storage_width() {
    fn stored<S: Sample>() -> (usize, usize) {
        (S::BITS as usize, 8 * size_of::<S>())
    }
    for (bits, bytes) in [
        stored::<i8>(),
        stored::<i16>(),
        stored::<I24>(),
        stored::<i32>(),
        stored::<f32>(),
        stored::<f64>(),
    ] {
        assert_eq!(bits, bytes);
    }
}

/// Silence is the default value and sorts as zero in every format.
#[test]
fn silence_is_the_default_in_every_format() {
    fn zero<S: Sample>() {
        assert_eq!(S::default(), S::SILENCE);
        assert!(S::SILENCE <= S::default() && S::SILENCE >= S::default());
        assert_eq!(S::SILENCE.to_f64(), 0.0);
    }
    zero::<i8>();
    zero::<i16>();
    zero::<I24>();
    zero::<i32>();
    zero::<f32>();
    zero::<f64>();
}

/// The formats a run-time dispatch sees are all distinct.
#[test]
fn the_variants_are_distinct() {
    let all = [
        <i8 as Sample>::FORMAT,
        <i16 as Sample>::FORMAT,
        <I24 as Sample>::FORMAT,
        <i32 as Sample>::FORMAT,
        <f32 as Sample>::FORMAT,
        <f64 as Sample>::FORMAT,
    ];
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

/// A float width and its sample format are the same width: the round trip holds
/// for both arms, `Float::WIDTH` agrees with `Sample::FORMAT`, and the bytes are
/// the type's size.
#[test]
fn a_float_width_round_trips_through_its_sample_format() {
    use tutti_buffers::{Float, FloatWidth};
    for width in [FloatWidth::F32, FloatWidth::F64] {
        assert_eq!(FloatWidth::try_from(SampleFormat::from(width)), Ok(width));
    }
    assert_eq!(SampleFormat::from(<f32 as Float>::WIDTH), f32::FORMAT);
    assert_eq!(SampleFormat::from(<f64 as Float>::WIDTH), f64::FORMAT);
    assert_eq!(FloatWidth::F32.bytes(), 4);
    assert_eq!(FloatWidth::F64.bytes(), 8);
}

/// An integer format is refused, and the error names it.
#[test]
fn an_integer_format_is_no_float_width() {
    use tutti_buffers::{FloatWidth, NotAFloat};
    for format in [
        SampleFormat::I8,
        SampleFormat::I16,
        SampleFormat::I24,
        SampleFormat::I32,
    ] {
        assert_eq!(FloatWidth::try_from(format), Err(NotAFloat(format)));
    }
}
