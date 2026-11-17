//! Waiting on a sequencer client and a stop descriptor.
//!
//! A non-blocking client is read until it is empty, then waited on here. The
//! sequencer's descriptors are level-triggered, so an event that arrives between
//! the last read and the wait wakes it.

use std::io;
use std::ops::Deref;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::raw::{c_short, c_uint};
use std::time::Duration;

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
/// client closes with it, and so does the stop descriptor. It derefs to the
/// [`Client`] for reads and sends.
#[derive(Debug)]
pub struct Poller {
    client: Client,
    /// The client's descriptors, then the stop descriptor last.
    polls: Vec<libc::pollfd>,
    /// Polled through `polls`: owned here, so the number there stays an open
    /// descriptor for as long as the client is polled.
    _stop: OwnedFd,
}

impl Deref for Poller {
    type Target = Client;

    fn deref(&self) -> &Client {
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
        polls.push(libc::pollfd {
            fd: stop.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        Ok(Poller {
            client: self,
            polls,
            _stop: stop,
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
            if let Some(wait) = wait_on(&mut self.polls, -1)? {
                return Ok(wait);
            }
        }
    }

    /// As [`wait`](Self::wait), and `None` when `timeout` passes with nothing
    /// ready.
    ///
    /// # Errors
    ///
    /// The OS error of `poll`, but an interrupted wait, which is retried.
    pub fn wait_for(&mut self, timeout: Duration) -> io::Result<Option<Wait>> {
        wait_on(&mut self.polls, timeout_ms(timeout))
    }
}

/// Polls `polls`, the stop descriptor last, for `timeout_ms` (-1 waits for
/// ever); `None` when the time passes with nothing ready.
fn wait_on(polls: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<Option<Wait>> {
    loop {
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
        if ready == 0 {
            return Ok(None);
        }
        let Some((stop, seq)) = polls.split_last() else {
            return Ok(Some(Wait::Broken));
        };
        if stop.revents != 0 {
            return Ok(Some(Wait::Stopped));
        }
        let broken = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
        if seq.iter().any(|p| p.revents & broken != 0) {
            return Ok(Some(Wait::Broken));
        }
        return Ok(Some(Wait::Ready));
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

    /// A socket stands in for the sequencer's descriptor, so the wait is
    /// tested without the sequencer (CI's kernel has no `snd-seq`): nothing
    /// readable times out, the stand-in readable is `Ready`, and a readable
    /// stop descriptor is `Stopped` even with input pending.
    #[test]
    fn the_wait_reports_input_timeout_and_stop_with_stop_first() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        let (mut seq_tx, seq_rx) = UnixStream::pair().unwrap();
        let (mut stop_tx, stop_rx) = UnixStream::pair().unwrap();
        let mut polls = vec![
            libc::pollfd {
                fd: seq_rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop_rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        assert_eq!(wait_on(&mut polls, 20).unwrap(), None);
        seq_tx.write_all(b"x").unwrap();
        assert_eq!(wait_on(&mut polls, 20).unwrap(), Some(Wait::Ready));
        stop_tx.write_all(b"x").unwrap();
        assert_eq!(wait_on(&mut polls, -1).unwrap(), Some(Wait::Stopped));
    }
}
