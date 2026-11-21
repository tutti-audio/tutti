use super::*;
use std::error::Error as _;

/// The `io::Error` at the end of `error`'s `source()` chain, if there is one.
fn io_cause(error: &MidiHardwareError) -> Option<&std::io::Error> {
    error.source()?.downcast_ref::<std::io::Error>()
}

/// **A thread that cannot start keeps the OS's refusal as its source**, so a
/// caller walking `source()` reaches the `io::Error` and its kind.
#[test]
fn a_spawn_failure_keeps_the_os_refusal_as_its_source() {
    let error = MidiHardwareError::Spawn {
        thread: MidiThread::MidiIn,
        source: std::io::Error::from(std::io::ErrorKind::OutOfMemory),
    };
    assert_eq!(
        io_cause(&error).map(std::io::Error::kind),
        Some(std::io::ErrorKind::OutOfMemory)
    );
}

/// **A failed system call keeps its `io::Error` as the source.**
#[test]
fn an_io_failure_keeps_the_os_error_as_its_source() {
    let error = MidiHardwareError::Io {
        step: "make a stop pair",
        source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    };
    assert_eq!(
        io_cause(&error).map(std::io::Error::kind),
        Some(std::io::ErrorKind::PermissionDenied)
    );
}

/// **A refused seed is the `SeedError` itself**, reachable through `source()`
/// and through `From`.
#[test]
fn a_seed_failure_keeps_the_seed_error_as_its_source() {
    let error = MidiHardwareError::from(crate::SeedError);
    assert!(matches!(error, MidiHardwareError::Seed(_)));
    assert!(error.source().is_some_and(|s| s.is::<crate::SeedError>()));
}
