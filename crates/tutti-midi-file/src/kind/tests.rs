use super::*;
use crate::MidiFile;

#[test]
fn detect_tells_the_two_midi_formats_apart() {
    let clip = MidiFile::clip(
        tutti_ump::TicksPerQuarterNote::of(96),
        crate::Track::default(),
    )
    .unwrap()
    .encode();
    assert_eq!(MidiFileKind::of_header(&clip), Some(MidiFileKind::ClipFile));

    let smf = MidiFile::smf(
        crate::SmfDivision::per_quarter(480),
        crate::Layout::Format0(crate::LeadTrack::new(crate::Track::default())),
    )
    .unwrap()
    .encode();
    assert_eq!(
        MidiFileKind::of_header(&smf),
        Some(MidiFileKind::StandardMidiFile)
    );

    assert_eq!(MidiFileKind::of_header(b"not midi at all"), None);
    // Too short to carry either magic — not a format, not a panic.
    assert_eq!(MidiFileKind::of_header(b"SMF2"), None);
    assert_eq!(MidiFileKind::of_header(b""), None);
}

/// `detect` answers from the file's magic, and distinguishes the three
/// outcomes a caller must tell apart: recognised, readable-but-unrecognised,
/// and unreadable.
///
/// The extension case is the trap this closes — a clip file named `.mid`.
/// The extension says SMF; the magic says otherwise, and the magic wins.
#[test]
fn detect_reads_the_magic_not_the_extension() {
    let dir = tempfile::tempdir().unwrap();

    // A clip file under an SMF extension still detects as a clip file, and
    // the SMF reader rejects it rather than mis-parsing it.
    let misnamed = dir.path().join("misnamed.mid");
    MidiFile::clip(
        tutti_ump::TicksPerQuarterNote::of(96),
        crate::Track::default(),
    )
    .unwrap()
    .write(&misnamed)
    .unwrap();
    assert_eq!(
        MidiFileKind::detect(&misnamed).unwrap(),
        Some(MidiFileKind::ClipFile)
    );
    assert_eq!(
        MidiFile::read(&misnamed).unwrap().encode()[..8],
        crate::clip_file::CLIP_FILE_MAGIC
    );

    // Readable but unrecognised — `Ok(None)`, not an error.
    let junk = dir.path().join("not_midi.bin");
    std::fs::write(&junk, b"just some bytes").unwrap();
    assert_eq!(MidiFileKind::detect(&junk).unwrap(), None);

    // The same bytes from memory answer alike.
    assert_eq!(
        MidiFileKind::detect(b"just some bytes".to_vec()).unwrap(),
        None
    );
    assert_eq!(
        MidiFileKind::detect(std::fs::read(&misnamed).unwrap()).unwrap(),
        Some(MidiFileKind::ClipFile)
    );

    // Missing file — an error, not `Ok(None)`.
    assert!(MidiFileKind::detect(dir.path().join("absent.midi2")).is_err());
}

/// M2-116 section 4.1: Clip Files use the file extension `.midi2`; the
/// Standard MIDI File's are `.mid` and `.midi`.
/// Spec: m2-116/4.1#1
#[test]
fn the_extensions_include_the_ones_the_specifications_name() {
    assert_eq!(MidiFile::EXTENSIONS, ["mid", "midi", "midi2"]);
}
