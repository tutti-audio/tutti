//! The `plugin-probe` command line.

use std::process::Command;

/// **A format the probe does not know is refused with the usage text**, before
/// anything is loaded: exit status 2 and no result file.
#[test]
fn an_unknown_format_for_as_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let result = dir.path().join("result");

    let out = Command::new(env!("CARGO_BIN_EXE_plugin-probe"))
        .arg(dir.path().join("Some.clap"))
        .arg(&result)
        .args(["--as", "foo"])
        .output()
        .expect("spawn");

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: plugin-probe"));
    assert!(!result.exists());
}
