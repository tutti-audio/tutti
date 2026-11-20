//! Writing a file: a path is staged beside the target and replaces what is
//! there unless `create_new` refuses it; a writer is given the whole file.

use std::path::Path;

use tutti_midi_file::{MidiFile, SmfDivision, WriteError};

use crate::common::{ToBytes, track};

fn file(tpq: u16) -> MidiFile {
    crate::common::format0(SmfDivision::per_quarter(tpq), track(vec![])).unwrap()
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
fn create_new_over_an_existing_file_is_exists_and_keeps_the_old_bytes_and_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    std::fs::write(&path, b"old").unwrap();
    let err = file(480)
        .write_options()
        .create_new()
        .write(&path)
        .unwrap_err();
    assert!(
        matches!(err, WriteError::Exists { path: ref p } if *p == path),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    assert_eq!(entries(dir.path()), ["a.mid"]);
}

#[test]
fn the_default_write_replaces_the_old_file_with_the_new_bytes_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    std::fs::write(&path, b"old").unwrap();
    let f = file(96);
    f.write(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), f.to_bytes());
    assert_eq!(MidiFile::read(&path).unwrap(), f);
    assert_eq!(entries(dir.path()), ["a.mid"]);
}

#[test]
fn create_new_on_a_new_path_creates_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    file(96).write_options().create_new().write(&path).unwrap();
    assert!(path.is_file());
}

/// A path and bytes of the same file read to equal files.
#[test]
fn a_read_from_a_path_equals_a_read_from_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    let f = file(240);
    f.write(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        MidiFile::read(&path).unwrap(),
        MidiFile::read(bytes).unwrap()
    );
}

/// What a writer is given reads back as the file.
#[test]
fn a_write_to_a_vec_reads_back_as_the_file() {
    let f = file(192);
    let mut bytes = Vec::new();
    f.write(&mut bytes).unwrap();
    assert!(!bytes.is_empty());
    assert_eq!(MidiFile::read(bytes).unwrap(), f);
}

/// A writer is appended to, in one piece, whatever `create_new` says.
#[test]
fn a_write_to_a_writer_appends_the_whole_file_and_ignores_create_new() {
    let f = file(96);
    let mut bytes = b"head".to_vec();
    f.write_options().create_new().write(&mut bytes).unwrap();
    assert_eq!(&bytes[..4], b"head");
    // The SMF header chunk (SMF 1.0 section 2.1) follows what was there, whole.
    assert_eq!(&bytes[4..8], b"MThd");
    assert_eq!(MidiFile::read(bytes[4..].to_vec()).unwrap(), f);
}

#[test]
fn a_failing_writer_is_a_writer_error() {
    struct Full;
    impl std::io::Write for Full {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::StorageFull.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let err = file(96).write(&mut Full).unwrap_err();
    assert!(matches!(err, WriteError::Writer(_)), "{err:?}");
}

#[test]
fn a_read_of_a_missing_path_is_an_io_error_not_a_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = MidiFile::read(dir.path().join("absent.mid")).unwrap_err();
    assert!(matches!(err, tutti_midi_file::ReadError::Io(_)), "{err:?}");
}

#[test]
fn a_write_into_a_missing_directory_is_an_io_error_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("no").join("a.mid");
    let err = file(96).write(&path).unwrap_err();
    assert!(matches!(err, WriteError::Stage(_)), "{err:?}");
    assert!(entries(dir.path()).is_empty());
}
