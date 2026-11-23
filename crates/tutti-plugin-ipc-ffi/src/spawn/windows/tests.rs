//! What a Windows spawn hands its child, read from the child's side. A child is
//! this test binary run again on one test, which checks what it can see and
//! reports through its exit code; the host side asserts the report.

use super::command_line::{application_name, command_line};
use super::environment;
use super::{Spawn, SpawnError, spawn};
use crate::Reaped;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::Command;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{CompareObjectHandles, HANDLE, LocalFree, WAIT_OBJECT_0};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_UNKNOWN, GetFileType};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetProcessHandleCount, GetProcessId, OpenEventW, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, SYNCHRONIZATION_SYNCHRONIZE,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

/// A handle of the test's own to the process `reaped` holds, opened by its id
/// (which `reaped` keeps from being reused while it lives).
fn opened(reaped: &Reaped) -> OwnedHandle {
    // SAFETY: plain scalars; the result is checked before it is owned.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            reaped.id(),
        )
    };
    assert!(!handle.is_null(), "the process opens by its id");
    // SAFETY: the open succeeded: the handle is open and owned by no one else.
    unsafe { OwnedHandle::from_raw_handle(handle) }
}

/// The variable that makes this binary a child, holding the test it is a child of.
const CHILD: &str = "TUTTI_SPAWN_CHILD";
/// What a child exits with when its check holds, and when it does not. Any other
/// code (0 for a test name that matched nothing, 101 for a panic) is neither.
const HOLDS: i32 = 42;
const FAILS: i32 = 43;

/// Arguments that exercise the quoting: none starts with `-`, which the test
/// harness of the child would read as an option.
const ARGUMENTS: &[&str] = &[
    "",
    "plain",
    "a b",
    "q\"uote",
    "back\\",
    "back\\\\\"x",
    "trail \\",
    "tab\there",
    "line\nbreak",
    "\\\\\\\\\\\\\\\\ many",
    "\"",
    "\\\"",
    "\u{fc}n\u{ef} \u{2603}",
];

/// The name `--exact` selects the calling test by.
fn selector(test: &str) -> String {
    let module = module_path!();
    let module = module.split_once("::").map_or(module, |(_, rest)| rest);
    format!("{module}::{test}")
}

/// When this process is the child of test `test`, runs `check` and exits with
/// the code that reports it; otherwise returns.
fn as_child(test: &str, check: impl FnOnce() -> bool) {
    if std::env::var_os(CHILD).is_some_and(|name| name == test) {
        std::process::exit(if check() { HOLDS } else { FAILS });
    }
}

/// The command that runs `test` again as its own child.
fn child_of(test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("the test binary's path"));
    command
        .args([selector(test).as_str(), "--exact", "--nocapture"])
        .env(CHILD, test);
    command
}

/// What `reaped` exited with.
fn exit_code(mut reaped: Reaped) -> Option<i32> {
    reaped.wait().expect("the child ends").code()
}

/// `name` as a NUL-terminated UTF-16 string.
fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain([0]).collect()
}

/// An inheritable event named `name`, so a child can open the same object by
/// name and tell it from any other handle that happens to share its value.
fn inheritable_event(name: &str) -> OwnedHandle {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let name = wide(name);
    // SAFETY: `attributes` is a live, correctly sized struct; `name` is a live,
    // NUL-terminated UTF-16 string.
    let event = unsafe { CreateEventW(&attributes, 1, 1, name.as_ptr()) };
    assert!(!event.is_null(), "the event is made");
    // SAFETY: the event is open, non-null and owned by no one else.
    unsafe { OwnedHandle::from_raw_handle(event) }
}

/// Whether the standard handle `id` of this process is a handle at all, and of
/// type `kind` when one is given.
fn standard_is(id: u32, kind: Option<u32>) -> bool {
    // SAFETY: both calls take a scalar or the handle the first returns.
    let type_of = unsafe { GetFileType(GetStdHandle(id)) };
    type_of != FILE_TYPE_UNKNOWN && kind.is_none_or(|kind| type_of == kind)
}

