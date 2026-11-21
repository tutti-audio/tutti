//! What a plugin's calls into the host do: [`HostCallbacks`] for
//! [`HostState`].
//!
//! The FFI crate owns the `clap_host` vtable, decodes each call and runs it
//! under the panic guard; every method here is the smallest possible thing:
//! set a latch, push onto a queue, or read a published value. The host's own
//! reaction happens later, on the main thread, when a `take_*` drains what
//! was set. No method calls back into the plugin, which is what keeps a
//! plugin that notifies the host from inside its own `process` from
//! re-entering itself.

use super::state::{HostState, PosixFdEntry, TRANSPORT_QUEUE, TimerEntry};
use std::sync::atomic::Ordering;
use tutti_clap_ffi::{AudioPortsRescanFlags, HostCallbacks, LogSeverity, ParamsRescanFlags};
use tutti_plugin_ipc_ffi::FdId;
use tutti_plugin_types::{Request, RescanScope, TrackInfo, TransportRequest};

impl HostCallbacks for HostState {
    fn is_main_thread(&self) -> bool {
        HostState::is_main_thread(self)
    }

    fn is_audio_thread(&self) -> bool {
        HostState::is_audio_thread(self)
    }

    /// Emits one line as a `tracing` event on target `tutti::plugin`, with the
    /// plugin's id as the `plugin` field. (An audio-thread line never gets
    /// here: the FFI crate counts it through
    /// [`log_dropped_on_audio_thread`](Self::log_dropped_on_audio_thread)
    /// before decoding it, because building the `String` is itself one of the
    /// allocations that guard exists to prevent.)
    ///
    /// Debug maps to `DEBUG`, info to `INFO`, warning to `WARN`; error, fatal
    /// and both misbehaving severities to `ERROR`. A severity CLAP does not
    /// name is `WARN`, with the raw value in `severity`.
    fn log(&self, severity: LogSeverity, message: &str) {
        let plugin = self.plugin_id.get().map_or("", String::as_str);
        match severity {
            LogSeverity::Debug => {
                tracing::debug!(target: "tutti::plugin", plugin, "{message}");
            }
            LogSeverity::Info => {
                tracing::info!(target: "tutti::plugin", plugin, "{message}");
            }
            LogSeverity::Warning => {
                tracing::warn!(target: "tutti::plugin", plugin, "{message}");
            }
            LogSeverity::Error
            | LogSeverity::Fatal
            | LogSeverity::HostMisbehaving
            | LogSeverity::PluginMisbehaving => {
                tracing::error!(target: "tutti::plugin", plugin, "{message}");
            }
            LogSeverity::Unknown(raw) => {
                tracing::warn!(target: "tutti::plugin", plugin, severity = raw, "{message}");
            }
        }
    }

    /// `clap.log` is `[thread-safe]`, audio thread included, but emitting a
    /// line allocates, so an audio-thread line is counted, not emitted
    /// (visible through [`log_lines_dropped`](crate::ClapLoaded::log_lines_dropped)).
    /// That loses the message text, a real cost.
    fn log_dropped_on_audio_thread(&self) {
        self.log_dropped.fetch_add(1, Ordering::Relaxed);
    }

    fn refused_off_thread_call(&self) {
        self.refused_off_thread.add(1);
    }

    fn request_restart(&self) {
        Request::Restart.raise(&self.requests);
    }

    fn request_process(&self) {
        if let Some(wake) = self.process_wake.get() {
            wake.raise();
        }
    }

    fn request_callback(&self) {
        Request::Callback.raise(&self.requests);
    }

    fn params_rescan(&self, flags: ParamsRescanFlags) {
        // One bit per scope, so the main thread can tell a full rescan, which
        // the spec says is honoured only while the plugin is deactivated, from
        // a value-only one applicable live. Bits accumulate between takes.
        for (flag, scope) in [
            (ParamsRescanFlags::VALUES, RescanScope::Values),
            (ParamsRescanFlags::TEXT, RescanScope::Text),
            (ParamsRescanFlags::INFO, RescanScope::Info),
            (ParamsRescanFlags::ALL, RescanScope::All),
        ] {
            if flags.contains(flag) {
                Request::RescanParams(scope).raise(&self.requests);
            }
        }
    }

    fn params_request_flush(&self) {
        self.flush_requested.store(true, Ordering::Release);
    }

    fn state_mark_dirty(&self) {
        self.state_dirty.store(true, Ordering::Release);
    }

