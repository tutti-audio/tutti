//! The server-death tests that read `/proc` or name the server's pid.

use super::*;
use tutti_plugin_types::{IpcError, TerminationReason};

/// **A reload that fails to load reports the `IpcError` as the cause's
/// `source`.** The plugin is loaded from a copy of the reference plugin; the
/// copy is deleted once it is running, and its first server aborts on its
/// 40th block. Every replacement is then asked to load a file that is gone,
/// so each attempt fails with the server's load error: `Reloading` and
/// `GaveUp` carry `RecoveryFailure::ReloadFailed`, with the `IpcError`
/// behind it, not as text.
#[test]
fn a_reload_that_fails_to_load_carries_the_bridge_error_as_its_source() {
    use std::error::Error;
    let copy = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("vanishing-probe.clap");
    std::fs::copy(clap_probe(), &copy).expect("a copy of the reference plugin");
    // The copy is probed before the session's variables are set.
    crate::pooled::prime(&PluginRef::file(&copy));
    let mut s = session_built(&[("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "40")], |b| {
        b.plugin_recovery(Some(tutti_session::RecoveryPolicy {
            max_failures: 2,
            backoff: Duration::from_millis(1),
            ..Default::default()
        }))
    });
    let node = s
        .load_plugin_blocking(
            tutti_fixtures::main_thread(),
            Spec::new(PluginRef::file(&copy)),
        )
        .expect("the copy loads");
    s.graph().connect(node, Root).unwrap();
    let _ = s.update();
    std::fs::remove_file(&copy).expect("the copy is removed once it runs");

    let mut drive = Drive::new();
    drive.until(&mut s, "given up", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::GaveUp { .. },
                ..
            })
        )
    });
    let causes: Vec<&ReloadCause> = drive
        .events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { cause, .. },
                ..
            }) => Some(cause),
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            causes.first(),
            Some(ReloadCause::Died(
                PluginFailure::StreamClosed | PluginFailure::ServerExited { .. }
            ))
        ),
        "the first reload is for the death: {causes:?}"
    );
    let failed = causes
        .get(1)
        .expect("a second attempt follows a failed load");
    assert!(
        matches!(
            failed,
            ReloadCause::Attempt(RecoveryFailure::ReloadFailed { .. })
        ),
        "{failed:?}"
    );
    let ReloadCause::Attempt(attempt) = failed else {
        unreachable!()
    };
    let source = attempt.source().expect("a failed load has a source");
    assert!(
        matches!(
            source.downcast_ref::<IpcError>(),
            Some(IpcError::LoadFailed { .. })
        ),
        "the load's own error is the source: {source:?}"
    );
    let Some(SessionEvent::Plugin(PluginEvent {
        change: PluginChange::GaveUp { why, .. },
        ..
    })) = drive.events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::GaveUp { .. },
                ..
            })
        )
    })
    else {
        unreachable!()
    };
    let ReloadCause::Attempt(last @ RecoveryFailure::ReloadFailed { .. }) = &why.last else {
        panic!("the give-up carries the failed load: {:?}", why.last);
    };
    assert!(
        matches!(
            last.source().and_then(|e| e.downcast_ref::<IpcError>()),
            Some(IpcError::LoadFailed { .. })
        ),
        "{last:?}"
    );
}

/// Whether the process `pid` is gone (reaped, or dead and awaiting reaping).
fn process_ended(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with(['Z', 'X'])),
    }
}

/// **A plugin that stops answering is ended, then replaced.** The probe's
/// first server parks in `process()` for good (the once-file: a replacement
/// does not): it never crashes, so only the abandoned-block count shows it.
/// The session ends its server (the death reads "stopped answering"), the
/// node is kept, and when the replacement's load is submitted the hung
/// server is already gone: a live server is never left beside its
/// replacement. The replacement renders.
#[test]
fn a_plugin_that_stops_answering_is_ended_and_replaced() {
    let once = once_file("recovery-hang.once");
    // The spawner is the load's moment: once the death is reported, a job it
    // is handed (the reload) finds the hung server gone.
    let reloading = Arc::new(AtomicBool::new(false));
    let hung_pid = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (seen, pid_cell) = (Arc::clone(&reloading), Arc::clone(&hung_pid));
    let mut s = session_built(
        &[
            PASSTHROUGH,
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_BLOCK_FROM", "20"),
        ],
        |b| {
            b.plugin_hang(quick_hang_policy())
                .plugin_recovery(Some(quick_reload_policy()))
                .executor(tutti_platform::Executor::new(move |job| {
                    if seen.load(Ordering::SeqCst) {
                        let pid = pid_cell.load(Ordering::SeqCst);
                        assert!(
                            process_ended(pid),
                            "the hung server {pid} was still alive when its replacement was loaded"
                        );
                    }
                    tutti_fixtures::run_future(job);
                }))
        },
    );
    let (node, _) = wired_probe(&mut s);
    let pid = s
        .plugin_view(node)
        .unwrap()
        .diagnostics()
        .server_pid()
        .expect("an out-of-process plugin has a server");

    hung_pid.store(pid, Ordering::SeqCst);

    let mut drive = Drive::new();
    drive.reloading = Some(reloading);
    drive.until(&mut s, "recovered", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Recovered { .. },
                ..
            })
        )
    });
    let Some(SessionEvent::Plugin(PluginEvent {
        change: PluginChange::Reloading { cause, .. },
        ..
    })) = drive.events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { .. },
                ..
            })
        )
    })
    else {
        unreachable!()
    };
    assert!(
        matches!(
            cause,
            ReloadCause::Died(PluginFailure::Terminated {
                reason: TerminationReason::Hung { .. }
            })
        ),
        "{cause:?}"
    );
    assert_eq!(
        drive.count(|e| matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Died { .. },
                ..
            })
        )),
        0
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while !renders_sound(&mut s) {
        assert!(Instant::now() < deadline, "the replacement renders nothing");
        std::thread::sleep(Duration::from_millis(5));
    }
}
