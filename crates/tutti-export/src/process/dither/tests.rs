use super::*;
use tutti_buffers::ChannelLayout;
use tutti_codec::{Format, Pcm, Quality};

fn config(dither: Option<DitherShape>, format: Format) -> FileTarget {
    let target = FileTarget::new("unused.wav", format, ChannelLayout::STEREO);
    match dither {
        Some(shape) => target.dither(shape),
        None => target.no_dither(),
    }
}

fn wav(pcm: Pcm) -> Format {
    Format::Wav(pcm)
}

/// At a float width there is nothing to add: no shape, so nothing dithers.
#[test]
fn a_float_width_is_never_dithered() {
    for pcm in [Pcm::Float32, Pcm::Float64] {
        assert_eq!(
            dither_shape(&config(Some(DitherShape::Triangular), wav(pcm))),
            None
        );
    }
}

/// An Ogg export with dither on adds nothing.
#[test]
fn ogg_is_never_dithered() {
    let cfg = config(
        Some(DitherShape::Triangular),
        Format::Vorbis {
            quality: Quality::default(),
        },
    );
    assert_eq!(dither_shape(&cfg), None);
}

#[test]
fn off_has_no_shape() {
    assert_eq!(dither_shape(&config(None, wav(Pcm::Int16))), None);
}

/// An integer width takes the shape the option names.
#[test]
fn an_integer_width_takes_the_shape_of_the_option() {
    for shape in [DitherShape::Rectangular, DitherShape::Triangular] {
        for pcm in [Pcm::Int16, Pcm::Int24] {
            assert_eq!(dither_shape(&config(Some(shape), wav(pcm))), Some(shape));
        }
    }
}