    fn latency_changed(&self) {
        Request::LatencyChanged.raise(&self.requests);
    }

    fn tail_changed(&self) {
        Request::TailChanged.raise(&self.requests);
    }

    fn gui_request_resize(&self, width: u32, height: u32) -> bool {
        self.gui.resize_request.store([width, height]);
        true
    }

    fn gui_closed(&self, was_destroyed: bool) {
        self.gui.closed.store(true, Ordering::Release);
        // `was_destroyed` reports the plugin's *window* is gone. Record it so
        // `close_editor` skips `gui.hide` (there is no window left to hide)
        // while still calling `gui.destroy`, which `ext/gui.h` requires the
        // host call "to acknowledge the gui destruction" and which releases
        // what `create` allocated.
        if was_destroyed {
            self.gui.window_destroyed.store(true, Ordering::Release);
        }
    }

    fn audio_ports_rescan(&self, flags: AudioPortsRescanFlags) {
        // Five of the six flags are `[!active]`: only those change what a
        // buffer looks like, so only they ask for a restart. A cosmetic name
        // change asks for nothing.
        if !flags.difference(AudioPortsRescanFlags::NAMES).is_empty() {
            Request::IoChanged.raise(&self.requests);
        }
    }

    /// The note dialects are read at activation, so a note-port change is
    /// served as an I/O change: the lifecycle re-activates the plugin. The
    /// FFI crate hands over no rescan flags for the note ports, so a rescan
    /// of the names alone restarts as well.
    fn note_ports_rescan(&self) {
        Request::IoChanged.raise(&self.requests);
    }

    fn timer_register(&self, period_ms: u32) -> Option<u32> {
        let id = self.timer.next_id.fetch_add(1, Ordering::Relaxed);
        let mut timers = self.timer.timers.lock().ok()?;
        // `register_timer` is `[main-thread]`, where reading the clock is
        // allowed; the entry's period runs from this instant.
        let entry = TimerEntry {
            id,
            period_ms,
            last_fire: self.timer.now(),
        };
        self.timer.note_due(entry.wake_at());
        timers.push(entry);
        Some(id)
    }

    fn timer_unregister(&self, timer_id: u32) -> bool {
        let Ok(mut timers) = self.timer.timers.lock() else {
            return false;
        };
        let len_before = timers.len();
        timers.retain(|t| t.id != timer_id);
        timers.len() < len_before
    }

    fn track_info(&self) -> Option<TrackInfo> {
        self.track.info.lock().ok()?.clone()
    }

    fn transport_request(&self, request: TransportRequest) {
        // A refused request is the plugin's flood, so it is logged as the
        // plugin misbehaving.
        if !self.transport.push(request) {
            self.log(
                LogSeverity::PluginMisbehaving,
                &format!("transport request {request:?} refused: {TRANSPORT_QUEUE} already wait"),
            );
        }
    }

    fn posix_fd_register(&self, fd: i32, flags: u32) -> bool {
        // A negative number names no descriptor; the plugin's registration is
        // refused, as CLAP's "invalid fd".
        if FdId::new(fd).is_none() {
            return false;
        }
        let Ok(mut fds) = self.posix_fds.entries.lock() else {
            return false;
        };
        if fds.iter().any(|e| e.fd == fd) {
            return false;
        }
        fds.push(PosixFdEntry { fd, flags });
        true
    }

    fn posix_fd_modify(&self, fd: i32, flags: u32) -> bool {
        let Ok(mut fds) = self.posix_fds.entries.lock() else {
            return false;
        };
        match fds.iter_mut().find(|e| e.fd == fd) {
            Some(entry) => {
                entry.flags = flags;
                true
            }
            None => false,
        }
    }

