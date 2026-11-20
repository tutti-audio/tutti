//! The host's half of one lane's conversation with the server, as a state
//! machine that does no I/O.
//!
//! The host sends a request, and the server answers it, perhaps with several
//! frames, perhaps late, perhaps never. A [`Conversation`] keeps what the host
//! is owed: each request by its key (the id the request carried and the reply
//! echoes), the instant it is due, and whether the host has stopped waiting for
//! it. The thread that drives a lane only reads frames and clocks and feeds
//! them in: a reply is matched to its request by its key, never by its position
//! in the stream, so a reply that comes after its request was given up on is
//! recognised as that request's and not taken for the next one's answer.
//!
//! Every time rule is a function of the instants it is passed ([`open`],
//! [`hear`], [`left`]), so a test drives it with constructed ones.
//!
//! [`open`]: Conversation::open
//! [`hear`]: Conversation::hear
//! [`left`]: Conversation::left

use std::collections::VecDeque;
use std::time::Duration;
use tutti_platform::Instant;
use tutti_plugin_types::Deadline;

/// How many requests a lane may have given up on and not heard the end of: past
/// it the server is not answering, and the lane is its fault to end.
pub(crate) const ABANDONED_LIMIT: usize = 1 << 14;

/// What a [`Conversation`] refuses, as a fault of the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Fault {
    /// A reply named a request this lane never sent, or one whose reply had
    /// already ended: the server is making replies up.
    #[error("the server replied to a request this lane has no record of")]
    Unknown,
    /// The lane has given up on more requests than any server that answers
    /// leaves outstanding.
    #[error("the server left {abandoned} requests unanswered")]
    Flooded {
        /// How many were outstanding.
        abandoned: usize,
    },
}

/// What a frame that carries a request's key is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Heard<T> {
    /// The frame answers a request the lane still waits for. `ends` is whether
    /// it is the last frame of the answer: the request is done then, and not
    /// before.
    Answer {
        /// Whether no frame of this answer follows.
        ends: bool,
    },
    /// The frame belongs to a request the lane gave up on, which carried this
    /// tag. The request is forgotten once its last frame is heard.
    Late(T),
}

/// One request the lane is owed an answer to.
#[derive(Clone, Copy, Debug)]
struct Entry<K, T> {
    key: K,
    tag: T,
    /// How long the server may go without a sign of life for this request.
    window: Duration,
    deadline: Deadline,
    /// The latest `deadline` may be pushed to, however many signs of life come.
    ceiling: Deadline,
    abandoned: bool,
}

/// The requests of one lane that have been sent and not yet heard the end of.
///
/// `K` is the key a request and its reply share; `T` is what the lane's thread
/// needs to know about an abandoned request when its reply comes (a block's
/// reply is forwarded, a state's frames are dropped).
#[derive(Debug)]
pub(crate) struct Conversation<K, T> {
    entries: VecDeque<Entry<K, T>>,
    abandoned: usize,
    limit: usize,
}

impl<K: Copy + Eq, T: Copy> Conversation<K, T> {
    /// A conversation with nothing owed.
    pub(crate) fn new() -> Self {
        Self::with_limit(ABANDONED_LIMIT)
    }

