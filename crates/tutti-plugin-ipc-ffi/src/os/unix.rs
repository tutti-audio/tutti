//! The operating-system calls of the plugin process edge on unix, each behind a
//! safe function: the server's wait for its host to connect and its check that
//! the host is still running.
//!
//! Each is a direct call with scalar or borrowed arguments; the policy around
//! it (what a failure to ask means, how long to wait) stays with the caller.

use rustix::event::{PollFd, PollFlags, Timespec};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::time::Duration;

use crate::{Interest, Ready, Watch};

/// The `Timespec` `poll` waits for `timeout`, rounded up to a whole millisecond
/// (a wait of 0.3 ms must not become a busy `poll(0)` loop) and capped at
/// `i32::MAX` ms, the most a platform that takes `poll`'s timeout as a `c_int`
/// of milliseconds accepts; `None` waits for ever.
fn timespec(timeout: Option<Duration>) -> Option<Timespec> {
    timeout.map(|timeout| {
        let ms = u64::try_from(timeout.as_nanos().div_ceil(1_000_000))
            .unwrap_or(u64::MAX)
            .min(u64::from(i32::MAX.cast_unsigned()));
        Timespec {
            tv_sec: (ms / 1000) as _,
            tv_nsec: ((ms % 1000) * 1_000_000) as _,
        }
    })
}

/// Waits up to `timeout` (`None`: for ever) for `fd` to become readable (for a
/// listener: a connection is queued). `Ok(false)` on a timeout.
///
/// # Errors
///
/// The OS error of `poll`, [`Interrupted`](std::io::ErrorKind::Interrupted)
/// included: a signal during the wait is the caller's to retry.
pub fn wait_readable(fd: BorrowedFd<'_>, timeout: Option<Duration>) -> std::io::Result<bool> {
    let mut polls = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
    Ok(rustix::event::poll(&mut polls, timespec(timeout).as_ref())? > 0)
}

/// Which of the two descriptors [`wait_readable_either`] waited on are
/// readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Readable {
    /// The first descriptor is readable (or hung up: the read reports it).
    pub first: bool,
    /// The second descriptor is readable (or hung up).
    pub second: bool,
}

/// Waits up to `timeout` (`None`: for ever) for either of two descriptors to
/// become readable, in one `poll(2)` that allocates nothing: the wait of a
/// thread that serves a connection and is woken by a bell's descriptor.
/// Both `false` on a timeout.
///
/// # Errors
///
/// The OS error of `poll`, [`Interrupted`](std::io::ErrorKind::Interrupted)
/// included: a signal during the wait is the caller's to retry.
pub fn wait_readable_either(
    first: BorrowedFd<'_>,
    second: BorrowedFd<'_>,
    timeout: Option<Duration>,
) -> std::io::Result<Readable> {
    let mut polls = [
        PollFd::from_borrowed_fd(first, PollFlags::IN),
        PollFd::from_borrowed_fd(second, PollFlags::IN),
    ];
    rustix::event::poll(&mut polls, timespec(timeout).as_ref())?;
    Ok(Readable {
        first: !polls[0].revents().is_empty(),
        second: !polls[1].revents().is_empty(),
    })
}

