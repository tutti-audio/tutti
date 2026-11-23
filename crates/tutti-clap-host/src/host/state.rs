//! Shared host↔plugin state: the latches a plugin's callbacks set and the
//! host's poll methods drain, plus the `[audio-thread]` role guard.
//!
//! A plugin calls host callbacks from its own threads at times the host does
//! not choose, so almost everything here is an atomic flag *set* by a callback
//! and *cleared* by the matching `poll_*` on the main thread. The flags are
//! grouped into one struct per CLAP extension so a caller can see which
//! extension a request came from.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tutti_clap_ffi::ThreadRoles;
use tutti_platform::Instant;
use tutti_plugin_types::{
    AtomicRequestFlags, MainThreadMarker, ProcessWake, TrackInfo, TransportRequest,
};
use tutti_rt::AtomicCell;
use tutti_rt::Mutex;

pub(crate) struct TimerEntry {
    pub id: u32,
    pub period_ms: u32,
    /// When the timer last fired, or was registered.
    pub last_fire: Instant,
}

/// The least a timer's deadline is put after its last fire, whatever period it
/// asked for. `timer-support.h` lets the host adjust a period it finds too
/// short (30 Hz must be allowed), and a deadline at "now" for a period of zero
/// would have the main loop wake again at once, for ever. A timer that asks for
/// less still fires at its own period, whenever the loop is woken by anything.
pub(crate) const TIMER_FLOOR: std::time::Duration = std::time::Duration::from_millis(10);

impl TimerEntry {
    /// When the loop should wake for this timer: its period after the last
    /// fire, at least [`TIMER_FLOOR`].
    pub(crate) fn wake_at(&self) -> Instant {
        self.last_fire
            + std::time::Duration::from_millis(u64::from(self.period_ms)).max(TIMER_FLOOR)
    }
}

/// One POSIX file descriptor the plugin asked the host to watch
/// (`CLAP_EXT_POSIX_FD_SUPPORT`).
#[derive(Debug)]
pub struct PosixFdEntry {
    /// The descriptor to poll. Owned by the plugin — the host watches it but
    /// must not close it.
    pub fd: i32,
    /// Raw `clap_posix_fd_flags` bits naming which events to watch for.
    pub flags: u32,
}

/// Editor-window requests the plugin raised through `CLAP_EXT_GUI`.
#[derive(Debug)]
pub struct GuiState {
    /// The plugin reported its editor closed. Says nothing about whether the
    /// window survived — [`window_destroyed`](Self::window_destroyed) carries
    /// that, and the two differ in whether `gui.hide` still has a target.
    pub closed: AtomicBool,
    /// Set when the plugin reported `gui.closed(was_destroyed = true)` — its
    /// **window** is gone. `close_editor` reads this to skip `gui.hide`, which
    /// has no window left to act on, and still calls `gui.destroy`: `ext/gui.h`
    /// requires the host call `destroy()` to acknowledge the destruction, and
    /// `destroy` releases what `create` allocated rather than the window.
    /// Latched until the next editor is opened.
    pub window_destroyed: AtomicBool,
    /// What the plugin asked the editor window to be resized to, in pixels:
    /// width and height in one word, so a take never pairs one request's width
    /// with another's height. `[0, 0]` when none waits.
    pub resize_request: AtomicCell<[u32; 2]>,
}

/// [`GuiState::resize_request`] with no request: a request with no area names
/// no size.
pub(crate) const NO_RESIZE: [u32; 2] = [0, 0];

impl GuiState {
    /// The pending resize request, once: `None` after it, until the plugin
    /// asks again. One atomic swap, so a request raised after the take waits
    /// for the next one.
    pub(crate) fn take_resize(&self) -> Option<[u32; 2]> {
        let asked = self.resize_request.swap(NO_RESIZE);
        (asked != NO_RESIZE).then_some(asked)
    }

    fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            window_destroyed: AtomicBool::new(false),
            resize_request: AtomicCell::new(NO_RESIZE),
        }
    }
}

/// Periodic timers the plugin registered through `CLAP_EXT_TIMER_SUPPORT`.
///
/// The host is responsible for firing them; nothing here drives a clock.
pub struct TimerState {
    pub(crate) timers: Mutex<Vec<TimerEntry>>,
    pub(crate) next_id: AtomicU32,
    /// What `next_due` counts from.
    epoch: Instant,
    /// The earliest moment a timer can be due, in nanoseconds after `epoch`,
    /// or [`Self::NONE`] with no timer. Never later than the true earliest: a
    /// registration lowers it, an unregistration leaves it (a stale early
    /// value costs one pass, which recomputes it), and a pass over the timers
    /// sets it exactly. It lets the per-block pass skip the timers lock.
    next_due: AtomicU64,
}