/// **A spawned child does not inherit a handle of the host's that is marked
/// inheritable**: the event is named and inheritable, and a child started by std
/// alone holds it at the host's handle value (the control, which also proves the
/// child's check can see an inherited handle), while a helper and a leader do
/// not. The child compares the value with the object it opens by name, so a
/// handle of its own that shares the value is never taken for the event.
#[test]
fn a_spawned_child_inherits_no_inheritable_handle_of_the_host() {
    const EVENT: &str = "TUTTI_SPAWN_EVENT";
    const NAME: &str = "TUTTI_SPAWN_EVENT_NAME";
    let test = "a_spawned_child_inherits_no_inheritable_handle_of_the_host";
    as_child(test, || {
        let event: usize = std::env::var(EVENT).unwrap().parse().unwrap();
        let name = wide(&std::env::var(NAME).unwrap());
        // SAFETY: `name` is a live, NUL-terminated UTF-16 string.
        let opened = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
        assert!(!opened.is_null(), "the host's event opens by name");
        // SAFETY: the open succeeded: the handle is open and owned by no one else.
        let opened = unsafe { OwnedHandle::from_raw_handle(opened) };
        // SAFETY: both arguments are handle values; one that names no object
        // here compares unequal.
        unsafe { CompareObjectHandles(event as HANDLE, opened.as_raw_handle()) != 0 }
    });
    let name = format!("Local\\tutti-spawn-inherit-{}", std::process::id());
    let event = inheritable_event(&name);
    let value = (event.as_raw_handle() as usize).to_string();

    let mut control = child_of(test);
    control.env(EVENT, &value).env(NAME, &name);
    let status = control.status().expect("the control spawns");
    assert_eq!(
        status.code(),
        Some(HOLDS),
        "the control child sees the event"
    );

    for how in [Spawn::Helper, Spawn::Leader] {
        let mut command = child_of(test);
        command.env(EVENT, &value).env(NAME, &name);
        let reaped = spawn(command, how).expect("the child spawns");
        assert_eq!(
            exit_code(reaped),
            Some(FAILS),
            "{how:?} inherited the event"
        );
    }
}

/// **A helper's three standard handles are the `NUL` device, and a leader's are
/// open handles**: a handle list that left a standard handle out would show
/// the child an invalid one.
#[test]
fn a_childs_standard_handles_are_open() {
    let test = "a_childs_standard_handles_are_open";
    let ids = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    as_child(test, || {
        let kind = std::env::var("TUTTI_SPAWN_KIND").unwrap();
        let kind = (kind == "null").then_some(FILE_TYPE_CHAR);
        ids.iter().all(|&id| standard_is(id, kind))
    });
    let mut helper = child_of(test);
    helper.env("TUTTI_SPAWN_KIND", "null");
    let helper = spawn(helper, Spawn::Helper).expect("the helper spawns");
    assert_eq!(exit_code(helper), Some(HOLDS), "a helper's handles are NUL");
    let mut leader = child_of(test);
    leader.env("TUTTI_SPAWN_KIND", "any");
    let leader = spawn(leader, Spawn::Leader).expect("the leader spawns");
    assert_eq!(
        exit_code(leader),
        Some(HOLDS),
        "a leader's handles are open"
    );
}

/// The arguments `CommandLineToArgvW` reads out of `line`.
fn parsed(line: &[u16]) -> Vec<Vec<u16>> {
    let mut count = 0;
    // SAFETY: `line` is NUL-terminated and `count` is a live out-parameter.
    let argv = unsafe { CommandLineToArgvW(line.as_ptr(), &mut count) };
    assert!(!argv.is_null(), "the line parses");
    let args = (0..usize::try_from(count).unwrap())
        .map(|i| {
            // SAFETY: `argv` holds `count` strings, each NUL-terminated, and the
            // length is found before the slice is made.
            unsafe {
                let start = *argv.add(i);
                let mut len = 0;
                while *start.add(len) != 0 {
                    len += 1;
                }
                std::slice::from_raw_parts(start, len).to_vec()
            }
        })
        .collect();
    // SAFETY: `argv` is the one block `CommandLineToArgvW` allocated.
    unsafe { LocalFree(argv.cast()) };
    args
}

/// **The command line reads back, through Windows's own `CommandLineToArgvW`,
/// as the arguments it was made from**, and the program is `argv[0]` whole, a
/// path with spaces included.
#[test]
fn the_command_line_round_trips_through_command_line_to_argv() {
    let program = OsStr::new("C:\\Program Files\\tutti\\server.exe");
    let args: Vec<&OsStr> = ARGUMENTS.iter().map(OsStr::new).collect();
    let line = command_line(program, args.iter().copied()).expect("the line is made");
    let read = parsed(&line);
    assert_eq!(read[0], program.encode_wide().collect::<Vec<u16>>());
    let want: Vec<Vec<u16>> = ARGUMENTS
        .iter()
        .map(|a| a.encode_utf16().collect())
        .collect();
    assert_eq!(read[1..], want[..]);
}