    /// A conversation that faults once `limit` requests have been given up on.
    pub(crate) fn with_limit(limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            abandoned: 0,
            limit,
        }
    }

    /// Notes that request `key`, tagged `tag`, was sent at `now`. The server
    /// has `window` to answer, and `window` more for each sign of life
    /// ([`hear`](Self::hear) of a frame that does not end it), but never until
    /// after `ceiling` from `now`.
    pub(crate) fn open(
        &mut self,
        key: K,
        tag: T,
        now: Instant,
        window: Duration,
        ceiling: Duration,
    ) {
        debug_assert!(
            self.entries.iter().all(|e| e.key != key),
            "a key is opened once"
        );
        self.entries.push_back(Entry {
            key,
            tag,
            window,
            deadline: Deadline::after(now, window),
            ceiling: Deadline::after(now, ceiling),
            abandoned: false,
        });
    }

    /// The server showed it is working on `key` at `now`: the request's window
    /// restarts, up to its ceiling. `false` when `key` is not awaited.
    pub(crate) fn extend(&mut self, key: K, now: Instant) -> bool {
        match self
            .entries
            .iter_mut()
            .find(|e| e.key == key && !e.abandoned)
        {
            Some(entry) => {
                let restarted = Deadline::after(now, entry.window).sooner(entry.ceiling);
                if remaining(restarted, now) > remaining(entry.deadline, now) {
                    entry.deadline = restarted;
                }
                true
            }
            None => false,
        }
    }

    /// Gives `key` a new window from `now`, replacing the one it was opened
    /// with: what the server is doing has changed what a reasonable wait is.
    /// The ceiling stands. `false` when `key` is not awaited.
    pub(crate) fn restart(&mut self, key: K, now: Instant, window: Duration) -> bool {
        match self
            .entries
            .iter_mut()
            .find(|e| e.key == key && !e.abandoned)
        {
            Some(entry) => {
                entry.window = window;
                entry.deadline = Deadline::after(now, window).sooner(entry.ceiling);
                true
            }
            None => false,
        }
    }

    /// What a frame for request `key` is, heard at `now`.
    ///
    /// A frame that does not end an awaited request's answer is a sign of life
    /// ([`extend`](Self::extend)): a state arriving a chunk at a time is
    /// alive while chunks arrive.
    ///
    /// # Errors
    ///
    /// [`Fault::Unknown`] when no request with `key` is awaited or abandoned.
    pub(crate) fn hear(&mut self, key: K, ends: bool, now: Instant) -> Result<Heard<T>, Fault> {
        let at = self
            .entries
            .iter()
            .position(|e| e.key == key)
            .ok_or(Fault::Unknown)?;
        let abandoned = self.entries[at].abandoned;
        if abandoned {
            let tag = self.entries[at].tag;
            if ends {
                self.entries.remove(at);
                self.abandoned -= 1;
            }
            return Ok(Heard::Late(tag));
        }
        if ends {
            self.entries.remove(at);
        } else {
            self.extend(key, now);
        }
        Ok(Heard::Answer { ends })
    }

    /// How long the lane may wait for `key`'s answer at `now`: zero once it is
    /// due, `None` when `key` is not awaited.
    pub(crate) fn left(&self, key: K, now: Instant) -> Option<Duration> {
        self.entries
            .iter()
            .find(|e| e.key == key && !e.abandoned)
            .map(|e| e.deadline.left(now).unwrap_or(Duration::MAX))
    }

    /// Whether `key` is awaited and due at `now`.
    pub(crate) fn is_due(&self, key: K, now: Instant) -> bool {
        self.entries
            .iter()
            .any(|e| e.key == key && !e.abandoned && e.deadline.passed(now))
    }

    /// Stops waiting for `key`: its frames, when they come, are late. A request
    /// that is not awaited is left as it is.
    ///
    /// # Errors
    ///
    /// [`Fault::Flooded`] when this is the request that takes the abandoned
    /// past the limit.
    pub(crate) fn abandon(&mut self, key: K) -> Result<(), Fault> {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.key == key && !e.abandoned)
        {
            entry.abandoned = true;
            self.abandoned += 1;
        }
        if self.abandoned > self.limit {
            return Err(Fault::Flooded {
                abandoned: self.abandoned,
            });
        }
        Ok(())
    }
}

