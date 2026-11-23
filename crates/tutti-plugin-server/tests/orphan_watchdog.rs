//! The host watch, exercised against the real `plugin-server` binary.
//!
//! # What this file covers that the unit tests cannot
//!
//! `tutti-plugin-ipc-ffi`'s `watch_host` tests pin that the OS's word of a
//! process's end reaches a function. They say nothing about whether the server
//! starts the watch, names the right process, or ends when the watch fires,
//! wherever it is then blocked. Those need a real process tree, which is what
//! lives here.
//!
//! # Why a shell as the throwaway parent
//!
//! The orphaning has to happen without this test process being the server's
//! parent, or the host the server was told about would never die. So
//! `sh -c 'spawn-in-background; ...'` is the intermediate: it spawns the
//! server, states its own PID as the host, and prints the server's PID. The
//! test then knows the server only by PID, which is why liveness is probed
//! with `kill(pid, 0)` rather than with `Child::try_wait` — and why the probe
//! has to reap first (see [`has_exited`]).
//!
//! # Bounding
//!
//! Nothing here sleeps for a fixed duration and then asserts. Each wait is on
//! an event, bounded by a named hang guard, and blowing the guard is itself the
//! failure message.

#![cfg(unix)]

use std::io::BufRead as _;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;
use tutti_plugin_ipc::Shutdown;

use tutti_plugin_ipc::{ControlFrame, read_frame};

/// The guard on an orphaned server's exit: it is a hang guard, and a server
/// that watches the wrong process never exits at all, so it fails whatever this
/// number is.
const EXIT_DEADLINE: Duration = Duration::from_secs(60);

/// Has the process `pid` exited?
///
/// **A zombie counts as exited, and getting that wrong produced a false
/// failure while this file was being written.** `kill(pid, 0)` succeeds for a
/// zombie — the PID stays allocated until someone reaps it — so a naive
/// existence probe reports a server that returned from `main` milliseconds ago
/// as still running. The subreaper test below is itself the process that
/// adopts the orphaned server, so until it reaped, a correctly-exiting server
/// looked stranded.
///
/// The reaping therefore happens first (see [`reap_any`]) and the probe after.
/// `EPERM` means the process exists but is not ours to signal, which is still
/// "not exited".
fn has_exited(pid: i32) -> bool {
    reap_any();
    // SAFETY: `kill` with signal 0 sends nothing; it only probes. Scalar
    // arguments, no memory touched.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
}

/// Reap every exited child, without blocking.
///
/// Needed because the subreaper test *adopts* the orphaned server: an adopted
/// child that exits stays a zombie, holding its PID, until its new parent
/// waits on it. `WNOHANG` makes this safe to call from a poll loop, and
/// reaping a process this test did not itself spawn is exactly the point.
fn reap_any() {
    // SAFETY: `waitpid` with `WNOHANG` returns immediately and writes nothing
    // through the null status pointer — the exit status is not wanted here.
    while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
}

/// Make sure a stray server does not outlive a failing test.
struct Reaper(i32);

impl Drop for Reaper {
    fn drop(&mut self) {
        // SAFETY: scalar arguments; a stale PID simply returns ESRCH.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
    }
}

/// A socket pair: the host's end, and the server's end as the standard input
/// a command is spawned with.
fn connection() -> (UnixStream, Stdio) {
    let (host, server) = UnixStream::pair().expect("a socket pair");
    (host, Stdio::from(OwnedFd::from(server)))
}

/// Spawn `plugin-server` from a shell that exits immediately, and return the
/// server's PID.
///
/// The shell is the throwaway parent: by the time this returns, the server's
/// original parent is gone, and the server has been reparented to whoever the
/// reaper is on this system.
///
/// The shell states its own PID in `HOST_PID_ENV`, exactly as
/// `tutti_plugin`'s `spawn_process` does, which is what makes it a fair
/// stand-in for a real host.
///
/// The shell must not linger: the host dying *during* the child's startup is
/// the scenario.
///
/// The host's end of the connection is returned, and the caller holds it: the
/// server then sees no end of stream and no request, so only the watch on the
/// host process can end it. (A shell may point a background command's standard
/// input at `/dev/null` before its own redirections, as dash does: the connection
/// moves to descriptor 3 first and `<&3` hands it to the server.)
fn spawn_orphaned_server() -> (Reaper, UnixStream) {
    let exe = env!("CARGO_BIN_EXE_plugin-server");
    let (host, server) = connection();
    let script = format!(
        // `exec` is deliberately absent: the shell must remain a distinct
        // process that spawns the server and *then* dies, which is the
        // orphaning. `$$` is the shell's own PID — the host, as far as the
        // server is concerned. `echo $!` hands the server's PID back; the
        // shell then exits at once.
        "exec 3<&0; {env}=$$ {exe:?} <&3 3<&- >/dev/null 2>&1 & echo $!",
        env = tutti_plugin_ipc_ffi::HOST_PID_ENV
    );
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .stdin(server)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawning the throwaway parent shell");
    let pid: i32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("the shell should print the server's PID");
    (Reaper(pid), host)
}

