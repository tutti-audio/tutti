//! What a spawned helper inherits: only descriptors 0, 1 and 2.
//!
//! One platform module is chosen here and nowhere else: Linux's `close_range`,
//! a descriptor sweep on the other unixes, and nothing on Windows.

#[cfg(unix)]
mod sweep;

#[cfg(target_os = "linux")]
#[path = "inherit/linux.rs"]
mod platform;
#[cfg(all(unix, not(target_os = "linux")))]
#[path = "inherit/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "inherit/windows.rs"]
mod platform;

pub use platform::inherit_only_stdio;

#[cfg(all(test, unix))]
mod tests {
    use super::inherit_only_stdio;
    use rustix::io::{FdFlags, fcntl_setfd};
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::process::{Command, Stdio};

    /// A descriptor open in this process without close-on-exec, which std's own
    /// opens never leave.
    fn inheritable() -> File {
        let file = File::open("/dev/null").expect("/dev/null opens");
        fcntl_setfd(&file, FdFlags::empty()).expect("close-on-exec clears");
        file
    }

    /// What the shell finds of descriptor `fd`: it reads from `<&fd` and
    /// answers `open` when the redirection succeeds.
    fn child_sees(fd: i32, configure: impl FnOnce(&mut Command)) -> String {
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                &format!("(: <&{fd}) 2>/dev/null && echo open || echo closed"),
            ])
            .stdout(Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().expect("the shell spawns");
        let mut out = String::new();
        child
            .stdout
            .take()
            .expect("piped")
            .read_to_string(&mut out)
            .expect("the shell answers");
        child.wait().expect("the shell ends");
        out.trim().to_owned()
    }

    /// **An inheritable descriptor of the parent is not the child's**: the
    /// control case shows the shell does see it without the call.
    #[test]
    fn a_child_inherits_no_descriptor_above_two() {
        let file = inheritable();
        let fd = file.as_raw_fd();
        assert!(fd >= 3);
        assert_eq!(child_sees(fd, |_| {}), "open", "the control child sees it");
        assert_eq!(child_sees(fd, inherit_only_stdio), "closed");
    }

    /// **Descriptor 0 of the child is what the parent set**: the hook runs
    /// after the standard descriptors are in place and leaves them.
    #[test]
    fn the_childs_standard_input_is_the_one_given() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        inherit_only_stdio(&mut command);
        let mut child = command.spawn().expect("the shell spawns");
        std::io::Write::write_all(&mut child.stdin.take().expect("piped"), b"tutti")
            .expect("the child reads its input");
        let mut out = String::new();
        child
            .stdout
            .take()
            .expect("piped")
            .read_to_string(&mut out)
            .expect("read");
        child.wait().expect("ends");
        assert_eq!(out, "tutti");
    }

    /// **A failed exec is still an error of `spawn`**: the hook only marks
    /// descriptors, so std's pipe that reports the exec failure stays open to
    /// the exec.
    #[test]
    fn a_missing_program_is_still_a_spawn_error() {
        let mut command = Command::new("/nonexistent/tutti-no-such-program");
        inherit_only_stdio(&mut command);
        assert!(command.spawn().is_err());
    }
}