/// **A program or argument that cannot be passed is `InvalidInput`**: a NUL, a
/// program with no path, with a quote, or a batch file, and a line past the limit.
#[test]
fn what_cannot_be_passed_is_refused() {
    let refused = |result: std::io::Result<_>| {
        assert_eq!(result.expect_err("refused").kind(), ErrorKind::InvalidInput);
    };
    refused(application_name(OsStr::new("server.exe")));
    refused(application_name(OsStr::new("C:\\a\\b\"c.exe")));
    refused(application_name(OsStr::new("C:\\a\\run.CMD")));
    refused(application_name(OsStr::new("C:\\a\\run.bat")));
    refused(application_name(&OsString::from_wide(&[
        0x43, 0x3a, 0x5c, 0, 0x61,
    ])));
    refused(command_line(OsStr::new("C:\\a.exe"), [OsStr::new("a\0b")]));
    let long = "x".repeat(32_767);
    refused(command_line(OsStr::new("C:\\a.exe"), [OsStr::new(&long)]));
    assert!(application_name(OsStr::new("C:\\a\\server.exe")).is_ok());
}

/// **Every argument reaches the child as it was given**: the child compares its
/// own arguments, as the C runtime and std read them, with the table.
#[test]
fn the_arguments_reach_the_child_as_given() {
    let test = "the_arguments_reach_the_child_as_given";
    as_child(test, || {
        // The program, the test, `--exact` and `--nocapture` come first.
        std::env::args_os()
            .skip(4)
            .eq(ARGUMENTS.iter().map(OsString::from))
    });
    let mut command = child_of(test);
    command.args(ARGUMENTS);
    let reaped = spawn(command, Spawn::Helper).expect("the child spawns");
    assert_eq!(exit_code(reaped), Some(HOLDS));
}

/// **The environment block is the host's variables with the command's changes
/// applied, in the order Windows wants**: sorted by upper-cased name, a variable
/// set under another case replacing the host's, one removed gone, and ended by
/// an extra NUL.
#[test]
fn the_environment_block_applies_the_commands_changes_in_order() {
    let mut command = Command::new("C:\\a.exe");
    command
        .env("path", "overlay")
        .env("tutti_spawn_b", "2")
        .env("TUTTI_SPAWN_A", "1")
        .env_remove("ComSpec");
    let block = environment::block(&command).expect("the block is made");
    assert_eq!(block[block.len() - 2..], [0, 0]);
    // The last entry's terminator and the block's are the final two units.
    let entries: Vec<String> = block[..block.len() - 2]
        .split(|&unit| unit == 0)
        .map(String::from_utf16_lossy)
        .collect();
    let names: Vec<String> = entries
        .iter()
        // A name runs to the first `=` after its first character (a hidden
        // name starts with one).
        .map(|entry| {
            let end = entry[1..].find('=').map_or(entry.len(), |at| at + 1);
            entry[..end].to_uppercase()
        })
        .collect();
    assert!(names.is_sorted(), "{names:?}");
    assert_eq!(names.iter().filter(|name| *name == "PATH").count(), 1);
    assert!(entries.iter().any(|entry| entry == "path=overlay"));
    assert!(entries.contains(&"tutti_spawn_b=2".to_owned()));
    assert!(!names.iter().any(|name| name == "COMSPEC"));
    let a = names.iter().position(|n| n == "TUTTI_SPAWN_A").unwrap();
    let b = names.iter().position(|n| n == "TUTTI_SPAWN_B").unwrap();
    assert!(a < b);
}

