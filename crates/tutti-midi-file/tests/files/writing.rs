//! Writing a file: a path is staged beside the target and replaces what is
//! there unless `create_new` refuses it; a writer is given the whole file.
//! A refused or failed commit keeps the staged file, as the platform documents.

use std::path::Path;

use tutti_midi_file::{MidiFile, SmfDivision, WriteError};
use tutti_platform::Destination;

use crate::common::{encoded, track};

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
    assert_eq!(std::fs::read(&path).unwrap(), encoded(&f));
    assert_eq!(entries(dir.path()), ["a.mid"]);
    assert_eq!(MidiFile::read(&path).unwrap(), f);
}

/// A refused `create_new` is `Exists`, not the generic commit failure, and its
/// source names the kept staged file, which holds the whole new file.
#[test]
fn create_new_over_an_existing_file_is_exists_keeps_the_old_bytes_and_the_staged_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    std::fs::write(&path, b"old").unwrap();
    let f = file(480);
    let err = f.write(Destination::from(&path).create_new()).unwrap_err();
    let WriteError::Exists {
        path: ref taken,
        ref source,
    } = err
    else {
        panic!("not Exists: {err:?}");
    };
    assert_eq!(*taken, path);
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    assert_eq!(std::fs::read(source.kept()).unwrap(), encoded(&f));
    assert_eq!(entries(dir.path()).len(), 2);
}

#[test]
fn the_default_write_replaces_the_old_file_with_the_new_bytes_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    std::fs::write(&path, b"old").unwrap();
    let f = file(96);
    f.write(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), encoded(&f));
    assert_eq!(MidiFile::read(&path).unwrap(), f);
    assert_eq!(entries(dir.path()), ["a.mid"]);
}

#[test]
fn create_new_on_a_new_path_creates_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    file(96)
        .write(Destination::from(&path).create_new())
        .unwrap();
    assert_eq!(entries(dir.path()), ["a.mid"]);
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
    f.write(Destination::from(&mut bytes).create_new()).unwrap();
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

/// A commit that fails for a reason other than a refused `create_new` is
/// `Commit`, and the platform keeps the complete staged file: a default write
/// onto a directory cannot be renamed into place.
#[test]
fn a_failed_commit_is_commit_and_keeps_the_staged_file_and_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    std::fs::create_dir(&path).unwrap();
    let f = file(96);
    let err = f.write(&path).unwrap_err();
    let WriteError::Commit(ref commit) = err else {
        panic!("not Commit: {err:?}");
    };
    assert!(path.is_dir());
    assert_eq!(std::fs::read(commit.kept()).unwrap(), encoded(&f));
}

/// Whatever is at the path is refused, a directory too.
#[test]
fn create_new_over_a_directory_is_exists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mid");
    std::fs::create_dir(&path).unwrap();
    let err = file(96)
        .write(Destination::from(&path).create_new())
        .unwrap_err();
    assert!(matches!(err, WriteError::Exists { .. }), "{err:?}");
    assert!(path.is_dir());
}
