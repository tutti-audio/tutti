//! Unix: the server leads a process group of its own, so the group's end
//! reaches the helper processes a plugin spawned.

use rustix::process::{Pid, Signal};
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Makes `command` start its process as the leader of a new process group.
pub fn own_group(command: &mut Command) {
    command.process_group(0);
}

/// Ends every process of the group `leader` leads (`SIGKILL`). Call it while
/// the leader is unreaped (or just reaped): a reaped leader's id may name an
/// unrelated process later. A group that is already gone is not an error.
pub fn kill_group(leader: u32) {
    if let Some(pid) = i32::try_from(leader).ok().and_then(Pid::from_raw) {
        let _ = rustix::process::kill_process_group(pid, Signal::KILL);
    }
}

/// Whether the child `leader` has ended and is not yet reaped, without reaping
/// it: a zombie keeps its id, so its group can still be ended by it
/// ([`kill_group`]) before the reaping lets the id go.
#[must_use]
pub fn has_ended(leader: u32) -> bool {
    use rustix::process::{WaitId, WaitIdOptions};
    let Some(pid) = i32::try_from(leader).ok().and_then(Pid::from_raw) else {
        return false;
    };
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    matches!(
        rustix::process::waitid(WaitId::Pid(pid), options),
        Ok(Some(_))
    )
}

// A loom build leaves the executor out (see `tutti-platform`), and this test with it.
#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// **A process that leads its group takes the group's other members with
    /// it**: the shell's background `sleep` is in the shell's group, and ends
    /// when the group is killed.
    #[test]
    fn killing_a_group_ends_the_helpers_its_leader_spawned() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 60 & echo $!; wait"])
            .stdout(Stdio::piped());
        own_group(&mut command);
        let mut leader = command.spawn().expect("the shell spawns");
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(leader.stdout.take().expect("piped")),
            &mut line,
        )
        .expect("the shell prints its helper's pid");
        let helper: u32 = line.trim().parse().expect("a pid");
        let (ended, helper_ended) = std::sync::mpsc::channel();
        let executor = tutti_platform::Executor::new();
        let _watch = crate::watch_host(&executor, helper, move || {
            ended.send(()).expect("listening");
        })
        .expect("the watch on the helper starts");

        kill_group(leader.id());
        leader.wait().expect("the leader ends");
        helper_ended.recv().expect("the helper ends with its group");
    }
}
