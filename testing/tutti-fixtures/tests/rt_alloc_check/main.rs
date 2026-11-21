//! `RtAllocCheck` aborts an allocation made inside an `RtScope`, and only there.
//!
//! The refusal aborts the process, so the test re-runs this binary as a child
//! that allocates inside a scope, and reads the child's exit status and stderr.

use std::process::Command;
use tutti_fixtures::RtAllocCheck;
use tutti_rt::RtScope;

#[global_allocator]
static A: RtAllocCheck = RtAllocCheck;

const CHILD: &str = "TUTTI_RT_ALLOC_CHECK_CHILD";

#[test]
fn an_allocation_inside_a_scope_aborts_and_one_outside_does_not() {
    if let Ok(case) = std::env::var(CHILD) {
        match case.as_str() {
            "outside" => drop(std::hint::black_box(vec![1_u8; 64])),
            _ => {
                let _block = RtScope::enter();
                drop(std::hint::black_box(vec![1_u8; 64]));
            }
        }
        return;
    }
    let run = |case: &str| {
        Command::new(std::env::current_exe().expect("the test binary's path"))
            .args([
                "--exact",
                "an_allocation_inside_a_scope_aborts_and_one_outside_does_not",
                "--nocapture",
            ])
            .env(CHILD, case)
            .output()
            .expect("the child runs")
    };
    let outside = run("outside");
    assert!(
        outside.status.success(),
        "an allocation outside a scope is allowed"
    );
    let inside = run("inside");
    assert!(
        !inside.status.success(),
        "an allocation inside a scope aborts"
    );
    let err = String::from_utf8_lossy(&inside.stderr);
    assert!(
        err.contains("inside an audio block"),
        "the report names the cause: {err}"
    );
}
