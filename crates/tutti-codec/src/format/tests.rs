use super::*;

/// An extension names its format with the defaults, case and `aif` aside; an
/// unknown one is named lowercased, and a missing one is `(none)`.
#[test]
fn extensions_name_formats_and_refuse_the_rest() {
    let name = |r: Result<Format>| match r {
        Err(EncodeError::UnknownExtension { extension }) => extension,
        other => panic!("expected UnknownExtension, got {other:?}"),
    };
    assert_eq!(name(Format::from_path("x.XYZ")), "xyz");
    assert_eq!(name(Format::from_path("x")), "(none)");
    #[cfg(feature = "encode-wav")]
    assert_eq!(
        Format::from_path("a/b.WAV").unwrap(),
        Format::Wav(Pcm::Int24)
    );
    #[cfg(feature = "encode-aiff")]
    assert_eq!(
        Format::from_extension("aif").unwrap(),
        Format::Aiff(Pcm::Int24)
    );
    #[cfg(feature = "encode-flac")]
    assert_eq!(
        Format::from_extension("flac").unwrap(),
        Format::Flac {
            depth: IntDepth::Int24,
            level: FlacLevel::Standard
        }
    );
}

/// Only a format that quantizes names an integer width, and only WAV bounds
/// its length.
#[test]
fn the_integer_width_and_the_length_bound_follow_the_format() {
    #[cfg(feature = "encode-wav")]
    {
        assert_eq!(Format::Wav(Pcm::Int16).int_depth(), Some(IntDepth::Int16));
        assert_eq!(Format::Wav(Pcm::Float32).int_depth(), None);
        assert!(
            Format::Wav(Pcm::Int16)
                .max_frames(ChannelLayout::STEREO)
                .is_some()
        );
    }
    #[cfg(feature = "encode-aiff")]
    {
        assert_eq!(Format::Aiff(Pcm::Float32).int_depth(), None);
        assert_eq!(
            Format::Aiff(Pcm::Int24).max_frames(ChannelLayout::STEREO),
            None
        );
    }
    #[cfg(feature = "encode-flac")]
    assert_eq!(
        Format::Flac {
            depth: IntDepth::Int16,
            level: FlacLevel::Best
        }
        .int_depth(),
        Some(IntDepth::Int16)
    );
}
