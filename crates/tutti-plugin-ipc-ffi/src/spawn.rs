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

/// What a spawned helper is to the host, and the one entry for its standard
/// streams on every platform.
///
/// A helper's `stdin`, `stdout` and `stderr` are `NUL` and a leader's `stdout`
/// and `stderr` are the host's, whatever the command sets: the command's own
/// `stdout` and `stderr` (and a helper's `stdin`) are read on no platform. A
/// leader's `stdin` is the caller's: on unix the command's own (the transport's
/// channel to the server), on Windows the host's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Spawn {
    /// A helper that ends by itself or by its owner's kill, with no standard
    /// input or output of the host's.
    Helper,
    /// A server, which leads the process group (unix) or sits in the host's
    /// kill-on-close job (Windows) that ends it, and the helpers a plugin
    /// spawned, with the host; on Windows it is in the job before it runs. It
    /// writes to the host's standard output and error.
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
    use std::io::pipe;
    use std::os::fd::AsRawFd;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A descriptor open in this process without close-on-exec, which std's own
    /// opens never leave.
    fn inheritable() -> File {
        let file = File::open("/dev/null").expect("/dev/null opens");
        fcntl_setfd(&file, FdFlags::empty()).expect("close-on-exec clears");
        file
    }

    /// Runs `script` in a shell started as `how` (`None`: by std alone) with the
    /// report file's path as `$1` and a fifo's as `$2`. The script ends by
    /// reading the fifo, so the shell lives while `inspect` runs on its pid
    /// and the report file's text. The shell's standard streams are the
    /// command's own, which `how` may override: the report goes through a file.
    fn run<T>(
        script: &str,
        how: Option<Spawn>,
        mut command_with: impl FnMut(&mut Command),
        inspect: impl FnOnce(u32, &str) -> T,
    ) -> T {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tutti-spawn-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).expect("a directory");
        let report = dir.join("report");
        let fifo = dir.join("hold");
        let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
        assert!(made.success(), "mkfifo {}", fifo.display());
        let mut command = Command::new("sh");
        command
            .args(["-c", &format!("{script}\nread line <\"$2\""), "sh"])
            .arg(&report)
            .arg(&fifo);
        command_with(&mut command);
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
        // The shell opens the fifo for reading after it wrote the report, so the
        // open returns once the report is complete and the shell is blocked.
        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&fifo)
            .expect("the fifo opens");
        let text = std::fs::read_to_string(&report).expect("the report");
        let seen = inspect(id, &text);
        drop(held);
        end();
        std::fs::remove_dir_all(&dir).expect("the directory goes");
        seen
    }

    /// What a shell started as `how` finds of descriptor `fd`, `open` or
    /// `closed`, and whether its process group is its own.
    fn child_sees(fd: i32, how: Option<Spawn>) -> (String, bool) {
        let script = format!("(: <&{fd}) 2>/dev/null && echo open >\"$1\" || echo closed >\"$1\"");
        run(
            &script,
            how,
            |_| {},
            |id, text| {
                let pid = Pid::from_raw(i32::try_from(id).expect("a pid")).expect("a pid");
                let leads = getpgid(Some(pid)).expect("the group reads") == pid;
                (text.trim().to_owned(), leads)
            },
        )
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

    /// `/proc` names each descriptor's file, which only Linux has.
    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;

        /// What the shell's descriptors 0, 1 and 2 are, as the kernel names them.
        fn links(how: Spawn, command_with: impl FnMut(&mut Command)) -> Vec<String> {
            let script = "a=$(readlink /proc/$$/fd/0); b=$(readlink /proc/$$/fd/1); \
                          c=$(readlink /proc/$$/fd/2); printf '%s\\n%s\\n%s\\n' \"$a\" \"$b\" \"$c\" >\"$1\"";
            run(script, Some(how), command_with, |_, text| {
                text.lines().map(str::to_owned).collect()
            })
        }

        fn host_link(fd: i32) -> String {
            std::fs::read_link(Path::new("/proc/self/fd").join(fd.to_string()))
                .expect("the host's descriptor")
                .to_string_lossy()
                .into_owned()
        }

        /// **The spawn kind, not the command, decides a helper's standard streams**:
        /// a helper's three are `/dev/null` though the command set pipes, and a
        /// leader's `stdout` and `stderr` are the host's though the command set
        /// pipes, while its `stdin` stays the command's own pipe.
        #[test]
        fn the_kind_decides_the_standard_streams() {
            let pipes = |command: &mut Command| {
                let (_, a) = pipe().expect("a pipe");
                let (_, b) = pipe().expect("a pipe");
                let (c, _) = pipe().expect("a pipe");
                command.stdin(c).stdout(a).stderr(b);
            };
            let helper = links(Spawn::Helper, pipes);
            assert_eq!(helper, ["/dev/null"; 3]);

            let (held, _write) = pipe().expect("a pipe");
            let own = host_link(held.as_raw_fd());
            let leader = links(Spawn::Leader, |command| {
                let (_, a) = pipe().expect("a pipe");
                let (_, b) = pipe().expect("a pipe");
                command
                    .stdin(held.try_clone().expect("a clone"))
                    .stdout(a)
                    .stderr(b);
            });
            assert_eq!(leader, [own, host_link(1), host_link(2)]);
        }
    }
}
