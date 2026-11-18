//! The `plugin-probe` command line.

use std::process::Command;
use tutti_plugin_types::{ProbeExit, ProbeOutcome, ProbeRefusal, read_result};

/// **A format the probe does not know is a `Usage` result with the usage text**,
/// before anything is loaded: the probe exits cleanly (it is not a crash), and
/// the refusal is in the result file.
#[test]
fn an_unknown_format_for_as_is_a_usage_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let result = dir.path().join("result");

    let out = Command::new(env!("CARGO_BIN_EXE_plugin-probe"))
        .arg(dir.path().join("Some.clap"))
        .arg(&result)
        .args(["--as", "foo"])
        .output()
        .expect("spawn");

    assert_eq!(
        out.status.code(),
        Some(i32::from(ProbeExit::Written.code()))
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: plugin-probe"));
    assert_eq!(
        read_result(&result).unwrap(),
        Some(ProbeOutcome::Failed(ProbeRefusal::Usage))
    );
}

/// **With no result file named there is nowhere to say what was wrong**, so the
/// probe exits by the code that means exactly that, not by a small status a
/// plugin's `exit` could be.
#[test]
fn a_command_line_with_no_result_file_exits_unwritable() {
    let out = Command::new(env!("CARGO_BIN_EXE_plugin-probe"))
        .output()
        .expect("spawn");

    assert_eq!(
        out.status.code(),
        Some(i32::from(ProbeExit::ResultUnwritable.code()))
    );
}