/// **The child sees the environment it was given**: a variable set is there, one
/// set under another case replaces the host's, and one removed is gone.
#[test]
fn the_child_sees_the_environment_it_was_given() {
    let test = "the_child_sees_the_environment_it_was_given";
    as_child(test, || {
        let paths: Vec<OsString> = std::env::vars_os()
            .filter(|(name, _)| name.eq_ignore_ascii_case("path"))
            .map(|(_, value)| value)
            .collect();
        std::env::var("TUTTI_SPAWN_NEW").is_ok_and(|value| value == "1")
            && paths == [OsString::from("overlay")]
            && std::env::var_os("ComSpec").is_none()
    });
    let mut command = child_of(test);
    command
        .env("TUTTI_SPAWN_NEW", "1")
        .env("pAtH", "overlay")
        .env_remove("ComSpec");
    let reaped = spawn(command, Spawn::Helper).expect("the child spawns");
    assert_eq!(exit_code(reaped), Some(HOLDS));
}

/// **A leader runs its first instruction already inside a job**, which a resume
/// before the assignment would not guarantee, and is a member of one when the
/// host looks.
#[test]
fn a_leader_is_in_a_job_before_it_runs() {
    let test = "a_leader_is_in_a_job_before_it_runs";
    as_child(test, || {
        let mut inside = 0;
        // SAFETY: the process is this one's pseudo handle, a null job asks "any
        // job", and `inside` is a live out-parameter.
        let asked = unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut inside) };
        asked != 0 && inside != 0
    });
    let reaped = spawn(child_of(test), Spawn::Leader).expect("the leader spawns");
    let mut inside = 0;
    let process = opened(&reaped);
    // SAFETY: `process` is an open process handle, a null job asks "any job",
    // and `inside` is a live out-parameter.
    let asked = unsafe { IsProcessInJob(process.as_raw_handle(), null_mut(), &mut inside) };
    assert!(asked != 0 && inside != 0, "the host sees no job");
    assert_eq!(
        exit_code(reaped),
        Some(HOLDS),
        "the child ran outside the job"
    );
}

fn handle_count() -> u32 {
    let mut count = 0;
    // SAFETY: the pseudo handle of this process, and a live out-parameter.
    assert!(unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) } != 0);
    count
}

/// **A spawn that fails leaves no handle behind**: the process's handle count is
/// what it was after twenty missing programs and twenty refused arguments.
///
/// The count is taken after three warm-up rounds: the OS opens and keeps keys
/// and sections on a process's first `CreateProcessW` calls, which a per-spawn
/// leak is told apart from by repeating.
#[test]
fn a_failed_spawn_leaves_no_handle() {
    let round = || {
        let missing = Command::new("C:\\no\\such\\dir\\tutti.exe");
        match spawn(missing, Spawn::Leader) {
            Err(SpawnError::Start(e)) => assert_eq!(e.kind(), ErrorKind::NotFound),
            other => panic!("a missing program started: {other:?}"),
        }
        let mut refused = Command::new("C:\\no\\such\\dir\\tutti.exe");
        refused.arg("a\0b");
        assert!(matches!(
            spawn(refused, Spawn::Helper),
            Err(SpawnError::Start(_))
        ));
    };
    for _ in 0..3 {
        round();
    }
    let before = handle_count();
    for _ in 0..20 {
        round();
    }
    assert_eq!(handle_count(), before);
}

/// **A held process answers as a process**: running it has no status; its id is
/// the OS's for the handle; killed, it ends with a non-zero status; dropped, it
/// is ended.
#[test]
fn a_held_process_is_a_process() {
    let test = "a_held_process_is_a_process";
    // The child only stays alive until it is killed.
    as_child(test, || {
        loop {
            std::thread::park();
        }
    });
    let mut reaped = spawn(child_of(test), Spawn::Helper).expect("the child spawns");
    assert!(
        reaped.try_reap().expect("the look").is_none(),
        "it has ended"
    );
    let process = opened(&reaped);
    // SAFETY: `process` is an open process handle, borrowed for the call.
    assert_eq!(reaped.id(), unsafe {
        GetProcessId(process.as_raw_handle())
    });
    reaped.kill_all().expect("the kill");
    let status = reaped.wait().expect("the wait");
    assert!(!status.success());
    assert_eq!(reaped.try_reap().expect("the look"), Some(status));

    let reaped = spawn(child_of(test), Spawn::Helper).expect("the second child spawns");
    let handle = opened(&reaped);
    drop(reaped);
    // The bound only guards a hang.
    // SAFETY: `handle` is an open process handle, borrowed for the call.
    let waited = unsafe { WaitForSingleObject(handle.as_raw_handle(), 60_000) };
    assert_eq!(waited, WAIT_OBJECT_0, "a dropped process still runs");
}
