//! A hosted plugin's own bypass is the node's mode: what the session writes, how
//! it finds the plugin in the other state, and how often it writes again.

use tutti_plugin_host::{PluginHandle, PluginParamValues};
use tutti_plugin_types::{BypassWrite, OwnBypass, bypass_value, is_bypassed};

/// One hosted plugin's reconciliation of its own bypass, on the control thread.
///
/// A write is [`Sent`](BypassWrite::Sent), [`Lost`](BypassWrite::Lost) or
/// [`Refused`](BypassWrite::Refused); a
/// `Sent` one is not applied until the plugin is read in the node's mode.
/// Each [`reconcile`](Self::reconcile) compares the plugin with the mode, with no
/// round trip, and writes the mode again where they differ, or where the last
/// write never reached the plugin (no read decides that). A difference is one
/// episode, counted once until the plugin is back in the mode, and its retries
/// wait 1, 2, 4 ... 32 ticks.
#[derive(Debug)]
pub(crate) struct BypassSync {
    /// Episodes: a bypass found other than the mode after a write.
    unapplied: u64,
    /// Whether an episode is open.
    episode: bool,
    /// The last write did not reach the plugin.
    lost: bool,
    /// Ticks to skip before the next comparison or retry.
    wait: u32,
    /// The wait after the next retry.
    backoff: u32,
}

impl Default for BypassSync {
    fn default() -> Self {
        Self {
            unapplied: 0,
            episode: false,
            lost: false,
            wait: 0,
            backoff: 1,
        }
    }
}

impl BypassSync {
    /// The episodes counted over the plugin's life: a write that was lost or
    /// refused, or a bypass the plugin's editor, a restored state or its
    /// automation moved since. A report that arrives late can open one that the
    /// next tick closes.
    pub(crate) fn unapplied(&self) -> u64 {
        self.unapplied
    }

    /// Writes `want` now: the node's mode changed or a plugin entered the node.
    /// The wait and the backoff start over, so a change of mode is never held
    /// back, and a write that is lost is tried again on the next tick.
    pub(crate) fn assert_mode(
        &mut self,
        handle: &PluginHandle,
        mirror: &PluginParamValues,
        want: bool,
    ) {
        self.backoff = 1;
        self.wait = 0;
        self.send(handle, mirror, want);
    }

    /// Compares the plugin's bypass with `want` and writes it again where they
    /// differ. An in-process plugin is read (without waiting for its instance);
    /// a served one's value is its mirrored parameter, where the session records
    /// what it wrote only once the write was delivered, so a plugin's own refusal
    /// is not seen there and a change it reports (its editor, its automation) is.
    pub(crate) fn reconcile(
        &mut self,
        handle: &PluginHandle,
        mirror: &PluginParamValues,
        want: bool,
    ) {
        let Some(site) = handle.loaded().own_bypass else {
            return;
        };
        if self.wait > 0 {
            self.wait -= 1;
            return;
        }
        if !self.lost {
            let held = if handle.server_pid().is_some() {
                mirrored(site, mirror)
            } else {
                handle.bypassed()
            };
            match held {
                None => return,
                Some(held) if held == want => {
                    self.episode = false;
                    self.backoff = 1;
                    return;
                }
                Some(_) => {}
            }
        }
        if !self.episode {
            self.episode = true;
            self.unapplied += 1;
            self.backoff = 1;
        }
        self.send(handle, mirror, want);
        self.wait = self.backoff;
        self.backoff = (self.backoff * 2).min(32);
    }

    fn send(&mut self, handle: &PluginHandle, mirror: &PluginParamValues, want: bool) {
        match handle.set_bypassed(want) {
            BypassWrite::Sent => {
                self.lost = false;
                // An active plugin applies a write at its next block, so the
                // comparison one tick on is not yet an answer.
                self.wait = 1;
                // A served plugin is compared with its mirror, which learns what
                // was delivered as it learns what the plugin reports.
                if let (Some(OwnBypass::Parameter(id)), Some(_)) =
                    (handle.loaded().own_bypass, handle.server_pid())
                {
                    mirror.report(id, bypass_value(want));
                }
            }
            // A refusal is the plugin's: the retries back off like a loss and
            // the episode is the one already counted.
            BypassWrite::Lost | BypassWrite::Refused => self.lost = true,
            BypassWrite::NoOwnBypass => {}
        }
    }
}

