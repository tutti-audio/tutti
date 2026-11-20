use std::io::{ErrorKind, Write};
use std::path::Path;

use super::Staged;

fn names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn stage(staged: &Staged, bytes: &[u8]) {
    staged.open().unwrap().write_all(bytes).unwrap();
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
        stage(&staged, b"whole");
        staged.commit().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"whole");
        assert_eq!(names(dir.path()), ["a.bin"], "create_new = {create_new}");
    }
}

#[test]
fn a_dropped_write_leaves_no_temporary_and_the_target_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.bin");
    std::fs::write(&target, b"old").unwrap();
    let staged = Staged::new(&target);
    stage(&staged, b"new");
    assert_eq!(names(dir.path()).len(), 2);
    drop(staged);
    assert_eq!(names(dir.path()), ["a.bin"]);
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
}

#[test]
fn a_dropped_staging_that_never_opened_is_a_clean_drop() {
    let dir = tempfile::tempdir().unwrap();
    drop(Staged::new(&dir.path().join("a.bin")));
    assert!(names(dir.path()).is_empty());
}

#[test]
fn the_default_commit_replaces_an_existing_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.bin");
    std::fs::write(&target, b"old").unwrap();
    let staged = Staged::new(&target);
    stage(&staged, b"new");
    staged.commit().unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"new");
    assert_eq!(names(dir.path()), ["a.bin"]);
}

#[test]
fn create_new_refuses_an_existing_target_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.bin");
    std::fs::write(&target, b"old").unwrap();
    let staged = Staged::new(&target).create_new();
    stage(&staged, b"new");
    let err = staged.commit().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(names(dir.path()), ["a.bin"]);
}

#[test]
fn create_new_refuses_a_target_that_appears_after_the_staging() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.bin");
    let staged = Staged::new(&target).create_new();
    stage(&staged, b"new");
    std::fs::write(&target, b"raced").unwrap();
    assert_eq!(
        staged.commit().unwrap_err().kind(),
        ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"raced");
}

/// A temporary that cannot be removed is logged, not an error and not a
/// panic: the write's outcome is already decided. A directory in the
/// temporary's place stands for any removal that fails.
#[test]
fn a_temporary_that_cannot_be_removed_does_not_fail_the_drop() {
    let dir = tempfile::tempdir().unwrap();
    let staged = Staged::new(&dir.path().join("a.wav"));
    std::fs::create_dir(staged.temp()).unwrap();
    let path = staged.temp().to_path_buf();
    drop(staged);
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
