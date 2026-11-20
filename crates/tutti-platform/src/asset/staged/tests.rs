use std::io::{ErrorKind, Write};
use std::path::Path;

use super::{Staged, StagedFile};

fn names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn stage(staged: Staged, bytes: &[u8]) -> StagedFile {
    let mut file = staged.open().unwrap();
    file.write_all(bytes).unwrap();
    file
}

#[test]
fn a_committed_write_leaves_only_the_target_with_the_bytes() {
    for create_new in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.bin");
        let mut staged = Staged::new(&target);
        if create_new {
            staged = staged.create_new();
        }
        stage(staged, b"whole").commit().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"whole");
        assert_eq!(names(dir.path()), ["a.bin"], "create_new = {create_new}");
    }
}

#[test]
fn a_dropped_write_leaves_no_temporary_and_the_target_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.bin");
    std::fs::write(&target, b"old").unwrap();
    let file = stage(Staged::new(&target), b"new");
    assert_eq!(names(dir.path()).len(), 2);
    drop(file);
    assert_eq!(names(dir.path()), ["a.bin"]);
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
}

#[test]
fn a_staging_that_never_opened_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    drop(Staged::new(&dir.path().join("a.bin")));
    assert!(names(dir.path()).is_empty());
}

/// A write still in a `BufWriter` reaches the target once the writer is handed
/// back and the file committed: the hand-back flushes, the commit syncs.
#[test]
fn a_buffered_write_is_in_the_target_after_the_hand_back_and_commit() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.bin");
    let mut writer = std::io::BufWriter::new(Staged::new(&target).open().unwrap());
    writer.write_all(b"buffered").unwrap();
    assert_eq!(std::fs::metadata(writer.get_ref().temp()).unwrap().len(), 0);
    writer.into_inner().unwrap().commit().unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"buffered");
    assert_eq!(names(dir.path()), ["a.bin"]);
}

/// A staged file seeks and overwrites, as a header back-patch needs.
#[test]
fn a_staged_file_seeks_and_overwrites() {
    use std::io::{Seek, SeekFrom};
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.bin");
    let mut file = stage(Staged::new(&target), b"abcdef");
    file.seek(SeekFrom::Start(1)).unwrap();
    file.write_all(b"XY").unwrap();
    file.commit().unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"aXYdef");
}

#[test]
fn the_default_commit_replaces_an_existing_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.bin");
    std::fs::write(&target, b"old").unwrap();
    stage(Staged::new(&target), b"new").commit().unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"new");
    assert_eq!(names(dir.path()), ["a.bin"]);
}

#[test]
fn a_refused_commit_keeps_the_complete_file_and_leaves_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.bin");
    std::fs::write(&target, b"old").unwrap();
    let err = stage(Staged::new(&target).create_new(), b"new")
        .commit()
        .unwrap_err();
    assert_eq!(
        err.kept().parent(),
        target.parent(),
        "the file is kept beside the target"
    );
    assert_eq!(std::fs::read(err.kept()).unwrap(), b"new");
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    let kept = err.kept().to_path_buf();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert!(err.to_string().contains(&kept.display().to_string()));
    assert_eq!(err.into_source().kind(), ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&kept).unwrap(),
        b"new",
        "the kept file outlives the error"
    );
    assert_eq!(names(dir.path()).len(), 2);
}

#[test]
fn create_new_refuses_a_target_that_appears_after_the_staging() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.bin");
    let file = stage(Staged::new(&target).create_new(), b"new");
    std::fs::write(&target, b"raced").unwrap();
    let err = file.commit().unwrap_err();
    assert_eq!(std::fs::read(err.kept()).unwrap(), b"new");
    assert_eq!(err.into_source().kind(), ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&target).unwrap(), b"raced");
}

/// A temporary that cannot be removed is logged, not an error and not a
/// panic: the write's outcome is already decided. A directory in the
/// temporary's place stands for any removal that fails.
#[test]
fn a_temporary_that_cannot_be_removed_does_not_fail_the_drop() {
    let dir = tempfile::tempdir().unwrap();
    let file = stage(Staged::new(&dir.path().join("a.wav")), b"x");
    let path = file.temp().to_path_buf();
    // Replaced behind the file's back by a directory, which `remove_file` cannot remove.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    drop(file);
    assert!(path.is_dir(), "the leftover is still there");
}

/// The temporary sits beside its target (a rename within one directory) and
/// two stagings of one target do not share a name.
#[test]
fn the_temporary_is_beside_the_target_and_unique() {
    let target = Path::new("some/dir/a.wav");
    let (a, b) = (Staged::new(target), Staged::new(target));
    assert_eq!(a.temp().parent(), target.parent());
    assert_ne!(a.temp(), b.temp());
}

/// The name is made of the process-wide counter, the clock and the target's
/// name, so two stagings of one target differ even in one nanosecond, and the
/// target's own name is in it.
#[test]
fn many_stagings_of_one_target_all_differ() {
    let target = Path::new("a.wav");
    let names: std::collections::HashSet<_> = (0..1000)
        .map(|_| Staged::new(target).temp().to_path_buf())
        .collect();
    assert_eq!(names.len(), 1000);
    assert!(names.iter().all(|n| {
        n.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".a.wav.")
    }));
}