/// (a) The parent dies → the server exits.
///
/// This is the defect the host watch exists for: one orphaned subprocess
/// per loaded plugin, every time a DAW crashes or is force-quit.
#[test]
fn an_orphaned_server_exits_without_a_host() {
    let (server, _host) = spawn_orphaned_server();

    tutti_fixtures::wait_until(
        &format!(
            "server {} to exit after its parent exited; the host watch never fired",
            server.0
        ),
        EXIT_DEADLINE,
        || has_exited(server.0),
    );
}

/// Reads the server's `Hello` from the host's end of its connection: the server
/// is then past its start and greeted this connection. Panics if the server ends
/// first (the connection ends with it).
fn greeted(host: &mut UnixStream) {
    assert!(
        matches!(
            read_frame::<_, ControlFrame>(host),
            Ok(ControlFrame::Hello { .. })
        ),
        "the server ended before it greeted its host"
    );
}

/// (b) The parent is alive → the server keeps serving.
///
/// The safe direction of the tradeoff, and the one a too-eager watch breaks: a
/// host that is slow to connect (loading a large plugin, a cold page cache)
/// must not have its server quit underneath it. The server here is a direct
/// child of this test process, which stays alive: it greets the connection it was
/// given, which an ended server cannot.
///
/// **This test also carries the file's premise.** The others assert that a
/// server *exits*, which a server crashing on startup satisfies just as well (a
/// missing library, a standard input that is not a socket). Only here is a
/// server required to greet its host, so only here does the suite prove that
/// `plugin-server` gets that far. If this one fails, read the others as vacuous.
#[test]
fn a_server_with_a_live_parent_serves_its_host() {
    let exe = env!("CARGO_BIN_EXE_plugin-server");
    let (mut host, server) = connection();
    let mut child = Command::new(exe)
        .stdin(server)
        .env(
            tutti_plugin_ipc_ffi::HOST_PID_ENV,
            std::process::id().to_string(),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the server as our own child");

    greeted(&mut host);
    assert!(
        child.try_wait().expect("polling").is_none(),
        "the server ended while its host was running"
    );
    let _ = child.kill();
    let _ = child.wait();
}

/// **A server behind a wrapper does not take the wrapper for an ended host.** A
/// shell that runs the server without `exec` (a wrapper script, `strace`, a
/// sandbox) is the server's parent, and the host that stated its pid is alive.
/// The server is asked to shut down by the host, and its log must not say that its
/// host was gone: the watch logs that the moment it fires, so it cannot have
/// fired while the server answered.
#[test]
fn a_server_behind_a_wrapper_does_not_take_its_host_for_gone() {
    let exe = env!("CARGO_BIN_EXE_plugin-server");
    let (mut host, server) = connection();
    let dir = std::env::temp_dir().join(format!("tw-wrapper-log-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let log = dir.join("server.log");
    // `true` after it keeps the shell from replacing itself with the server.
    let mut wrapper = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{exe:?} >{log:?} 2>&1; true"))
        .stdin(server)
        .env("RUST_LOG", "info")
        .env(
            tutti_plugin_ipc_ffi::HOST_PID_ENV,
            std::process::id().to_string(),
        )
        .spawn()
        .expect("spawning the wrapper");

    greeted(&mut host);
    tutti_plugin_ipc::write_frame(
        &mut host,
        &tutti_plugin_ipc::Request {
            id: tutti_plugin_ipc::ReqId::new(1),
            body: tutti_plugin_ipc::ControlRequest::Shutdown(Shutdown),
        },
    )
    .expect("ask the server to shut down");
    assert!(wrapper.wait().expect("the wrapper ends").success());

    let printed = std::fs::read_to_string(&log).expect("the server's log");
    assert!(
        !printed.contains("the host process is gone"),
        "the server took its running host for gone: {printed}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// (e) **A host that ends after it greeted ends the server, though the
/// connection stays open.** A watch that covered only the start leaves a server
/// that is past it, parked reading the handshake, an orphan for ever: here this
/// test process holds the host's end of the connection open, so the server sees
/// no end of stream, and only the watch on the host process can end it.
#[test]
fn a_host_that_ends_after_connecting_ends_the_server() {
    let exe = env!("CARGO_BIN_EXE_plugin-server");
    let (mut connection, server) = connection();
    // The shell is the host until this test kills it: it prints the server's PID
    // and waits. Its standard input is the server's connection, so it waits on
    // `sleep` and not on a read.
    let script = format!(
        "exec 3<&0; {env}=$$ {exe:?} <&3 3<&- >/dev/null 2>&1 & echo $!; exec sleep 600",
        env = tutti_plugin_ipc_ffi::HOST_PID_ENV
    );
    let mut host = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .stdin(server)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the host shell");
    let mut line = String::new();
    std::io::BufReader::new(host.stdout.take().expect("piped"))
        .read_line(&mut line)
        .expect("the shell prints the server's PID");
    let server = Reaper(line.trim().parse().expect("the server's PID"));

    // Greeted: the server is past its start.
    greeted(&mut connection);

    // The host ends; its connection to the server stays open in this process.
    host.kill().expect("the host shell is ended");
    host.wait().expect("the host shell is reaped");
    tutti_fixtures::wait_until(
        &format!("server {} to end with its host, past its start", server.0),
        EXIT_DEADLINE,
        || has_exited(server.0),
    );
}

/// The subreaper case needs `prctl` and `/proc`: Linux's.
#[cfg(target_os = "linux")]
mod subreaper {
    use super::*;

    /// (c) The subreaper case.
    ///
    /// This test process calls `prctl(PR_SET_CHILD_SUBREAPER, 1)`, so when the
    /// throwaway shell dies the orphaned server is reparented to **this test
    /// process**, not to pid 1. A watch written as `getppid() == 1` sees a
    /// perfectly ordinary non-1 parent, concludes the host is alive, and waits
    /// forever. The host's own statement of its PID, watched by the OS, is what
    /// notices the host ended.
    ///
    /// The server's post-orphan ppid is asserted to be this test process (proving
    /// the subreaper took effect and pid 1 is not involved), and *then* the server
    /// is required to exit anyway.
    #[test]
    fn a_subreaper_does_not_hide_the_hosts_death() {
        // SAFETY: `prctl` with `PR_SET_CHILD_SUBREAPER` takes scalars and affects
        // only this process's reparenting behaviour. Setting it is not undone, but
        // it is harmless for the rest of the suite: it only means orphaned
        // grandchildren of the test binary are adopted here rather than by init,
        // and the test binary reaps nothing else.
        let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) };
        assert_eq!(rc, 0, "PR_SET_CHILD_SUBREAPER failed: this test needs it");

        let (server, _host) = spawn_orphaned_server();
        let me = std::process::id() as i32;

        // First: prove the premise. Once the shell is gone, the server must have
        // been adopted by *us*. If this never becomes true the environment did not
        // do what the test assumes, and the exit assertion below would prove
        // nothing about the subreaper case.
        tutti_fixtures::wait_until(
            &format!(
                "server {} to be reparented to this test process ({me}); \
                 PR_SET_CHILD_SUBREAPER did not take effect if it never is, so \
                 this run does not exercise the subreaper case at all",
                server.0
            ),
            EXIT_DEADLINE,
            || ppid_of(server.0) == Some(me),
        );

        // Then: the server must exit regardless. Its parent is a live, non-1
        // process — the exact configuration in which `getppid() == 1` is silent.
        tutti_fixtures::wait_until(
            &format!(
                "server {} to exit after being orphaned under a subreaper: its ppid became \
                 {me} rather than 1, so a watch comparing against pid 1 sees \
                 nothing wrong; the watch on the host's own PID is what must catch this",
                server.0
            ),
            EXIT_DEADLINE,
            || has_exited(server.0),
        );
    }

    /// The parent PID of `pid`, from `/proc`. Linux-only, which matches the
    /// subreaper test's own gate.
    ///
    /// Field 4 of `/proc/<pid>/stat` is the ppid, but field 2 is the executable
    /// name in parentheses and may itself contain spaces or parentheses — so the
    /// split starts after the final `)`, never at the first space.
    fn ppid_of(pid: i32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = &stat[stat.rfind(')')? + 1..];
        after_comm.split_whitespace().nth(1)?.parse().ok()
    }
}

/// (d) The host watch ends nothing but its own process.
///
/// The server ends itself and signals no one else. An unrelated bystander
/// process is left running for the whole of an orphaned server's exit, and
/// must be untouched afterwards: this covers the blast radius.
#[test]
fn the_host_watch_takes_down_only_its_own_process() {
    // A bystander with no relationship to the server: a long sleep, spawned by
    // this test, that nothing in the plugin system knows about.
    let mut bystander = Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the bystander");
    let bystander_pid = bystander.id() as i32;

    let (server, _host) = spawn_orphaned_server();
    tutti_fixtures::wait_until(
        &format!("server {} to exit", server.0),
        EXIT_DEADLINE,
        || has_exited(server.0),
    );

    // Probed by PID rather than with `try_wait`, because `has_exited` above
    // reaps every exited child indiscriminately — including this one, were it
    // to die. `kill(pid, 0)` after that reaping is the honest question: a
    // bystander the watch signalled is both dead *and* reaped, so it
    // answers ESRCH, while a live one answers success.
    // SAFETY: signal 0 sends nothing; scalar arguments, no memory touched.
    let bystander_survived = unsafe { libc::kill(bystander_pid, 0) } == 0;
    let _ = bystander.kill();
    let _ = bystander.wait();

    assert!(
        bystander_survived,
        "bystander {bystander_pid} died while the orphaned server was exiting; \
         the watch signalled a process other than itself"
    );
}