/// Waits on one descriptor of the caller's own and any number of others a
/// plugin asked to have watched, in one `poll(2)`.
///
/// Waits up to `timeout` (`None`: for ever) for `own` to be readable (a hang-up
/// counts: the read reports it) or for any of `watched` to be ready, and writes
/// the ready ones, with only the events they asked for, into `ready` (replacing
/// its contents). Returns whether `own` is readable; `false` on a timeout.
///
/// A descriptor that is not open answers [`Interest::ERROR`], never a fault. A
/// zero timeout never blocks.
///
/// A plugin's descriptor arrives as a plain number, and borrowing it is the one
/// `unsafe` of this function.
///
/// # Errors
///
/// The OS error of `poll`, [`Interrupted`](std::io::ErrorKind::Interrupted)
/// included: a signal during the wait is the caller's to retry.
pub fn poll_wait(
    own: Option<BorrowedFd<'_>>,
    watched: &[Watch],
    timeout: Option<Duration>,
    ready: &mut Vec<Ready>,
) -> std::io::Result<bool> {
    ready.clear();
    let mut polls: Vec<PollFd<'_>> = Vec::with_capacity(watched.len() + 1);
    polls.extend(own.map(|own| PollFd::from_borrowed_fd(own, PollFlags::IN)));
    let skip = polls.len();
    polls.extend(watched.iter().map(|&Watch { fd, interest }| {
        let mut events = PollFlags::empty();
        if interest.contains(Interest::READ) {
            events |= PollFlags::IN;
        }
        if interest.contains(Interest::WRITE) {
            events |= PollFlags::OUT;
        }
        // SAFETY: the descriptor is a plain number the plugin registered with
        // the host, borrowed only for this one `poll` call, which its
        // registration keeps open; if it is not open `poll` answers `NVAL` and
        // nothing is read from or written to it.
        let fd = unsafe { BorrowedFd::borrow_raw(fd.get()) };
        PollFd::from_borrowed_fd(fd, events)
    }));
    if rustix::event::poll(&mut polls, timespec(timeout).as_ref())? == 0 {
        return Ok(false);
    }
    let own_ready = skip == 1 && !polls[0].revents().is_empty();
    ready.extend(polls[skip..].iter().zip(watched).filter_map(
        |(poll, &Watch { fd, interest })| {
            let got = poll.revents();
            let mut now = Interest::NONE;
            if got.intersects(PollFlags::IN | PollFlags::HUP) {
                now = now.union(Interest::READ);
            }
            if got.contains(PollFlags::OUT) {
                now = now.union(Interest::WRITE);
            }
            if got.intersects(PollFlags::ERR | PollFlags::NVAL) {
                now = now.union(Interest::ERROR);
            }
            let events = now.intersection(interest);
            (!events.is_empty()).then_some(Ready { fd, events })
        },
    ));
    Ok(own_ready)
}

/// Looks, without waiting, at which of `watched` are ready, into `ready`
/// (replacing its contents), through [`poll_wait`]. A failed look is none
/// ready: the descriptors are looked at again on the next call.
pub fn poll_now(watched: &[Watch], ready: &mut Vec<Ready>) {
    ready.clear();
    if poll_wait(None, watched, Some(Duration::ZERO), ready).is_err() {
        ready.clear();
    }
}

/// Whether `pid` names a live process, without signalling it.
///
/// A number that cannot be a process id (zero, or past `pid_t`) names no
/// process.
///
/// `kill(pid, 0)` runs the permission and existence checks and delivers
/// nothing. `EPERM` means it exists but is not ours to signal, which is still
/// "alive"; only `ESRCH` is a definite death. Anything else is a failure to
/// ask, and a failure to ask counts as alive, erring toward waiting rather than
/// toward a server that quits on a live host.
#[must_use]
pub fn process_exists(pid: u32) -> bool {
    use rustix::io::Errno;
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    !matches!(rustix::process::test_kill_process(pid), Err(Errno::SRCH))
}

/// Ends this process at once with `SIGKILL`, without running a plugin
/// library's exit handlers or any other thread's cleanup, for a server whose
/// host is gone: a plain `exit` would run every loaded library's destructors
/// while other threads are mid-call. Aborts if the OS refuses.
pub fn terminate_process() -> ! {
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
    std::process::abort()
}

/// This process's parent PID, `None` once reparented to init (a PID of 1).
#[must_use]
pub fn parent_process_id() -> Option<u32> {
    Some(std::os::unix::process::parent_id()).filter(|&pid| pid > 1)
}

