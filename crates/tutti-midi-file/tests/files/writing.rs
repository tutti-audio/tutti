//! Writing a file to a path: staged beside the target, refused over an
//! existing file unless replaced.

use std::path::Path;

use tutti_midi_file::{MidiFile, SmfDivision, SmfFormat, WriteError};

use crate::common::track;

fn file(tpq: u16) -> MidiFile {
    MidiFile::smf(
        SmfFormat::SingleTrack,
        SmfDivision::of(tpq),
        vec![track(vec![])],
    )
    .unwrap()
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn a_write_to_a_new_path_creates_exactly_the_files_bytes_and_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    let f = file(480);
    f.write(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), f.to_bytes());
    assert_eq!(entries(dir.path()), ["a.mid"]);
    assert_eq!(MidiFile::read(&path).unwrap(), f);
}

#[test]
fn a_write_over_an_existing_file_is_exists_and_keeps_the_old_bytes_and_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    std::fs::write(&path, b"old").unwrap();
    let err = file(480).write(&path).unwrap_err();
    assert!(
        matches!(err, WriteError::Exists { path: ref p } if *p == path),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    assert_eq!(entries(dir.path()), ["a.mid"]);
}

#[test]
fn replace_swaps_the_old_file_for_the_new_bytes_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    std::fs::write(&path, b"old").unwrap();
    let f = file(96);
    f.write_options().replace().write(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), f.to_bytes());
    assert_eq!(MidiFile::read(&path).unwrap(), f);
    assert_eq!(entries(dir.path()), ["a.mid"]);
}

#[test]
fn replace_on_a_new_path_creates_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    file(96).write_options().replace().write(&path).unwrap();
    assert!(path.is_file());
}

#[test]
fn a_write_into_a_missing_directory_is_an_io_error_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("no").join("a.mid");
    let err = file(96).write(&path).unwrap_err();
    assert!(matches!(err, WriteError::Stage(_)), "{err:?}");
    assert!(entries(dir.path()).is_empty());
}
