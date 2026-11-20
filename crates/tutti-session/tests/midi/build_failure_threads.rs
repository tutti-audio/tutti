//! A build that fails after the session made its own executor leaves none of its
//! drive threads running. Linux reads the process's thread names.
//!
//! The count covers every `tutti-exec*` thread in the process, so the test relies
//! on nextest running each test in a process of its own.

use tutti_midi_hardware::MidiHardwareError;
use tutti_session::{Error, Session};
use tutti_units::SampleRate;

/// How many threads of this process are executor drivers.
fn drivers() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("the process lists its threads")
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .filter(|name| name.starts_with("tutti-exec"))
        .count()
}

/// **The session's own executor and its drive threads end with a build that
/// fails**: once the error is returned, no `tutti-exec` thread remains. The
/// wait is a hang guard on the kernel's listing of a joined thread.
#[test]
fn a_failed_midi_open_leaves_no_drive_thread_running() {
    let built = Session::builder()
        .midi_hardware_over(
            Box::new(super::build_failure::Refuses),
            tutti_midi_hardware::MidiHardwareConfig::new(),
        )
        .sample_rate(SampleRate::SR_48K)
        .build();
    assert!(matches!(
        built,
        Err(Error::MidiHardware(MidiHardwareError::Unsupported(_)))
    ));
    let end = tutti_platform::Instant::now() + std::time::Duration::from_secs(30);
    while drivers() != 0 {
        assert!(
            tutti_platform::Instant::now() < end,
            "a drive thread is left"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
