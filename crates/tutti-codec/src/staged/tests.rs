use super::*;

/// A temporary that cannot be removed is logged, not an error and not a
/// panic: the render's outcome (a cancel, say) is already decided. A
/// directory in the temporary's place stands for any removal that fails.
#[test]
fn a_temporary_that_cannot_be_removed_does_not_fail_the_drop() {
    let dir = tempfile::tempdir().unwrap();
    let staged = Staged::beside(&dir.path().join("a.wav"));
    std::fs::create_dir(staged.temp()).unwrap();
    let path = staged.temp().to_path_buf();
    drop(staged);
    assert!(
        path.is_dir(),
        "the leftover is still there, and was reported"
    );
}

/// The temporary sits beside its target (a rename within one directory) and
/// two stagings of one target do not share a name.
#[test]
fn the_temporary_is_beside_the_target_and_unique() {
    let target = std::path::Path::new("some/dir/a.wav");
    let (a, b) = (Staged::beside(target), Staged::beside(target));
    assert_eq!(a.temp().parent(), target.parent());
    assert_ne!(a.temp(), b.temp());
}

/// The name is made of the process-wide counter, the clock and the target's
/// name, so two stagings of one target differ even in one nanosecond, and the
/// target's own name is in it.
#[test]
fn many_stagings_of_one_target_all_differ() {
    let target = std::path::Path::new("a.wav");
    let names: std::collections::HashSet<_> = (0..1000)
        .map(|_| Staged::beside(target).temp().to_path_buf())
        .collect();
    assert_eq!(names.len(), 1000);
    assert!(names.iter().all(|n| {
        n.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".a.wav.")
    }));
}