impl std::fmt::Debug for TimerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimerState")
            .field("next_id", &self.next_id)
            .field("epoch", &self.epoch)
            .field("next_due", &self.next_due)
            .finish_non_exhaustive()
    }
}

impl TimerState {
    /// [`Self::next_due_nanos`] when no timer is registered.
    pub(crate) const NONE: u64 = u64::MAX;

    pub(crate) fn new() -> Self {
        Self {
            timers: Mutex::new(Vec::new()),
            next_id: AtomicU32::new(1),
            epoch: Instant::now(),
            next_due: AtomicU64::new(Self::NONE),
        }
    }

    /// The instant a registration is stamped with: the one place a timer
    /// reads a clock. Every rule that compares instants takes them as
    /// parameters.
    pub(crate) fn now(&self) -> Instant {
        Instant::now()
    }

    pub(crate) fn epoch(&self) -> Instant {
        self.epoch
    }

    fn nanos(&self, at: Instant) -> u64 {
        // Saturating, and one short of `NONE`, which a real deadline never is.
        at.saturating_duration_since(self.epoch)
            .as_nanos()
            .min(u128::from(Self::NONE - 1)) as u64
    }

    /// Lowers the cached deadline to `due`, for a timer just registered.
    pub(crate) fn note_due(&self, due: Instant) {
        self.next_due.fetch_min(self.nanos(due), Ordering::AcqRel);
    }

    /// Sets the cached deadline exactly: `None` when no timer is left.
    pub(crate) fn set_next_due(&self, due: Option<Instant>) {
        let nanos = due.map_or(Self::NONE, |d| self.nanos(d));
        self.next_due.store(nanos, Ordering::Release);
    }

    /// The cached deadline in nanoseconds after [`Self::epoch`]; see `next_due`.
    pub(crate) fn next_due_nanos(&self) -> u64 {
        self.next_due.load(Ordering::Acquire)
    }
}

/// File descriptors the plugin asked the host to watch
/// (`CLAP_EXT_POSIX_FD_SUPPORT`).
#[derive(Debug)]
pub struct PosixFdState {
    /// Descriptors the plugin asked the host to watch, in registration order.
    pub entries: Mutex<Vec<PosixFdEntry>>,
}

impl PosixFdState {
    fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }
}

/// How many transport requests wait between two takes. A plugin asks from the
/// main thread, one request per user action, so a take every pass leaves the
/// queue near empty; past this the newest are refused, which keeps the
/// queue's memory fixed against a plugin that asks in a loop.
pub(crate) const TRANSPORT_QUEUE: usize = 64;

/// What the plugin asked the transport to do (`CLAP_EXT_TRANSPORT_CONTROL`),
/// in the order it asked. The extension is `[main-thread]`, and the FFI crate
/// refuses a call from anywhere else, so the queue has one writer and one
/// reader, both the main thread: the lock is never contended and the audio
/// thread never reaches it.
#[derive(Debug)]
pub struct TransportState {
    queue: Mutex<Vec<TransportRequest>>,
}

impl TransportState {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Vec::with_capacity(TRANSPORT_QUEUE)),
        }
    }

    /// Queues `request`; `false` when it was refused, because
    /// [`TRANSPORT_QUEUE`] are waiting.
    #[must_use]
    pub(crate) fn push(&self, request: TransportRequest) -> bool {
        let mut queue = self.queue.lock();
        if queue.len() < TRANSPORT_QUEUE {
            queue.push(request);
            true
        } else {
            false
        }
    }

    /// Moves what is queued to `out`, oldest first.
    pub(crate) fn drain_into(&self, out: &mut Vec<TransportRequest>) {
        out.append(&mut self.queue.lock());
    }
}

/// What the host says of the plugin's track (`CLAP_EXT_TRACK_INFO`), read by
/// the plugin's `get`, which is `[main-thread]`.
#[derive(Debug)]
pub struct TrackState {
    pub(crate) info: Mutex<Option<TrackInfo>>,
}

impl TrackState {
    fn new() -> Self {
        Self {
            info: Mutex::new(None),
        }
    }
}

