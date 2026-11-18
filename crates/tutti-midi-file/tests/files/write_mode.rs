//! The mode of a written file.

use std::os::unix::fs::PermissionsExt;

use tutti_midi_file::{MidiFile, SmfDivision, SmfFormat};

use crate::common::track;

/// The staged file is created at `0o666` under the umask, as `std::fs::write`
/// would, not at the temp file's private `0o600`.
#[test]
fn a_written_file_has_the_umask_mode_not_the_private_temp_mode() {
    let dir = tempfile::tempdir().unwrap();
    let reference = dir.path().join("reference");
    std::fs::write(&reference, b"").unwrap();
    let expected = std::fs::metadata(&reference).unwrap().permissions().mode();
    let path = dir.path().join("a.mid");
    MidiFile::smf(
        SmfFormat::SingleTrack,
        SmfDivision::per_quarter(96),
        vec![track(vec![])],
    )
    .unwrap()
    .write(&path)
    .unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode, expected);
}
