use super::*;

/// A path that is not there is an I/O error of kind `NotFound`, from both
/// entry points.
#[test]
fn a_missing_file_is_an_io_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("none.wav");
    for error in [
        read_wave::<f32>(&missing).unwrap_err(),
        read_wave_metadata(&missing).unwrap_err(),
    ] {
        assert!(
            matches!(&error, DecodeError::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
            "{error}"
        );
    }
}

/// Bytes that are no container are refused, not decoded as silence.
#[test]
fn bytes_that_are_no_audio_are_an_error() {
    assert!(read_wave::<f32>(vec![0x5a; 4096]).is_err());
    assert!(read_wave::<f32>(Vec::new()).is_err());
}

/// A track that states no sample rate is refused with `NoSampleRate`, never
/// given one of ours: the file would play at the wrong pitch with no error.
/// (A container that states none cannot be built from bytes here, since the
/// demuxers refuse a zero rate themselves, so the track's parameters are.)
#[test]
fn a_track_with_no_sample_rate_is_refused() {
    let mut params = AudioCodecParameters::new();
    assert!(matches!(
        sample_rate_of(&params),
        Err(DecodeError::NoSampleRate)
    ));
    // A stated rate of 0 states none.
    params.with_sample_rate(0);
    assert!(matches!(
        sample_rate_of(&params),
        Err(DecodeError::NoSampleRate)
    ));
    params.with_sample_rate(48_000);
    assert_eq!(
        sample_rate_of(&params).expect("stated"),
        tutti_units::SampleRate::SR_48K
    );
}

/// A header stating far more frames than the source holds reserves a bounded
/// count, however large the stated figure and however few the channels.
#[test]
fn the_reserve_for_a_stated_frame_count_is_bounded() {
    assert_eq!(reserved_frames(100, 2), 100);
    assert_eq!(reserved_frames(u64::MAX, 1), RESERVE_SAMPLES as usize);
    assert_eq!(reserved_frames(u64::MAX, 8), RESERVE_SAMPLES as usize / 8);
}

/// A mono 16-bit header whose data length (`0x4500_0040` bytes) far exceeds the
/// 32 bytes present decodes to the 16 frames present.
#[test]
fn a_data_length_far_past_the_source_decodes_the_frames_present() {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend(0xFFF7_FFFFu32.to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    for field in [1u16, 1] {
        bytes.extend(field.to_le_bytes());
    }
    bytes.extend(8_000u32.to_le_bytes());
    bytes.extend(16_000u32.to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(0x4500_0040u32.to_le_bytes());
    bytes.extend([0u8; 32]);
    let wave = read_wave::<f32>(bytes).expect("decodes");
    assert_eq!(wave.channels(), 1);
    assert_eq!(wave.len().get(), 16);
}
