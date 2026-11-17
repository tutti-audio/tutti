//! Waiting on a sequencer client and a stop descriptor.
//!
//! A non-blocking client is read until it is empty, then waited on here. The
//! sequencer's descriptors are level-triggered, so an event that arrives between
//! the last read and the wait wakes it.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::raw::{c_short, c_uint};
use std::time::{Duration, Instant};

use crate::client::Client;
use crate::error::{Operation, Result, check};
use crate::sys;

/// Why a wait returned.
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum Wait {
    /// The sequencer has input.
    Ready,
    /// The stop descriptor is readable or closed.
    Stopped,
    /// A sequencer descriptor reports an error or hang-up.
    Broken,
}

/// A client together with the stop descriptor it is waited on with.
///
/// It owns both, so the descriptors it polls cannot be closed under it: the
/// client closes with it, and so does the stop descriptor. Reads and sends go
/// through [`client`](Self::client).
#[derive(Debug)]
pub struct Poller {
    client: Client,
    /// The client's input descriptors, as numbers the client owns. Room is kept
    /// for one more, which a wait uses for the stop descriptor.
    seq: Vec<libc::pollfd>,
    /// The stop descriptor, owned here: the number a wait polls for it stays an
    /// open descriptor for as long as the client is polled.
    stop: OwnedFd,
}

impl Poller {
    /// The client that is polled, for reads and sends.
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }
}

impl Client {
    /// This client's input descriptors, and `stop`, which the owner of the
    /// other end closes to wake the waiter, as a [`Poller`] that owns both.
    ///
    /// # Errors
    ///
    /// The code of the descriptor query that failed.
    pub fn into_poller(self, stop: OwnedFd) -> Result<Poller> {
        let events = libc::POLLIN;
        // SAFETY: `self` is open.
        let rc = unsafe { sys::snd_seq_poll_descriptors_count(self.raw(), events) };
        let count = check(rc, Operation::PollDescriptorsCount)?;
        let space = count as usize;
        let mut polls = vec![
            libc::pollfd {
                fd: -1,
                events: 0,
                revents: 0
            };
            space
        ];
        // SAFETY: `polls` is `space` `pollfd`s, which is `count` of them, and
        // alsa-lib writes at most that many.
        let rc = unsafe {
            sys::snd_seq_poll_descriptors(
                self.raw(),
                polls.as_mut_ptr().cast(),
                c_uint::try_from(space).unwrap_or(c_uint::MAX),
                events,
            )
        };
        check(rc, Operation::PollDescriptors)?;
        polls.reserve_exact(1);
        Ok(Poller {
            client: self,
            seq: polls,
            stop,
        })
    }
}

impl Poller {
    /// Blocks until the sequencer has input or the stop descriptor fires.
    ///
    /// # Errors
    ///
    /// The OS error of `poll`, but an interrupted wait, which is retried.
    pub fn wait(&mut self) -> io::Result<Wait> {
        loop {
            if let Some(wait) = wait_on(&mut self.seq, self.stop.as_raw_fd(), None)? {
                return Ok(wait);
            }
        }
    }

    /// As [`wait`](Self::wait), and `None` when `timeout` passes with nothing
    /// ready. An interrupted wait is retried with the time left of `timeout`.
    ///
    /// # Errors
    ///
    /// The OS error of `poll`, but an interrupted wait, which is retried.
    pub fn wait_for(&mut self, timeout: Duration) -> io::Result<Option<Wait>> {
        let deadline = Instant::now() + timeout;
        wait_on(&mut self.seq, self.stop.as_raw_fd(), Some(deadline))
    }
}

/// Polls `seq` and `stop` until something is ready or `deadline` passes (`None`
/// waits for ever); `None` when the deadline passes with nothing ready. `seq`
/// has room for one more, which holds `stop` during the call and is taken off
/// again, so no wait allocates.
fn wait_on(
    seq: &mut Vec<libc::pollfd>,
    stop: RawFd,
    deadline: Option<Instant>,
) -> io::Result<Option<Wait>> {
    let count = seq.len();
    seq.push(libc::pollfd {
        fd: stop,
        events: libc::POLLIN,
        revents: 0,
    });
    let ready = poll_until(seq, deadline);
    let stopped = seq[count].revents != 0;
    seq.truncate(count);
    if !ready? {
        return Ok(None);
    }
    let broken = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
    Ok(Some(if stopped {
        Wait::Stopped
    } else if seq.iter().any(|p| p.revents & broken != 0) {
        Wait::Broken
    } else {
        Wait::Ready
    }))
}

/// `poll` over `polls` until one is ready (true) or `deadline` passes (false).
/// An interrupted `poll` goes again with the time left.
fn poll_until(polls: &mut [libc::pollfd], deadline: Option<Instant>) -> io::Result<bool> {
    loop {
        let timeout_ms = match deadline {
            None => -1,
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() { 0 } else { timeout_ms(left) }
            }
        };
        for p in polls.iter_mut() {
            p.revents = 0;
        }
        // SAFETY: `polls` is a live array of `polls.len()` `pollfd`s.
        let ready =
            unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, timeout_ms) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready > 0 {
            return Ok(true);
        }
        // Nothing ready: the wait is over when the deadline has passed.
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(false);
        }
    }
}

/// `poll`'s timeout: a time is rounded up to a millisecond (0 would not wait
/// at all) and saturates at `i32::MAX`.
fn timeout_ms(timeout: Duration) -> i32 {
    i32::try_from(timeout.as_millis().max(1)).unwrap_or(i32::MAX)
}

const _: () = {
    // `c_short` is what `pollfd.events` holds; the event masks fit it.
    let _: c_short = libc::POLLIN;
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_is_whole_milliseconds_and_never_zero() {
        assert_eq!(timeout_ms(Duration::ZERO), 1);
        assert_eq!(timeout_ms(Duration::from_micros(10)), 1);
        assert_eq!(timeout_ms(Duration::from_millis(250)), 250);
        assert_eq!(timeout_ms(Duration::from_secs(u64::MAX / 2000)), i32::MAX);
    }

    /// Two sockets stand in for the sequencer's descriptor and the stop
    /// descriptor, so the wait is tested without the sequencer (CI's kernel has
    /// no `snd-seq`): nothing readable passes the deadline, the stand-in
    /// readable is `Ready`, a hung-up one is `Broken`, and a readable stop
    /// descriptor is `Stopped` even with input pending.
    #[test]
    fn the_wait_reports_input_timeout_hang_up_and_stop_with_stop_first() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        let past = Some(Instant::now());
        let soon = || Some(Instant::now() + Duration::from_millis(20));
        let (mut seq_tx, seq_rx) = UnixStream::pair().unwrap();
        let (mut stop_tx, stop_rx) = UnixStream::pair().unwrap();
        let mut seq = vec![libc::pollfd {
            fd: seq_rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        seq.reserve_exact(1);
        let stop = stop_rx.as_raw_fd();
        assert_eq!(wait_on(&mut seq, stop, past).unwrap(), None);
        assert_eq!(wait_on(&mut seq, stop, soon()).unwrap(), None);
        seq_tx.write_all(b"x").unwrap();
        assert_eq!(wait_on(&mut seq, stop, soon()).unwrap(), Some(Wait::Ready));
        drop(seq_tx);
        assert_eq!(wait_on(&mut seq, stop, soon()).unwrap(), Some(Wait::Broken));
        stop_tx.write_all(b"x").unwrap();
        assert_eq!(wait_on(&mut seq, stop, None).unwrap(), Some(Wait::Stopped));
        assert_eq!(seq.len(), 1, "the stop slot is taken off again");
    }
}
