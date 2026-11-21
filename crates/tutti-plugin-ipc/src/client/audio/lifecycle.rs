//! What the audio-bridge handle and the bridge thread share: whether the
//! bridge runs, the server's [`ServerWatch`], the refused rate and the editor's
//! news.

use crate::ServerWatch;
use dpi::Size;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tutti_plugin_types::{PluginFailure, RateRefusal, Timeouts};
use tutti_rt::LockExt;

#[derive(Clone)]
pub(super) struct Lifecycle {
    running: Arc<AtomicBool>,
    /// Whether the server is alive, and why not: the one latch every signal of
    /// its death sets. The bridge thread declares the stream's end and
    /// failures here, the process guard the process's end, the host its own
    /// verdict; the first cause stands, so a stream's death cascading into
    /// follow-on errors keeps the first as the diagnosis.
    ///
    /// The latch (not a notification alone) is authoritative because two of the
    /// three crash sites run before a listener is installed
    /// (`PluginBridge::new` spawns the thread, `set_listener` is called
    /// afterwards), so a callback-only design would drop exactly the failures a
    /// host most needs: a plugin that never connected.
    watch: Arc<ServerWatch>,
    /// Every deadline of the bridge's waits.
    timeouts: Timeouts,
    /// The sample rate the plugin refused and why, while it is refused. Not a
    /// crash: the plugin stays loaded and is retried at every later rate
    /// change, which clears this when it takes one.
    refusal: Arc<Mutex<Option<RateRefusal>>>,
    /// Whether `refusal` holds one: the audio thread's question, answered
    /// without the lock or the reason's clone. Stored after the
    /// slot, so a reader that sees it set finds the reason.
    refused: Arc<AtomicBool>,
    /// The size the server's editor asked its window to take
    /// (`EditorWantsSize`), until the host takes it.
    editor_wants: Arc<Mutex<Option<Size>>>,
    /// The server said it closed the editor on its own account
    /// (`EditorClosed`), until the remote editor takes the news.
    editor_closed: Arc<AtomicBool>,
    /// Whether the host's last queued render-mode command said offline.
    offline: Arc<AtomicBool>,
    /// Whether a lane's thread has told the listener of the crash: one event
    /// for the bridge, whichever of its two threads sees the end first.
    announced: Arc<AtomicBool>,
}

impl Lifecycle {
    pub(super) fn new(watch: Arc<ServerWatch>, timeouts: Timeouts) -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            watch,
            timeouts,
            refusal: Arc::new(Mutex::new(None)),
            refused: Arc::new(AtomicBool::new(false)),
            editor_wants: Arc::new(Mutex::new(None)),
            editor_closed: Arc::new(AtomicBool::new(false)),
            offline: Arc::new(AtomicBool::new(false)),
            announced: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Records the render mode the host has just queued for the plugin.
    pub(super) fn set_render_mode(&self, mode: tutti_plugin_types::RenderMode) {
        self.offline.store(mode.is_offline(), Ordering::Release);
    }

    /// The render mode the host last queued: realtime until it says otherwise.
    pub(super) fn render_mode(&self) -> tutti_plugin_types::RenderMode {
        if self.offline.load(Ordering::Acquire) {
            tutti_plugin_types::RenderMode::Offline
        } else {
            tutti_plugin_types::RenderMode::Realtime
        }
    }

    /// The server's editor asked for `size`; the latest ask stands.
    pub(super) fn set_editor_wants(&self, size: Size) {
        *self.editor_wants.lock_recover() = Some(size);
    }

    /// The size the server's editor asked for since the last call.
    pub(super) fn take_editor_wants(&self) -> Option<Size> {
        self.editor_wants.lock_recover().take()
    }

    /// The server closed its editor itself.
    pub(super) fn set_editor_closed(&self) {
        self.editor_closed.store(true, Ordering::Release);
    }

    /// Whether the server closed its editor itself since the last call.
    pub(super) fn take_editor_closed(&self) -> bool {
        self.editor_closed.swap(false, Ordering::AcqRel)
    }

    /// The deadlines the bridge's waits run under.
    pub(super) fn timeouts(&self) -> &Timeouts {
        &self.timeouts
    }

    /// The refused rate (Hz) and the plugin's reason, while one stands.
    pub(super) fn rate_refusal(&self) -> Option<RateRefusal> {
        self.refusal.lock_recover().clone()
    }

