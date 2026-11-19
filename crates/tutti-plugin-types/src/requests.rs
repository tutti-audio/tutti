//! How a plugin's calls into the host cross threads.
//!
//! A plugin calls its host from any thread. Each such call is one flag of an
//! [`AtomicRequestFlags`]: [`Request::flag`] names it, the
//! callback raises it, and the main thread's idle pass turns the taken flags
//! back into [`Request`]s ([`taken`]) and feeds them to the core. Timers and
//! file descriptors carry an id, so they are main-thread registrations and
//! have no flag.

use tutti_rt::{AtomicRequestFlags, RequestFlag};

use crate::{Request, RescanScope};

const RESTART: RequestFlag = RequestFlag::of(0);
const IO_CHANGED: RequestFlag = RequestFlag::of(1);
const LATENCY_CHANGED: RequestFlag = RequestFlag::of(2);
const TAIL_CHANGED: RequestFlag = RequestFlag::of(3);
const RELOAD: RequestFlag = RequestFlag::of(4);
const CALLBACK: RequestFlag = RequestFlag::of(5);
const RESCAN_VALUES: RequestFlag = RequestFlag::of(6);
const RESCAN_TEXT: RequestFlag = RequestFlag::of(7);
const RESCAN_INFO: RequestFlag = RequestFlag::of(8);
const RESCAN_ALL: RequestFlag = RequestFlag::of(9);
const MIDI_CC: RequestFlag = RequestFlag::of(10);
const NOTE_EXPRESSION: RequestFlag = RequestFlag::of(11);

/// Every request that has a flag, in the order [`taken`] yields them.
const FLAG_REQUESTS: [(RequestFlag, Request); 12] = [
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
    /// The flag a callback raises for this request, or `None` for a timer or a
    /// file descriptor, which carry an id and are registered on the main
    /// thread.
    #[must_use]
    pub fn flag(self) -> Option<RequestFlag> {
        FLAG_REQUESTS
            .iter()
            .find_map(|&(flag, request)| (request == self).then_some(flag))
    }

    /// Raises this request on `flags`. Wait-free; any thread. `false` when the
    /// request has no flag and nothing was raised.
    pub fn raise(self, flags: &AtomicRequestFlags) -> bool {
        match self.flag() {
            Some(flag) => {
                flags.raise(flag);
                true
            }
            None => false,
        }
    }
}

/// The requests raised on `flags` since the last call, each once. Main thread
/// only: it takes the flags.
pub fn taken(flags: &AtomicRequestFlags) -> impl Iterator<Item = Request> + use<> {
    let taken = flags.take();
    FLAG_REQUESTS
        .into_iter()
        .filter_map(move |(flag, request)| taken.contains(flag).then_some(request))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request with a flag round-trips through the flags, once each, and a
    /// timer or descriptor has none.
    #[test]
    fn every_request_with_a_flag_is_taken_once() {
        let flags = AtomicRequestFlags::new();
        for &(_, request) in &FLAG_REQUESTS {
            assert!(request.raise(&flags));
            assert!(request.raise(&flags), "raising twice is one request");
        }
        assert!(!Request::Timer(crate::TimerId(1)).raise(&flags));
        assert!(!Request::Fd(tutti_plugin_ipc_ffi::FdId::new(3).unwrap()).raise(&flags));
        let got: Vec<_> = taken(&flags).collect();
        let want: Vec<_> = FLAG_REQUESTS.iter().map(|&(_, r)| r).collect();
        assert_eq!(got, want);
        assert_eq!(taken(&flags).count(), 0);
    }
}