    fn posix_fd_unregister(&self, fd: i32) -> bool {
        let Ok(mut fds) = self.posix_fds.entries.lock() else {
            return false;
        };
        let len_before = fds.len();
        fds.retain(|e| e.fd != fd);
        fds.len() < len_before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused off-thread call counts on the instance's own host state, and
    /// on no other.
    #[test]
    fn a_refused_off_thread_call_counts_on_its_own_instance() {
        let (one, other) = (
            HostState::new(tutti_fixtures::main_thread()),
            HostState::new(tutti_fixtures::main_thread()),
        );
        one.refused_off_thread_call();
        one.refused_off_thread_call();
        assert_eq!(one.refused_off_thread.get(), 2);
        assert_eq!(other.refused_off_thread.get(), 0);
    }

    /// An audio-ports rescan asks for a restart only when a flag that changes a
    /// buffer's shape is set; a cosmetic name change asks for nothing.
    #[test]
    fn a_ports_rescan_asks_for_a_restart_only_for_a_change_of_shape() {
        let state = HostState::new(tutti_fixtures::main_thread());

        state.audio_ports_rescan(AudioPortsRescanFlags::NAMES);
        assert_eq!(tutti_plugin_types::taken(&state.requests).count(), 0);

        state.audio_ports_rescan(AudioPortsRescanFlags::CHANNEL_COUNT);
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(got, [Request::IoChanged]);
    }

    /// Each of the five flags `ext/audio-ports.h` annotates `[!active]` asks
    /// for a restart on its own, and the safe `NAMES` beside one does not
    /// excuse it.
    #[test]
    fn every_non_name_ports_flag_asks_for_a_restart() {
        let state = HostState::new(tutti_fixtures::main_thread());
        for flag in [
            AudioPortsRescanFlags::FLAGS,
            AudioPortsRescanFlags::CHANNEL_COUNT,
            AudioPortsRescanFlags::PORT_TYPE,
            AudioPortsRescanFlags::IN_PLACE_PAIR,
            AudioPortsRescanFlags::LIST,
            AudioPortsRescanFlags::NAMES | AudioPortsRescanFlags::CHANNEL_COUNT,
        ] {
            state.audio_ports_rescan(flag);
            let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
            assert_eq!(got, [Request::IoChanged], "{flag:?}");
        }
    }

    /// A value-only parameter rescan is picked up live; `ALL` is its own scope,
    /// which the lifecycle serves with the plugin deactivated.
    #[test]
    fn a_full_parameter_rescan_is_its_own_scope() {
        let state = HostState::new(tutti_fixtures::main_thread());
        state.params_rescan(ParamsRescanFlags::VALUES);
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(got, [Request::RescanParams(RescanScope::Values)]);

        state.params_rescan(ParamsRescanFlags::ALL);
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(got, [Request::RescanParams(RescanScope::All)]);
    }

    /// The note dialects are read at activation, so a note-ports rescan is
    /// an I/O change the lifecycle serves with a restart.
    #[test]
    fn a_note_ports_rescan_asks_for_a_restart() {
        let state = HostState::new(tutti_fixtures::main_thread());
        state.note_ports_rescan();
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(got, [Request::IoChanged]);
    }

    /// A take returns the last resize request once, then none.
    #[test]
    fn a_resize_request_is_taken_once() {
        let state = HostState::new(tutti_fixtures::main_thread());
        assert_eq!(state.gui.take_resize(), None);
        assert!(state.gui_request_resize(640, 480));
        assert!(state.gui_request_resize(800, 600));
        assert_eq!(state.gui.take_resize(), Some([800, 600]));
        assert_eq!(state.gui.take_resize(), None);
    }

    /// A request stored after a take is the next take's: the take leaves
    /// the word empty for it, never clears it afterwards.
    #[test]
    fn a_resize_request_after_a_take_is_seen_by_the_next_take() {
        let state = HostState::new(tutti_fixtures::main_thread());
        assert!(state.gui_request_resize(640, 480));
        assert_eq!(state.gui.take_resize(), Some([640, 480]));
        assert!(state.gui_request_resize(1024, 768));
        assert_eq!(state.gui.take_resize(), Some([1024, 768]));
    }

    /// Two rescans between takes keep both requests, each once.
    #[test]
    fn rescans_between_takes_accumulate_rather_than_overwrite() {
        let state = HostState::new(tutti_fixtures::main_thread());

        state.params_rescan(ParamsRescanFlags::VALUES);
        state.params_rescan(ParamsRescanFlags::TEXT | ParamsRescanFlags::VALUES);
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(
            got,
            [
                Request::RescanParams(RescanScope::Values),
                Request::RescanParams(RescanScope::Text),
            ]
        );
    }

    /// A restart, a callback, a latency and a tail change each reach the word.
    #[test]
    fn the_plain_callbacks_raise_their_requests() {
        let state = HostState::new(tutti_fixtures::main_thread());
        state.request_restart();
        state.request_callback();
        state.latency_changed();
        state.tail_changed();
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(
            got,
            [
                Request::Restart,
                Request::LatencyChanged,
                Request::TailChanged,
                Request::Callback,
            ]
        );
    }
}