/// Shared state for host↔plugin communication via atomic flags.
#[derive(Debug)]
pub struct HostState {
    /// Which thread plays which role, and the audio claim: shared with the
    /// FFI crate's plugin handle, which takes the claim around every
    /// audio-side entry.
    pub(crate) roles: Arc<ThreadRoles>,
    /// What the plugin asked of the host, by any callback on any thread: a
    /// restart, a main-thread callback, a latency or tail change, an
    /// audio-ports or parameter rescan. The main thread takes it once per
    /// pass ([`Control::take_requests`](tutti_plugin_types::Control::take_requests)).
    pub requests: AtomicRequestFlags,
    /// What `request_process` raises: the host's wake for a plugin it parked.
    /// Set once, by [`Control::bind_wake`](tutti_plugin_types::Control::bind_wake);
    /// a request before then has nobody to wake.
    pub process_wake: OnceLock<ProcessWake>,
    /// The plugin's saveable state changed (`state.mark_dirty`), so a state
    /// blob written earlier no longer matches the instance. Latched until
    /// [`Editor::take_dirty`](tutti_plugin_types::Editor::take_dirty).
    pub state_dirty: AtomicBool,
    /// The plugin asked for a parameter flush (`params.request_flush`).
    /// Latched until the main thread's pass flushes.
    pub flush_requested: AtomicBool,
    /// Editor close and resize requests.
    pub gui: GuiState,
    /// The `clap.log` lines refused on the audio thread: counted, not
    /// emitted, since emitting allocates.
    pub log_dropped: AtomicU32,
    /// The `[main-thread]` callbacks the plugin made from another thread,
    /// refused. Added to from any thread.
    pub refused_off_thread: tutti_rt::TallyWriter<u64>,
    /// The id `clap.log` lines are tagged with. Set once, at load, from the
    /// descriptor; a line before then carries an empty id.
    pub plugin_id: OnceLock<String>,
    /// Timers the plugin registered for the host to fire.
    pub timer: TimerState,
    /// Descriptors the plugin registered for the host to watch.
    pub posix_fds: PosixFdState,
    /// Transport requests the plugin made, waiting for the host's pass.
    pub transport: TransportState,
    /// What the host says of the plugin's track.
    pub track: TrackState,
}

impl HostState {
    /// Builds an unclaimed `HostState`. The thread `main` proves is taken as
    /// the `[main-thread]` role for the instance's whole life; it is recorded
    /// once, in the instance's [`ThreadRoles`], which every answer reads.
    #[must_use]
    pub fn new(main: MainThreadMarker) -> Self {
        Self {
            roles: Arc::new(ThreadRoles::new(main)),
            requests: AtomicRequestFlags::new(),
            process_wake: OnceLock::new(),
            state_dirty: AtomicBool::new(false),
            flush_requested: AtomicBool::new(false),
            gui: GuiState::new(),
            log_dropped: AtomicU32::new(0),
            refused_off_thread: tutti_rt::tally().writer(),
            plugin_id: OnceLock::new(),
            timer: TimerState::new(),
            posix_fds: PosixFdState::new(),
            transport: TransportState::new(),
            track: TrackState::new(),
        }
    }

    /// Reads a latch and clears it in one atomic step, returning whether it was
    /// set. Draining is the point: two consecutive takes of one unrepeated
    /// request answer `true` then `false`.
    pub fn take(&self, flag: &AtomicBool) -> bool {
        flag.swap(false, Ordering::AcqRel)
    }

    /// Whether the calling thread is currently acting as the audio thread.
    pub fn is_audio_thread(&self) -> bool {
        self.roles.is_audio_thread()
    }

    /// Whether the calling thread is currently acting as the main thread.
    ///
    /// **Exclusive with [`is_audio_thread`](Self::is_audio_thread)**: the
    /// spec lets a host mark the OS main thread as the audio thread, but the
    /// two symbolic roles are alternatives, not simultaneous identities. While
    /// an audio claim is held on this thread, this answers `false`,
    /// so a plugin asserting `!is_main_thread()` inside an `[audio-thread]`
    /// call is not silently defeated by a host that claims to be both.
    ///
    /// Note the exclusion is per-thread, not global: `is_audio_thread` compares
    /// against *this* thread, so a claim held by the real audio thread does not
    /// demote a concurrent main-thread caller. Only the main thread's own claim
    /// — the OS main thread running an `[audio-thread]` call — makes it answer
    /// `false` here.
    ///
    /// One consequence is worth knowing when reading plugin bug reports: an
    /// active `flush_params` claims the audio-thread role on whatever thread
    /// calls it, so if the *main* thread drives it, `[main-thread]` callbacks a
    /// plugin makes from inside `flush` (`request_callback`, `request_restart`,
    /// `params.rescan`) see `is_main_thread() == false`. That is the spec's
    /// own framing — `flush` on an active instance *is* `[audio-thread]` — but
    /// a plugin asserting `is_main_thread()` there will trip. Routing param
    /// changes through the next `process` block avoids it entirely, which is
    /// what `flush_params`' own docs already recommend.
    pub fn is_main_thread(&self) -> bool {
        self.roles.is_main_thread()
    }
}
