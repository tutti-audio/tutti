//! An [`Answer`]: a value a plugin's hosting fills later, awaited by the caller.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_channel::oneshot;

/// A value a plugin's hosting fills later, awaited by the caller.
///
/// The output is `Some(value)` when the thread that learns the answer sent one,
/// and `None` when its [`AnswerSender`] was dropped unanswered (the hosting
/// ended): an answer cancelled. A poll never blocks, and the sender's thread
/// wakes the task.
///
/// **What feeds it.** The plugin's hosting holds the [`AnswerSender`]: for a
/// served plugin, the bridge's control lane thread sends the server's reply
/// (bounded by the lane's query deadline); for an in-process plugin, the
/// main thread sends when it runs the queued work.
///
/// Made by [`Answer::channel`] (the hosting) or [`Answer::ready`] (a value already
/// known). Dropping it discards the answer: a later send is told so.
#[derive(Debug)]
#[must_use = "an answer does nothing unless it is awaited"]
pub struct Answer<T>(oneshot::Receiver<T>);

/// The half of an [`Answer`] the thread that learns the value holds.
#[derive(Debug)]
pub struct AnswerSender<T>(oneshot::Sender<T>);

impl<T> Answer<T> {
    /// An answer that is already filled with `value`, as
    /// [`std::future::ready`] is a future already resolved.
    pub fn ready(value: T) -> Self {
        let (tx, answer) = Self::channel();
        // The answer is alive: the send cannot fail.
        drop(tx.send(value));
        answer
    }

    /// A sender and the answer it fills. Dropping the sender unanswered cancels
    /// the answer.
    pub fn channel() -> (AnswerSender<T>, Self) {
        let (tx, rx) = oneshot::channel();
        (AnswerSender(tx), Self(rx))
    }
}

impl<T> Future for Answer<T> {
    type Output = Option<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        Pin::new(&mut self.0).poll(cx).map(Result::ok)
    }
}

impl<T> AnswerSender<T> {
    /// Whether the answer was dropped: nobody awaits it.
    #[must_use]
    pub fn is_canceled(&self) -> bool {
        self.0.is_canceled()
    }

    /// Fills the answer.
    ///
    /// # Errors
    ///
    /// Gives `value` back when the answer was dropped: nobody awaits it.
    pub fn send(self, value: T) -> Result<(), T> {
        self.0.send(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A filled answer reads its value, and a sender dropped unanswered
    /// cancels it, which reads `None`.**
    #[test]
    fn an_answer_whose_sender_was_dropped_reads_as_cancelled() {
        let (tx, answer) = Answer::channel();
        assert_eq!(tx.send(7), Ok(()));
        assert_eq!(futures_lite::future::block_on(answer), Some(7));

        let (tx, answer) = Answer::<u32>::channel();
        drop(tx);
        assert_eq!(futures_lite::future::block_on(answer), None);
    }

    /// **An answer nobody sent yet is pending, not cancelled.**
    #[test]
    fn an_unfilled_answer_is_pending() {
        let (tx, mut answer) = Answer::<u32>::channel();
        assert_eq!(
            futures_lite::future::block_on(futures_lite::future::poll_once(&mut answer)),
            None
        );
        tx.send(1).unwrap();
        assert_eq!(futures_lite::future::block_on(answer), Some(1));
    }

    /// **A send to a dropped answer hands the value back.**
    #[test]
    fn a_send_to_a_dropped_answer_gives_the_value_back() {
        let (tx, answer) = Answer::channel();
        drop(answer);
        assert_eq!(tx.send(3), Err(3));
    }
}