/// The time `deadline` has left at `now`, `Duration::MAX` for one that never
/// passes: an ordering of deadlines.
fn remaining(deadline: Deadline, now: Instant) -> Duration {
    deadline.left(now).unwrap_or(Duration::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(10);
    const CEILING: Duration = Duration::from_secs(60);

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn conversation() -> (Conversation<u32, char>, Instant) {
        (Conversation::new(), Instant::now())
    }

    /// **A reply is matched to its request by its key, not by its place in the
    /// stream**: request 1 gave up, request 2 is awaited, and the answer to 2
    /// is 2's even though 1's late reply is still to come; 1's reply then is
    /// late and carries 1's tag.
    #[test]
    fn a_reply_is_matched_by_its_key_and_not_its_place() {
        let (mut c, t) = conversation();
        c.open(1, 'a', t, WINDOW, CEILING);
        c.abandon(1).unwrap();
        c.open(2, 'b', at(t, 11), WINDOW, CEILING);

        assert_eq!(
            c.hear(2, true, at(t, 12)),
            Ok(Heard::Answer { ends: true }),
            "the awaited request's answer"
        );
        assert_eq!(c.hear(1, true, at(t, 13)), Ok(Heard::Late('a')));
        assert_eq!(c.hear(1, true, at(t, 14)), Err(Fault::Unknown), "it ended");
    }

    /// **A reply to no request is the server's fault**, and so is a second
    /// reply to a request whose reply has ended.
    #[test]
    fn a_reply_to_no_request_is_a_fault() {
        let (mut c, t) = conversation();
        assert_eq!(c.hear(9, true, t), Err(Fault::Unknown));
        c.open(1, 'a', t, WINDOW, CEILING);
        assert_eq!(c.hear(1, true, t), Ok(Heard::Answer { ends: true }));
        assert_eq!(c.hear(1, true, t), Err(Fault::Unknown), "answered once");
    }

    /// **An abandoned request that answers in several frames stays open until
    /// its last**: each frame before it is late and carries the tag, the last
    /// closes the request, and a frame after that is a fault.
    #[test]
    fn the_rest_of_an_abandoned_multi_frame_answer_is_late_until_its_last() {
        let (mut c, t) = conversation();
        c.open(7, 's', t, WINDOW, CEILING);
        c.abandon(7).unwrap();
        for _ in 0..3 {
            assert_eq!(c.hear(7, false, at(t, 20)), Ok(Heard::Late('s')));
        }
        assert_eq!(c.hear(7, true, at(t, 21)), Ok(Heard::Late('s')));
        assert_eq!(c.hear(7, false, at(t, 22)), Err(Fault::Unknown));
    }

    /// **The window is data: a request is due `window` after it was sent, at
    /// that instant and not before.**
    #[test]
    fn a_request_is_due_a_window_after_it_was_sent() {
        let (mut c, t) = conversation();
        c.open(1, 'a', t, WINDOW, CEILING);
        assert_eq!(c.left(1, t), Some(WINDOW));
        assert_eq!(c.left(1, at(t, 4)), Some(Duration::from_secs(6)));
        assert!(!c.is_due(1, at(t, 9)));
        assert!(c.is_due(1, at(t, 10)));
        assert_eq!(c.left(1, at(t, 30)), Some(Duration::ZERO));
        assert_eq!(c.left(2, t), None, "not awaited");
    }

    /// **A frame of an answer that has not ended is a sign of life: the window
    /// restarts from it**, so a state that arrives a chunk at a time is alive
    /// while chunks arrive, and fails a window after the last one.
    #[test]
    fn a_frame_that_does_not_end_the_answer_restarts_the_window() {
        let (mut c, t) = conversation();
        c.open(1, 'a', t, WINDOW, CEILING);
        assert_eq!(
            c.hear(1, false, at(t, 9)),
            Ok(Heard::Answer { ends: false })
        );
        assert_eq!(c.left(1, at(t, 9)), Some(WINDOW), "restarted at the frame");
        assert!(!c.is_due(1, at(t, 18)));
        assert!(c.is_due(1, at(t, 19)), "a window after the last frame");
    }

    /// **A sign of life extends a request's wait, up to its ceiling and no
    /// further**: a load that is still working is waited for, and one that works
    /// for ever is not.
    #[test]
    fn a_sign_of_life_extends_the_wait_up_to_the_ceiling() {
        let (mut c, t) = conversation();
        c.open(1, 'a', t, WINDOW, Duration::from_secs(25));
        assert!(c.extend(1, at(t, 8)));
        assert_eq!(c.left(1, at(t, 8)), Some(WINDOW));
        assert!(c.extend(1, at(t, 16)));
        assert_eq!(
            c.left(1, at(t, 16)),
            Some(Duration::from_secs(9)),
            "the ceiling (25 s from the send) is before 16 + 10"
        );
        assert!(c.extend(1, at(t, 24)));
        assert!(c.is_due(1, at(t, 25)));
        assert!(!c.extend(5, t), "no such request");
    }

    /// **A sign of life never shortens a wait**: a request opened with a
    /// ceiling earlier than its first deadline keeps that deadline when a sign
    /// of life would have capped it sooner.
    #[test]
    fn a_sign_of_life_never_shortens_the_wait() {
        let (mut c, t) = conversation();
        c.open(1, 'a', t, Duration::from_secs(30), Duration::from_secs(10));
        assert!(c.extend(1, at(t, 1)));
        assert_eq!(c.left(1, at(t, 1)), Some(Duration::from_secs(29)));
    }

    /// **A restart replaces the window**, even with a shorter one, and the
    /// ceiling still stands.
    #[test]
    fn a_restart_replaces_the_window_and_the_ceiling_stands() {
        let (mut c, t) = conversation();
        c.open(1, 'a', t, Duration::from_secs(30), Duration::from_secs(40));
        assert!(c.restart(1, at(t, 5), Duration::from_secs(10)));
        assert_eq!(c.left(1, at(t, 5)), Some(Duration::from_secs(10)));
        assert!(c.restart(1, at(t, 35), Duration::from_secs(100)));
        assert_eq!(
            c.left(1, at(t, 35)),
            Some(Duration::from_secs(5)),
            "ceiling 40 s"
        );
        assert!(!c.restart(9, t, WINDOW), "no such request");
    }

    /// **Only a request's own frames move its deadline**: a frame for another
    /// request, and the news the lane hears between (which a conversation is
    /// never told of), leave it where it was, so a chatty server does not hold
    /// a request open.
    #[test]
    fn another_requests_frames_do_not_extend_a_wait() {
        let (mut c, t) = conversation();
        c.open(1, 'a', t, WINDOW, CEILING);
        c.abandon(1).unwrap();
        c.open(2, 'b', at(t, 1), WINDOW, CEILING);
        for secs in 2..9 {
            assert_eq!(c.hear(1, false, at(t, secs)), Ok(Heard::Late('a')));
        }
        assert_eq!(c.left(2, at(t, 9)), Some(Duration::from_secs(2)));
        assert!(c.is_due(2, at(t, 11)));
    }

    /// **An abandoned request that is never answered is bounded**: the lane
    /// faults when the server has left more than the limit unanswered.
    #[test]
    fn too_many_unanswered_requests_is_a_fault() {
        let mut c: Conversation<u32, char> = Conversation::with_limit(3);
        let t = Instant::now();
        for key in 0..3 {
            c.open(key, 'a', t, WINDOW, CEILING);
            assert_eq!(c.abandon(key), Ok(()));
        }
        c.open(3, 'a', t, WINDOW, CEILING);
        assert_eq!(c.abandon(3), Err(Fault::Flooded { abandoned: 4 }));
        assert_eq!(c.hear(0, true, t), Ok(Heard::Late('a')));
        assert_eq!(c.hear(1, true, t), Ok(Heard::Late('a')));
        c.open(4, 'a', t, WINDOW, CEILING);
        assert_eq!(c.abandon(4), Ok(()), "late replies give their room back");
    }
}