/// The bypass parameter's value in the mirror, `None` while it is not read yet
/// or the bypass is not a parameter.
fn mirrored(site: OwnBypass, mirror: &PluginParamValues) -> Option<bool> {
    let OwnBypass::Parameter(id) = site else {
        return None;
    };
    if !mirror.is_ready() {
        return None;
    }
    let at = mirror.position(id)?;
    Some(is_bypassed(mirror.value_at(at)))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};

    use tutti_plugin_host::{ParameterChangeSink, PluginHandle, PluginParamValues};
    use tutti_plugin_types::{
        AnyThread, AutomationMode, AutomationModeError, BlockCounts, BypassWrite, Health,
        LoadedPlugin, Normalized, OwnBypass, ParamFlags, ParamId, ParameterInfo, PluginDescriptor,
        PluginFailure, RenderMode, StateError, TerminationReason, bypass_value, is_bypassed,
    };

    use super::BypassSync;

    pub(crate) const ID: ParamId = ParamId::new(5);

    /// A plugin that holds a bypass; it takes a write only if it `accepts`, and
    /// the write reaches it only if the link `delivers`.
    pub(crate) struct Plugin {
        pub(crate) held: AtomicBool,
        pub(crate) accepts: AtomicBool,
        pub(crate) delivers: AtomicBool,
        pub(crate) writes: AtomicU32,
    }

    impl AnyThread for Plugin {
        fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
            Some(vec![
                ParameterInfo::new(ID, "bypass").with_flags(ParamFlags::BYPASS, ParamFlags::BYPASS),
            ])
        }
        fn parameter_value(&self, _id: ParamId) -> Option<Normalized> {
            Some(bypass_value(self.held.load(SeqCst)))
        }
        fn set_parameter_value(&self, _id: ParamId, value: Normalized) {
            self.writes.fetch_add(1, SeqCst);
            if self.accepts.load(SeqCst) {
                self.held.store(is_bypassed(value), SeqCst);
            }
        }
        fn set_bypassed(&self, site: OwnBypass, bypassed: bool) -> BypassWrite {
            if !self.delivers.load(SeqCst) {
                return BypassWrite::Lost;
            }
            let OwnBypass::Parameter(id) = site else {
                return BypassWrite::Lost;
            };
            self.set_parameter_value(id, bypass_value(bypassed));
            BypassWrite::Sent
        }
        fn parameter_plain(&self, _id: ParamId, _value: Normalized) -> Option<f64> {
            None
        }
        fn parameter_normalized(&self, _id: ParamId, _plain: f64) -> Option<Normalized> {
            None
        }
        fn parameter_text(&self, _id: ParamId, _value: Normalized) -> Option<String> {
            None
        }
        fn parameter_value_from_text(&self, _id: ParamId, _text: &str) -> Option<Normalized> {
            None
        }
        fn save_state(&self) -> Result<Vec<u8>, StateError> {
            Err(StateError::NoStateRoute)
        }
        fn set_render_mode(&self, _mode: RenderMode) -> bool {
            false
        }
        fn set_automation_mode(&self, _mode: AutomationMode) -> Result<(), AutomationModeError> {
            Err(AutomationModeError::PluginDead)
        }
    }

    impl Health for Plugin {
        fn is_crashed(&self) -> bool {
            false
        }
        fn crash_cause(&self) -> Option<PluginFailure> {
            None
        }
        fn rate_refusal(&self) -> Option<(tutti_types::SampleRate, String)> {
            None
        }
        fn block_counts(&self) -> BlockCounts {
            BlockCounts::default()
        }
        fn terminate(&self, _reason: TerminationReason) {}
    }

    /// A handle over a plugin that holds a bypass, in process or served, with the
    /// mirror of its parameters read.
    pub(crate) fn rig(served: bool) -> (PluginHandle, Arc<Plugin>, PluginParamValues) {
        let plugin = Arc::new(Plugin {
            held: false.into(),
            accepts: true.into(),
            delivers: true.into(),
            writes: 0.into(),
        });
        let loaded = LoadedPlugin {
            own_bypass: Some(OwnBypass::Parameter(ID)),
            ..LoadedPlugin::default()
        };
        let handle = PluginHandle::from_backend(
            Arc::clone(&plugin),
            None,
            PluginDescriptor::default(),
            loaded,
            ParameterChangeSink::default(),
        )
        .with_server_pid(served.then_some(1234));
        let (mut mirror, read) = PluginParamValues::start(&handle);
        mirror.land(read.run().expect("the fake answers"));
        (handle, plugin, mirror)
    }

    /// **A write the plugin refuses is retried with a growing wait and counted
    /// once; the count starts again after it takes.** Ticks 0, 2 and 5 write
    /// (1, 2, then 4 ticks of wait), the count stays 1; then the plugin accepts,
    /// a write confirms, and a bypass moved off afterwards is a second episode.
    #[test]
    fn a_refused_write_is_retried_with_backoff_and_counted_once() {
        let (handle, plugin, mirror) = rig(false);
        plugin.accepts.store(false, SeqCst);
        let mut sync = BypassSync::default();
        let mut written_at = Vec::new();
        for tick in 0..8 {
            let before = plugin.writes.load(SeqCst);
            sync.reconcile(&handle, &mirror, true);
            if plugin.writes.load(SeqCst) > before {
                written_at.push(tick);
            }
        }
        assert_eq!(written_at, [0, 2, 5]);
        assert_eq!(sync.unapplied(), 1, "one episode");

        plugin.accepts.store(true, SeqCst);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, true);
        }
        assert_eq!(handle.bypassed(), Some(true), "the plugin took the write");
        assert_eq!(sync.unapplied(), 1, "still the one episode");

        plugin.held.store(false, SeqCst);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, true);
        }
        assert_eq!(handle.bypassed(), Some(true), "moved off, written again");
        assert_eq!(sync.unapplied(), 2, "a second episode");
    }

    /// **A served write the link drops is written again and counted once, with
    /// no read to tell.** The mirror is not told of a write that was not
    /// delivered, so it cannot claim the node's mode; the retries back off, and
    /// the write that gets through is the last.
    #[test]
    fn a_dropped_served_write_is_retried_and_counted_once() {
        let (handle, plugin, mirror) = rig(true);
        plugin.delivers.store(false, SeqCst);
        let mut sync = BypassSync::default();
        sync.assert_mode(&handle, &mirror, true);
        assert_eq!(plugin.writes.load(SeqCst), 0, "the write was dropped");
        assert_eq!(sync.unapplied(), 0, "nothing is counted before a tick");
        for _ in 0..12 {
            sync.reconcile(&handle, &mirror, true);
        }
        assert_eq!(sync.unapplied(), 1, "one episode for all the retries");
        assert_eq!(plugin.writes.load(SeqCst), 0, "still nothing arrived");

        plugin.delivers.store(true, SeqCst);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, true);
        }
        assert!(plugin.held.load(SeqCst), "the retry arrived");
        assert_eq!(sync.unapplied(), 1);
        assert_eq!(plugin.writes.load(SeqCst), 1, "written once it could be");
    }

    /// **A bypass the plugin reports moved is written back, from the mirror.** A
    /// served plugin is never read: the mirror holds the plugin's own report, and
    /// differs from the node's mode.
    #[test]
    fn a_served_plugins_own_report_that_differs_is_written_back() {
        let (handle, plugin, mirror) = rig(true);
        plugin.held.store(true, SeqCst);
        let mut sync = BypassSync::default();
        sync.reconcile(&handle, &mirror, false);
        assert_eq!(
            plugin.writes.load(SeqCst),
            0,
            "the mirror holds the mode and a read would have said otherwise"
        );
        mirror.report(ID, bypass_value(true));
        sync.reconcile(&handle, &mirror, false);
        assert_eq!(plugin.writes.load(SeqCst), 1, "the report differs: written");
        assert_eq!(sync.unapplied(), 1);
    }

    /// **A change of mode is written at once, whatever the wait, and again on
    /// the next tick when that write is lost too.** The first half shows the
    /// write goes out at once after the retries backed off; the second, which
    /// fails without the wait being cleared, shows a lost one is tried again on
    /// the next tick.
    #[test]
    fn a_change_of_mode_is_never_held_back() {
        let (handle, plugin, mirror) = rig(true);
        plugin.delivers.store(false, SeqCst);
        let mut sync = BypassSync::default();
        sync.assert_mode(&handle, &mirror, true);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, true);
        }
        // The backoff is at its longest now. The mode changes, and the link is
        // back: the write goes at once.
        plugin.delivers.store(true, SeqCst);
        sync.assert_mode(&handle, &mirror, false);
        assert_eq!(plugin.writes.load(SeqCst), 1, "written at once");

        // The same change lost: the retry is on the next tick, not 32 ticks on.
        let (handle, plugin, mirror) = rig(true);
        plugin.delivers.store(false, SeqCst);
        let mut sync = BypassSync::default();
        sync.assert_mode(&handle, &mirror, true);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, true);
        }
        sync.assert_mode(&handle, &mirror, false);
        plugin.delivers.store(true, SeqCst);
        sync.reconcile(&handle, &mirror, false);
        assert_eq!(plugin.writes.load(SeqCst), 1, "retried on the next tick");
    }

    /// A plugin that holds the mode is left alone.
    #[test]
    fn a_plugin_in_the_nodes_mode_is_not_written() {
        let (handle, plugin, mirror) = rig(false);
        let mut sync = BypassSync::default();
        for _ in 0..4 {
            sync.reconcile(&handle, &mirror, false);
        }
        assert_eq!(plugin.writes.load(SeqCst), 0);
        assert_eq!(sync.unapplied(), 0);
    }
}
