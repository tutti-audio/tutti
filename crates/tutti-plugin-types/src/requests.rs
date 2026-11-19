//! How a plugin's calls into the host cross threads.
//!
//! A plugin calls its host from any thread. Each such call is one bit of a
//! [`RequestWord`]: [`Request::bit`] names it, the
//! callback raises it, and the main thread's idle pass turns the taken bits
//! back into [`Request`]s ([`taken`]) and feeds them to the core. Timers and
//! file descriptors carry an id, so they are main-thread registrations and
//! have no bit.

use tutti_rt::{RequestBit, RequestWord};

use crate::{Request, RescanScope};

const RESTART: RequestBit = RequestBit::of(0);
const IO_CHANGED: RequestBit = RequestBit::of(1);
const LATENCY_CHANGED: RequestBit = RequestBit::of(2);
const TAIL_CHANGED: RequestBit = RequestBit::of(3);
const RELOAD: RequestBit = RequestBit::of(4);
const CALLBACK: RequestBit = RequestBit::of(5);
const RESCAN_VALUES: RequestBit = RequestBit::of(6);
const RESCAN_TEXT: RequestBit = RequestBit::of(7);
const RESCAN_INFO: RequestBit = RequestBit::of(8);
const RESCAN_ALL: RequestBit = RequestBit::of(9);
const MIDI_CC: RequestBit = RequestBit::of(10);
const NOTE_EXPRESSION: RequestBit = RequestBit::of(11);

/// Every request that has a bit, in the order [`taken`] yields them.
const WORD_REQUESTS: [(RequestBit, Request); 12] = [
    (RESTART, Request::Restart),
    (IO_CHANGED, Request::IoChanged),
    (LATENCY_CHANGED, Request::LatencyChanged),
    (TAIL_CHANGED, Request::TailChanged),
    (RELOAD, Request::Reload),
    (CALLBACK, Request::Callback),
    (RESCAN_VALUES, Request::RescanParams(RescanScope::Values)),
    (RESCAN_TEXT, Request::RescanParams(RescanScope::Text)),
    (RESCAN_INFO, Request::RescanParams(RescanScope::Info)),
    (RESCAN_ALL, Request::RescanParams(RescanScope::All)),
    (MIDI_CC, Request::MidiCcAssignmentChanged),
    (NOTE_EXPRESSION, Request::NoteExpressionChanged),
];

impl Request {
    /// The bit a callback raises for this request, or `None` for a timer or a
    /// file descriptor, which carry an id and are registered on the main
    /// thread.
    #[must_use]
    pub fn bit(self) -> Option<RequestBit> {
        WORD_REQUESTS
            .iter()
            .find_map(|&(bit, request)| (request == self).then_some(bit))
    }

    /// Raises this request on `word`. Wait-free; any thread. `false` when the
    /// request has no bit and nothing was raised.
    pub fn raise(self, word: &RequestWord) -> bool {
        match self.bit() {
            Some(bit) => {
                word.raise(bit);
                true
            }
            None => false,
        }
    }
}

/// The requests raised on `word` since the last call, each once. Main thread
/// only: it takes the word.
pub fn taken(word: &RequestWord) -> impl Iterator<Item = Request> + use<> {
    let bits = word.take();
    WORD_REQUESTS
        .into_iter()
        .filter_map(move |(bit, request)| bits.contains(bit).then_some(request))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request with a bit round-trips through the word, once each, and a
    /// timer or descriptor has none.
    #[test]
    fn every_request_with_a_bit_is_taken_once() {
        let word = RequestWord::new();
        for &(_, request) in &WORD_REQUESTS {
            assert!(request.raise(&word));
            assert!(request.raise(&word), "raising twice is one request");
        }
        assert!(!Request::Timer(crate::TimerId(1)).raise(&word));
        assert!(!Request::Fd(tutti_plugin_ipc_ffi::FdId::new(3).unwrap()).raise(&word));
        let got: Vec<_> = taken(&word).collect();
        let want: Vec<_> = WORD_REQUESTS.iter().map(|&(_, r)| r).collect();
        assert_eq!(got, want);
        assert_eq!(taken(&word).count(), 0);
    }
}
