//! How a helper process is started: its command, what it inherits and what ties
//! its life to the host's, in one call.
//!
//! One platform module is chosen here and nowhere else.

#[cfg(unix)]
#[path = "spawn/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "spawn/windows.rs"]
mod platform;

pub use platform::spawn;

/// What a spawned helper is to the host.
///
/// On Windows it also says what the helper's standard handles are, which std's
/// opaque `Stdio` cannot say to a hand-written spawn: a leader's are the host's
/// and a helper's are the `NUL` device, whatever the command sets. On unix the
/// command's own `stdin`, `stdout` and `stderr` are used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Spawn {
    /// A helper that ends by itself or by its owner's kill, with no standard
    /// input or output of the host's (Windows).
    Helper,
    /// A server, which leads the process group (unix) or sits in the host's
    /// kill-on-close job (Windows) that ends it, and the helpers a plugin
    /// spawned, with the host; on Windows it is in the job before it runs, and
    /// inherits the host's standard handles.
    Leader,
}

/// Why [`spawn`] returned no process.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The OS refused the spawn.
    #[error("the process did not start: {0}")]
    Start(#[source] std::io::Error),
    /// The process started and could not be tied to the host's end; it has been
    /// killed and reaped.
    #[error("the process could not be bound to the host: {0}")]
    NotBound(#[source] std::io::Error),
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Spawn, spawn};
    use rustix::io::{FdFlags, fcntl_setfd};
    use rustix::process::{Pid, getpgid};
    use std::fs::File;
    use std::io::{BufRead, BufReader, pipe};
    use std::os::fd::AsRawFd;
    use std::process::Command;

    /// A descriptor open in this process without close-on-exec, which std's own
    /// opens never leave.
    fn inheritable() -> File {
        let file = File::open("/dev/null").expect("/dev/null opens");
        fcntl_setfd(&file, FdFlags::empty()).expect("close-on-exec clears");
        file
    }

    /// What a shell started as `how` (`None`: by std alone) finds of descriptor
    /// `fd`, `open` or `closed`, and whether its process group is its own.
    fn child_sees(fd: i32, how: Option<Spawn>) -> (String, bool) {
        let mut command = Command::new("sh");
        let (stdout, report_to) = pipe().expect("the report pipe");
        let (hold, held) = pipe().expect("the stdin pipe");
        command
            .args([
                "-c",
                &format!("(: <&{fd}) 2>/dev/null && echo open || echo closed; read line"),
            ])
            .stdin(hold)
            .stdout(report_to);
        let (id, mut end): (u32, Box<dyn FnMut()>) = match how {
            Some(how) => {
                let mut reaped = spawn(command, how).expect("the shell spawns");
                (
                    reaped.id(),
                    Box::new(move || {
                        reaped.wait().expect("the shell ends");
                    }),
                )
            }
            None => {
                let mut child = command.spawn().expect("the shell spawns");
                (
                    child.id(),
                    Box::new(move || {
                        child.wait().expect("the shell ends");
                    }),
                )
            }
        };
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("the shell answers");
        // The shell is blocked on its input, so its group is read while it lives.
        let pid = Pid::from_raw(i32::try_from(id).expect("a pid")).expect("a pid");
        let leads = getpgid(Some(pid)).expect("the group reads") == pid;
        drop(held);
        end();
        (line.trim().to_owned(), leads)
    }

    /// **A spawned helper inherits no descriptor above two, and a leader leads
    /// a process group of its own**: both calls the seam makes are shown by the
    /// child's own view. The control child, started by std alone, sees the
    /// descriptor and shares its parent's group.
    #[test]
    fn a_spawn_applies_the_group_and_the_descriptor_sweep() {
        let file = inheritable();
        let fd = file.as_raw_fd();
        assert!(fd >= 3);
        assert_eq!(child_sees(fd, None), ("open".to_owned(), false), "control");
        assert_eq!(
            child_sees(fd, Some(Spawn::Helper)),
            ("closed".to_owned(), false)
        );
        assert_eq!(
            child_sees(fd, Some(Spawn::Leader)),
            ("closed".to_owned(), true)
        );
    }
}