    /// Whether a refused rate stands. Wait-free: no lock, no allocation.
    pub(super) fn is_rate_refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }

    pub(super) fn set_rate_refusal(&self, refusal: Option<RateRefusal>) {
        let refused = refusal.is_some();
        *self.refusal.lock_recover() = refusal;
        self.refused.store(refused, Ordering::Release);
    }

    /// Whether the server is gone, by any signal. Wait-free.
    pub(super) fn is_crashed(&self) -> bool {
        self.watch.is_dead()
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// The latched cause, if the server is gone.
    ///
    /// `None` while healthy. A dead server always has one: every
    /// declaration passes a reason.
    pub(super) fn crash_cause(&self) -> Option<PluginFailure> {
        self.watch.cause()
    }

    /// Declare the server gone and latch why; the first cause wins.
    pub(super) fn mark_crashed(&self, cause: PluginFailure) {
        self.watch.declare(cause);
    }

    /// The server's watch, for the guard and the fork to share.
    pub(super) fn watch(&self) -> &Arc<ServerWatch> {
        &self.watch
    }

    /// Whether the caller is the one to tell the listener of the crash: `true`
    /// for the first call only.
    pub(super) fn announce_crash(&self) -> bool {
        !self.announced.swap(true, Ordering::AcqRel)
    }

    pub(super) fn request_shutdown(&self) {
        self.running.store(false, Ordering::Release);
        // Store-load pair with the load in `crash`: ordered before the kill that
        // the guard makes next.
        std::sync::atomic::fence(Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A healthy bridge reports no cause, and a crashed one always does.
    ///
    /// The `Option` is the difference between "still running" and "died for a
    /// reason nobody recorded" — the second must be unreachable, because every
    /// call site passes a reason.
    #[test]
    fn a_cause_appears_exactly_when_the_bridge_crashes() {
        let life = Lifecycle::new(Arc::new(ServerWatch::new()), Timeouts::default());
        assert!(!life.is_crashed());
        assert!(
            life.crash_cause().is_none(),
            "a healthy bridge has no cause"
        );

        life.mark_crashed(PluginFailure::StreamClosed);
        assert!(life.is_crashed());
        assert!(matches!(
            life.crash_cause(),
            Some(PluginFailure::StreamClosed)
        ));
    }

    /// The first cause is kept, not the last.
    ///
    /// A stream death marks the bridge crashed and then drains every queued
    /// request with errors, each of which could mark again. Keeping the last
    /// would replace the fault with its own consequence — the host would be
    /// told "request cancelled" for a plugin that segfaulted.
    #[test]
    fn the_first_cause_survives_later_ones() {
        let life = Lifecycle::new(Arc::new(ServerWatch::new()), Timeouts::default());
        life.mark_crashed(PluginFailure::StreamClosed);
        life.mark_crashed(PluginFailure::Unreported);
        assert!(
            matches!(life.crash_cause(), Some(PluginFailure::StreamClosed)),
            "the follow-on error must not overwrite the fault that caused it"
        );
    }

    /// Shutdown is not a crash.
    ///
    /// The two use different atomics and an ordinary teardown must never
    /// present as a failure — this is what makes it safe to fire a crash
    /// notification from every `mark_crashed` site.
    #[test]
    fn requesting_shutdown_does_not_report_a_crash() {
        let life = Lifecycle::new(Arc::new(ServerWatch::new()), Timeouts::default());
        life.request_shutdown();
        assert!(!life.is_running());
        assert!(!life.is_crashed(), "a clean shutdown is not a crash");
        assert!(life.crash_cause().is_none());
    }

    /// **The audio thread's flag follows the refusal**, set with it and
    /// cleared with it, and answering never allocates.
    #[test]
    fn the_refused_flag_follows_the_refusal_without_allocating() {
        let life = Lifecycle::new(Arc::new(ServerWatch::new()), Timeouts::default());
        assert!(!life.is_rate_refused());
        life.set_rate_refusal(Some(RateRefusal::new(
            tutti_units::SampleRate::SR_44K1,
            tutti_plugin_types::RateRefusalCause::Unconfigured,
        )));
        let refused = assert_no_alloc::assert_no_alloc(|| life.is_rate_refused());
        assert!(refused);
        life.set_rate_refusal(None);
        assert!(!life.is_rate_refused());
    }
}