/// Why the socket the host handed this process on its stdin cannot be adopted.
#[derive(Debug, thiserror::Error)]
pub enum AdoptError {
    /// Descriptor 0 is not a socket: the process was started by hand, or by a
    /// launcher that did not hand it the host's end.
    #[error(
        "standard input is not a socket: the plugin server is started by its host, \
         which hands it the connection on stdin"
    )]
    NotASocket,
    /// Descriptor 0 is a socket that is not a stream.
    #[error("standard input is a socket of type {found:?}, not a stream")]
    NotAStream {
        /// The socket's type.
        found: rustix::net::SocketType,
    },
    /// The OS refused one of the calls that duplicate, inspect or replace the
    /// descriptor.
    #[error("failed to {op} standard input: {source}")]
    Os {
        /// What was being done, as a verb phrase.
        op: &'static str,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Takes the connection the host handed this process as its standard input: the
/// stream socket on descriptor 0 moves to a descriptor of its own, closed on
/// `exec`, and `/dev/null` replaces descriptor 0, so a plugin that reads its
/// standard input reads end of file and no process a plugin starts inherits the
/// socket.
///
/// Call it first thing in `main`, before any thread starts: replacing descriptor 0
/// is process-wide. A refusal leaves descriptor 0 as it was.
///
/// # Errors
///
/// [`AdoptError::NotASocket`] and [`AdoptError::NotAStream`] when descriptor 0 is
/// not a stream socket, [`AdoptError::Os`] when the OS refuses a call.
pub fn adopt_host_socket() -> Result<OwnedFd, AdoptError> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, open};
    use rustix::io::fcntl_dupfd_cloexec;
    use rustix::net::SocketType;
    use rustix::net::sockopt::socket_type;
    use rustix::stdio::{dup2_stdin, stdin};

    let os = |op| {
        move |errno: rustix::io::Errno| AdoptError::Os {
            op,
            source: errno.into(),
        }
    };
    let stat = fstat(stdin()).map_err(os("inspect"))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Socket {
        return Err(AdoptError::NotASocket);
    }
    let found = socket_type(stdin()).map_err(os("inspect"))?;
    if found != SocketType::STREAM {
        return Err(AdoptError::NotAStream { found });
    }
    // From 3: a duplicate that took the number of a closed standard descriptor
    // would be replaced with it.
    let socket = fcntl_dupfd_cloexec(stdin(), 3).map_err(os("duplicate"))?;
    let null = open("/dev/null", OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .map_err(os("open /dev/null for"))?;
    dup2_stdin(&null).map_err(os("replace"))?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FdId;

    /// This process exists; a PID far past `pid_max` does not; a number that is
    /// no `pid_t` (zero, past `i32::MAX`) names no process.
    #[test]
    fn process_exists_tells_a_live_pid_from_a_dead_one() {
        assert!(process_exists(std::process::id()));
        assert!(!process_exists(i32::MAX as u32));
        assert!(!process_exists(u32::MAX));
        assert!(!process_exists(0));
    }

    /// **Either descriptor ends the wait and is the one reported.**
    #[test]
    fn wait_readable_either_reports_the_descriptor_that_is_readable() {
        use std::io::Write;
        use std::os::fd::AsFd;
        let (first, mut first_writer) = std::io::pipe().unwrap();
        let (second, mut second_writer) = std::io::pipe().unwrap();
        let wait = |timeout| wait_readable_either(first.as_fd(), second.as_fd(), timeout).unwrap();
        assert_eq!(
            wait(Some(Duration::ZERO)),
            Readable {
                first: false,
                second: false
            }
        );
        second_writer.write_all(b"x").unwrap();
        assert_eq!(
            wait(None),
            Readable {
                first: false,
                second: true
            }
        );
        first_writer.write_all(b"x").unwrap();
        assert_eq!(
            wait(None),
            Readable {
                first: true,
                second: true
            }
        );
    }

    /// A pipe's read end is not readable until something is written to it.
    #[test]
    fn wait_readable_waits_for_data() {
        use std::io::Write;
        use std::os::fd::AsFd;
        let (reader, mut writer) = std::io::pipe().unwrap();
        assert!(!wait_readable(reader.as_fd(), Some(Duration::ZERO)).unwrap());
        writer.write_all(b"x").unwrap();
        assert!(wait_readable(reader.as_fd(), Some(Duration::from_secs(1))).unwrap());
        assert!(wait_readable(reader.as_fd(), None).unwrap());
    }

    /// A timeout is rounded up to whole milliseconds, so a sub-millisecond wait
    /// is never a zero one; one past `i32::MAX` ms is capped there; `None` has
    /// no timeout at all.
    #[test]
    fn a_timeout_rounds_up_to_a_millisecond_and_is_capped() {
        let spec = |t| timespec(Some(t)).map(|t| (t.tv_sec, t.tv_nsec));
        assert_eq!(spec(Duration::ZERO), Some((0, 0)));
        assert_eq!(spec(Duration::from_nanos(1)), Some((0, 1_000_000)));
        assert_eq!(spec(Duration::from_micros(300)), Some((0, 1_000_000)));
        assert_eq!(spec(Duration::from_micros(1001)), Some((0, 2_000_000)));
        assert_eq!(spec(Duration::from_millis(2500)), Some((2, 500_000_000)));
        let max_ms = i64::from(i32::MAX);
        assert_eq!(
            spec(Duration::MAX),
            Some((max_ms / 1000, (max_ms % 1000) * 1_000_000))
        );
        assert!(timespec(None).is_none());
    }

    /// **A watched descriptor is reported when it is readable and not before,
    /// with the bits it asked for; the caller's own descriptor is reported
    /// apart.** The instant a byte arrives ends a long wait.
    #[test]
    fn poll_wait_reports_a_watched_descriptor_when_it_is_readable() {
        use std::io::Write;
        use std::os::fd::{AsFd, AsRawFd};
        let (watched, mut writer) = std::io::pipe().unwrap();
        let (own, _own_writer) = std::io::pipe().unwrap();
        let mut ready = Vec::new();
        let fd = FdId::new(watched.as_raw_fd()).unwrap();
        let read = [Watch {
            fd,
            interest: Interest::READ,
        }];
        let own_ready =
            poll_wait(Some(own.as_fd()), &read, Some(Duration::ZERO), &mut ready).unwrap();
        assert!(!own_ready && ready.is_empty(), "nothing yet");
        writer.write_all(b"x").unwrap();
        // A timeout past the test harness's limit: a poll that ran it out fails.
        let own_ready = poll_wait(
            Some(own.as_fd()),
            &read,
            Some(Duration::from_secs(600)),
            &mut ready,
        )
        .unwrap();
        assert!(!own_ready, "the caller's own descriptor is apart");
        assert_eq!(
            ready,
            [Ready {
                fd,
                events: Interest::READ
            }]
        );
        // A write interest it did not ask for is not reported; a closed
        // descriptor is an error, not a fault.
        poll_wait(
            None,
            &[Watch {
                fd,
                interest: Interest::WRITE,
            }],
            Some(Duration::ZERO),
            &mut ready,
        )
        .unwrap();
        assert!(ready.is_empty(), "a pipe's read end is never writable");
        // A descriptor that is not open is an error for whoever asked for
        // errors, never a fault.
        drop(watched);
        poll_wait(
            None,
            &[Watch {
                fd,
                interest: Interest::READ.union(Interest::ERROR),
            }],
            Some(Duration::ZERO),
            &mut ready,
        )
        .unwrap();
        assert_eq!(
            ready,
            [Ready {
                fd,
                events: Interest::ERROR
            }]
        );
    }

    /// Descriptor 0 is the whole process's: these two tests replace it, so each
    /// runs in a process of its own (nextest's way) and never beside another test.
    mod adopt {
        use super::*;
        use rustix::io::{FdFlags, fcntl_getfd};
        use rustix::stdio::dup2_stdin;
        use std::io::{Read, Write};
        use std::os::unix::net::{UnixDatagram, UnixStream};

        /// **The adopted descriptor is the socket that was on descriptor 0, and
        /// descriptor 0 is `/dev/null` after**: the adopted end talks to the peer,
        /// is closed on `exec`, and a read of standard input finds end of file.
        #[test]
        fn adopt_takes_fd0_and_leaves_devnull() {
            let (mut host, server) = UnixStream::pair().unwrap();
            dup2_stdin(&server).unwrap();
            drop(server);

            let adopted = adopt_host_socket().expect("a stream socket on fd 0");
            assert!(fcntl_getfd(&adopted).unwrap().contains(FdFlags::CLOEXEC));

            let mut end = UnixStream::from(adopted);
            host.write_all(b"to the server").unwrap();
            let mut got = [0u8; 13];
            end.read_exact(&mut got).unwrap();
            assert_eq!(&got, b"to the server");
            end.write_all(b"to the host").unwrap();
            let mut back = [0u8; 11];
            host.read_exact(&mut back).unwrap();
            assert_eq!(&back, b"to the host");

            // Descriptor 0 no longer holds the socket: closing the adopted end
            // leaves the host's read at end of file, and a read of standard input
            // is end of file too.
            drop(end);
            assert_eq!(
                host.read(&mut got).unwrap(),
                0,
                "no descriptor holds the end"
            );
            assert_eq!(std::io::stdin().read(&mut got).unwrap(), 0);
        }

        /// **A standard input that is not a stream socket is refused by name**,
        /// and descriptor 0 is as it was.
        #[test]
        fn adopt_refuses_a_stdin_that_is_not_a_stream_socket() {
            let file = std::fs::File::open("/dev/null").unwrap();
            dup2_stdin(&file).unwrap();
            assert!(matches!(adopt_host_socket(), Err(AdoptError::NotASocket)));

            let (datagram, _peer) = UnixDatagram::pair().unwrap();
            dup2_stdin(&datagram).unwrap();
            assert!(matches!(
                adopt_host_socket(),
                Err(AdoptError::NotAStream { .. })
            ));
            // Left as it was: still the datagram socket, not `/dev/null`.
            let still = rustix::net::sockopt::socket_type(rustix::stdio::stdin()).unwrap();
            assert_eq!(still, rustix::net::SocketType::DGRAM);
        }
    }
}
