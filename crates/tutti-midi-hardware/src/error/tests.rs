use super::*;
use std::error::Error as _;

/// The `io::Error` at the end of `error`'s `source()` chain, if there is one.
fn io_cause(error: &MidiHardwareError) -> Option<&std::io::Error> {
    error.source()?.downcast_ref::<std::io::Error>()
}

/// **A thread that cannot start, and a failed system call, keep the OS's refusal
/// as their source**, so a caller walking `source()` reaches the `io::Error` and
/// its kind.
#[test]
fn an_error_that_the_os_refused_keeps_the_io_error_as_its_source() {
    use std::io::{Error, ErrorKind};
    for (case, error, kind) in [
        (
            "a spawn failure",
            MidiHardwareError::Spawn {
                thread: MidiThread::MidiIn,
                source: Error::from(ErrorKind::OutOfMemory),
            },
            ErrorKind::OutOfMemory,
        ),
        (
            "an io failure",
            MidiHardwareError::Io {
                step: "make a stop pair",
                source: Error::from(ErrorKind::PermissionDenied),
            },
            ErrorKind::PermissionDenied,
        ),
    ] {
        assert_eq!(io_cause(&error).map(Error::kind), Some(kind), "{case}");
    }
}

/// **A refused seed is the `SeedError` itself**, reachable through `source()`
/// and through `From`.
#[test]
fn a_seed_failure_keeps_the_seed_error_as_its_source() {
    let error = MidiHardwareError::from(crate::SeedError);
    assert!(matches!(error, MidiHardwareError::Seed(_)));
    assert!(error.source().is_some_and(|s| s.is::<crate::SeedError>()));
}
