//! What a spawned helper inherits: only descriptors 0, 1 and 2.
//!
//! One platform module is chosen here and nowhere else: Linux's `close_range`,
//! a descriptor sweep on the other unixes. Windows has no such step: its spawn
//! (`spawn/windows.rs`) states what a child inherits there.

mod sweep;

#[cfg(target_os = "linux")]
#[path = "inherit/linux.rs"]
mod platform;
#[cfg(not(target_os = "linux"))]
#[path = "inherit/unix.rs"]
mod platform;

pub(crate) use platform::inherit_only_stdio;

#[cfg(test)]
mod tests {
    use super::inherit_only_stdio;
    use std::io::Read;
    use std::process::{Command, Stdio};

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
