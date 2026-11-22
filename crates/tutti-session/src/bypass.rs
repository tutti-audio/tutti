//! A hosted plugin's own bypass is the node's mode: what the session writes, how
//! it finds the plugin in the other state, and how often it writes again.

use tutti_graph::NodeMode;
use tutti_plugin_host::{PluginHandle, PluginParamValues};
use tutti_plugin_types::{BypassWrite, OwnBypass, bypass_value, is_bypassed};

/// What the node's mode asks of the plugin's own bypass: a bypassed node
/// bypasses the plugin, any other mode lets it process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setting {
    Processing,
    Bypassed,
}

impl Setting {
    fn of(mode: NodeMode) -> Self {
        if mode == NodeMode::Bypassed {
            Self::Bypassed
        } else {
            Self::Processing
        }
    }

    fn is_bypassed(self) -> bool {
        self == Self::Bypassed
    }
}

/// How the last write went, which decides whether the next tick writes again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    /// The last write reached the plugin, or none was made.
    Open,
    /// The last write did not reach the plugin: it is written again, backing off.
    Lost,
    /// The plugin refused to be written to `setting`: nothing is written until
    /// the node asks for another.
    Refused { setting: Setting },
}

/// One hosted plugin's reconciliation of its own bypass, on the control thread.
///
/// A write is [`Sent`](BypassWrite::Sent), [`Lost`](BypassWrite::Lost) or
/// [`Refused`](BypassWrite::Refused).
/// A `Sent` write is not applied until the plugin is read in the node's mode.
/// Each [`reconcile`](Self::reconcile) compares the plugin with the mode, with
/// no round trip. It writes the mode again where they differ, or where the last
/// write never reached the plugin. Retries wait 1, 2, 4 ... 32 ticks.
///
/// A difference is one episode, counted once until the plugin is back in the
/// mode.
///
/// A refused write is not retried: the refusal holds for the mode it was made
/// for. A refusal counts an episode only when the plugin's bypass, read back,
/// differs from the mode or cannot be read. Lifting the refusal closes the
/// episode: a new mode, [`assert_mode`](Self::assert_mode), or a reload (which
/// makes a new `BypassSync`). A later refusal then counts again, one episode per
/// refused mode. A plugin still in another mode than the node's reopens an
/// episode at the next comparison: the same tick when `reconcile` lifts the
/// refusal, the next tick when `assert_mode` does.
#[derive(Debug)]
pub(crate) struct BypassSync {
    /// Episodes: a bypass found other than the mode after a write.
    unapplied: u64,
    /// Whether an episode is open.
    episode: bool,
    /// How the last write went.
    standing: Standing,
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
            standing: Standing::Open,
            wait: 0,
            backoff: 1,
        }
    }
}

impl BypassSync {
    /// The episodes counted over the plugin's life: a write that was lost, or
    /// refused while the plugin held another mode, or a bypass the plugin's
    /// editor, a restored state or its automation moved since. A refusal counts
    /// once per refused mode, however many ticks follow it. A report that
    /// arrives late can open an episode that the next tick closes.
    pub(crate) fn unapplied(&self) -> u64 {
        self.unapplied
    }

    /// Writes the mode now: it changed or a plugin entered the node. The wait
    /// and the backoff start over, so a change of mode is never held back, and a
    /// write that is lost is tried again on the next tick. A refusal is lifted.
    pub(crate) fn assert_mode(
        &mut self,
        handle: &PluginHandle,
        mirror: &PluginParamValues,
        mode: NodeMode,
    ) {
        self.backoff = 1;
        self.wait = 0;
        self.lift_refusal();
        self.send(handle, mirror, Setting::of(mode));
    }

    /// Compares the plugin's bypass with the mode and writes it again where they
    /// differ. An in-process plugin is read (without waiting for its instance);
    /// an out-of-process one's value is its mirrored parameter, where the session records
    /// what it wrote only once the write was delivered, so a plugin's own refusal
    /// is not seen there and a change it reports (its editor, its automation) is.
    /// A plugin that refused the mode is left alone.
    pub(crate) fn reconcile(
        &mut self,
        handle: &PluginHandle,
        mirror: &PluginParamValues,
        mode: NodeMode,
    ) {
        let Some(_) = handle.loaded().own_bypass else {
            return;
        };
        let want = Setting::of(mode);
        if self.standing == (Standing::Refused { setting: want }) {
            return;
        }
        self.lift_refusal();
        if self.wait > 0 {
            self.wait -= 1;
            return;
        }
        if self.standing != Standing::Lost {
            match held(handle, mirror) {
                None => return,
                Some(held) if held == want.is_bypassed() => {
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

    fn lift_refusal(&mut self) {
        if matches!(self.standing, Standing::Refused { .. }) {
            self.standing = Standing::Open;
            self.episode = false;
            self.wait = 0;
            self.backoff = 1;
        }
    }

    fn send(&mut self, handle: &PluginHandle, mirror: &PluginParamValues, want: Setting) {
        match handle.set_bypassed(want.is_bypassed()) {
            BypassWrite::Sent => {
                self.standing = Standing::Open;
                // An active plugin applies a write at its next block, so the
                // comparison one tick on is not yet an answer.
                self.wait = 1;
                // A served plugin is compared with its mirror, which learns what
                // was delivered as it learns what the plugin reports.
                if let (Some(OwnBypass::Parameter(id)), Some(_)) =
                    (handle.loaded().own_bypass, handle.server_pid())
                {
                    mirror.report(id, bypass_value(want.is_bypassed()));
                }
            }
            BypassWrite::Lost => self.standing = Standing::Lost,
            // Writing again changes nothing, so no write follows until the node
            // asks for another mode. The plugin refusing the mode it already
            // holds is no divergence.
            BypassWrite::Refused => {
                self.standing = Standing::Refused { setting: want };
                if !self.episode && held(handle, mirror) != Some(want.is_bypassed()) {
                    self.episode = true;
                    self.unapplied += 1;
                }
            }
            BypassWrite::NoOwnBypass => {}
        }
    }
}

/// Whether the plugin's own bypass is on now: an in-process plugin is read, a
/// out-of-process one's value is its mirrored parameter. `None` while it cannot be read.
fn held(handle: &PluginHandle, mirror: &PluginParamValues) -> Option<bool> {
    let site = handle.loaded().own_bypass?;
    if handle.server_pid().is_some() {
        mirrored(site, mirror)
    } else {
        handle.bypassed()
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
        AnyThread, AutomationMode, AutomationModeError, BypassWrite, Health, LoadedPlugin,
        OwnBypass, ParamFlags, ParameterInfo, PipelineCounts, PluginFailure, RenderMode,
        StateError, StateUse, TerminationReason, bypass_value, is_bypassed,
    };
    use tutti_units::Normalized;
    use tutti_units::ParamId;

    use tutti_graph::NodeMode;

    use super::{BypassSync, Setting};

    pub(crate) const ID: ParamId = ParamId::new(5);

    /// A plugin that holds a bypass; it takes a write only if it `accepts`, and
    /// the write reaches it only if the link `delivers`.
    pub(crate) struct Plugin {
        pub(crate) held: AtomicBool,
        pub(crate) accepts: AtomicBool,
        pub(crate) delivers: AtomicBool,
        /// The write policy refuses the bypass parameter.
        pub(crate) refuses: AtomicBool,
        /// The plugin answers a read of its bypass.
        pub(crate) readable: AtomicBool,
        /// Writes of the bypass the session attempted, refused or not.
        pub(crate) attempts: AtomicU32,
        pub(crate) writes: AtomicU32,
    }

    impl Plugin {
        /// The plugin's bypass value, `None` when it answers no read.
        fn read(&self) -> Option<Normalized> {
            self.readable
                .load(SeqCst)
                .then(|| bypass_value(self.held.load(SeqCst)))
        }
    }

    impl AnyThread for Plugin {
        fn parameter_descriptors(&self) -> tutti_plugin_types::Answer<Option<Vec<ParameterInfo>>> {
            tutti_plugin_types::Answer::ready(Some(vec![
                ParameterInfo::new(ID, "bypass").with_flags(ParamFlags::BYPASS, ParamFlags::BYPASS),
            ]))
        }
        fn parameter_value(&self, _id: ParamId) -> tutti_plugin_types::Answer<Option<Normalized>> {
            tutti_plugin_types::Answer::ready(self.read())
        }
        /// The way an in-process backend reads it: the plugin's own value, at
        /// once.
        fn bypassed(&self, _site: OwnBypass) -> Option<bool> {
            self.read().map(is_bypassed)
        }

        fn set_parameter_value(
            &self,
            _id: ParamId,
            value: Normalized,
        ) -> Result<(), tutti_plugin_types::UnknownParam> {
            self.writes.fetch_add(1, SeqCst);
            if self.accepts.load(SeqCst) {
                self.held.store(is_bypassed(value), SeqCst);
            }
            Ok(())
        }
        fn set_bypassed(&self, site: OwnBypass, bypassed: bool) -> BypassWrite {
            self.attempts.fetch_add(1, SeqCst);
            if self.refuses.load(SeqCst) {
                return BypassWrite::Refused;
            }
            if !self.delivers.load(SeqCst) {
                return BypassWrite::Lost;
            }
            let OwnBypass::Parameter(id) = site else {
                return BypassWrite::Lost;
            };
            let _ = self.set_parameter_value(id, bypass_value(bypassed));
            BypassWrite::Sent
        }
        fn parameter_plain(&self, _id: ParamId, _value: Normalized) -> Option<f64> {
            None
        }
        fn parameter_normalized(
            &self,
            _id: ParamId,
            _plain: f64,
        ) -> tutti_plugin_types::Answer<Option<Normalized>> {
            tutti_plugin_types::Answer::ready(None)
        }
        fn parameter_text(&self, _id: ParamId, _value: Normalized) -> Option<String> {
            None
        }
        fn parameter_value_from_text(&self, _id: ParamId, _text: &str) -> Option<Normalized> {
            None
        }
        fn save_state(&self, _: StateUse) -> Result<Vec<u8>, StateError> {
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
        fn rate_refusal(&self) -> Option<tutti_plugin_types::RateRefusal> {
            None
        }
        fn block_counts(&self) -> PipelineCounts {
            PipelineCounts::default()
        }
        fn terminate(&self, _reason: TerminationReason) {}
    }

    /// A handle over a plugin that holds a bypass, in process or out of process, with the
    /// mirror of its parameters read.
    pub(crate) fn rig(served: bool) -> (PluginHandle, Arc<Plugin>, PluginParamValues) {
        let plugin = Arc::new(Plugin {
            held: false.into(),
            accepts: true.into(),
            delivers: true.into(),
            refuses: false.into(),
            readable: true.into(),
            attempts: 0.into(),
            writes: 0.into(),
        });
        let loaded = LoadedPlugin {
            own_bypass: Some(OwnBypass::Parameter(ID)),
            ..LoadedPlugin::default()
        };
        let handle = PluginHandle::from_backend(
            Arc::clone(&plugin),
            None,
            tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
            loaded,
            ParameterChangeSink::default(),
        )
        .with_server_pid(served.then_some(1234));
        let (mut mirror, read) = PluginParamValues::start(&handle);
        mirror.land(
            futures_lite::future::block_on(std::future::IntoFuture::into_future(read))
                .expect("the fake answers"),
        );
        (handle, plugin, mirror)
    }

    /// **A served plugin's bypass is read from its mirrored parameter**: a plugin
    /// holding bypass reads `Some(true)`, one processing `Some(false)`, with no
    /// read of the plugin.
    #[test]
    fn a_served_plugins_bypass_is_read_from_its_mirror() {
        let (handle, plugin, _) = rig(true);
        for bypassed in [true, false] {
            plugin.held.store(bypassed, SeqCst);
            // A mirror started after the plugin changed reads its value.
            let (mut mirror, read) = PluginParamValues::start(&handle);
            mirror.land(
                futures_lite::future::block_on(std::future::IntoFuture::into_future(read))
                    .expect("the fake answers"),
            );
            assert_eq!(super::held(&handle, &mirror), Some(bypassed));
        }
    }

    /// **A write the plugin ignores is retried with a growing wait and counted
    /// once; the count starts again after it takes.** Ticks 0, 2 and 5 write
    /// (1, 2, then 4 ticks of wait), the count stays 1; then the plugin accepts,
    /// a write confirms, and a bypass moved off afterwards is a second episode.
    #[test]
    fn an_ignored_write_is_retried_with_backoff_and_counted_once() {
        let (handle, plugin, mirror) = rig(false);
        plugin.accepts.store(false, SeqCst);
        let mut sync = BypassSync::default();
        let mut written_at = Vec::new();
        for tick in 0..8 {
            let before = plugin.writes.load(SeqCst);
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
            if plugin.writes.load(SeqCst) > before {
                written_at.push(tick);
            }
        }
        assert_eq!(written_at, [0, 2, 5]);
        assert_eq!(sync.unapplied(), 1, "one episode");

        plugin.accepts.store(true, SeqCst);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
        }
        assert_eq!(handle.bypassed(), Some(true), "the plugin took the write");
        assert_eq!(sync.unapplied(), 1, "still the one episode");

        plugin.held.store(false, SeqCst);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
        }
        assert_eq!(handle.bypassed(), Some(true), "moved off, written again");
        assert_eq!(sync.unapplied(), 2, "a second episode");
    }

    /// **A write the write policy refuses is attempted once, however many ticks
    /// follow, and again only when the mode changes.** The plugin holds
    /// processing and refuses to be bypassed: one attempt after 80 ticks, one
    /// episode, nothing written; the next mode is attempted at once.
    #[test]
    fn a_refused_write_is_attempted_once_until_the_mode_changes() {
        let (handle, plugin, mirror) = rig(false);
        plugin.refuses.store(true, SeqCst);
        let mut sync = BypassSync::default();
        sync.assert_mode(&handle, &mirror, NodeMode::Bypassed);
        for _ in 0..80 {
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
        }
        assert_eq!(plugin.attempts.load(SeqCst), 1, "no retry");
        assert_eq!(sync.unapplied(), 1, "one episode");
        assert_eq!(plugin.writes.load(SeqCst), 0, "nothing arrived");

        sync.reconcile(&handle, &mirror, NodeMode::Active);
        assert_eq!(
            plugin.attempts.load(SeqCst),
            1,
            "the plugin holds processing: the new mode needs no write"
        );
        plugin.refuses.store(false, SeqCst);
        sync.assert_mode(&handle, &mirror, NodeMode::Bypassed);
        assert_eq!(plugin.attempts.load(SeqCst), 2, "a mode asked for again");
        assert!(plugin.held.load(SeqCst), "and taken");
    }

    /// **A refusal counts when the plugin's bypass cannot be read.** The
    /// plugin answers no read, so the refusal is not known to be harmless.
    #[test]
    fn a_refusal_of_an_unreadable_bypass_counts_one_episode() {
        let (handle, plugin, mirror) = rig(false);
        plugin.refuses.store(true, SeqCst);
        plugin.readable.store(false, SeqCst);
        let mut sync = BypassSync::default();
        sync.assert_mode(&handle, &mirror, NodeMode::Active);
        for _ in 0..10 {
            sync.reconcile(&handle, &mirror, NodeMode::Active);
        }
        assert_eq!(plugin.attempts.load(SeqCst), 1);
        assert_eq!(sync.unapplied(), 1, "it cannot be read, so it is counted");
    }

    /// **Lifting a refusal that `reconcile` itself met compares on that tick.**
    /// The ignored writes back the retries off; the refusal then arrives from
    /// `reconcile`'s own send and leaves a long wait, and the node's next mode
    /// is attempted on the same tick instead of after it.
    #[test]
    fn a_lifted_refusal_from_reconcile_is_compared_on_the_same_tick() {
        let (handle, plugin, mirror) = rig(false);
        plugin.accepts.store(false, SeqCst);
        let mut sync = BypassSync::default();
        while plugin.writes.load(SeqCst) < 3 {
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
        }
        plugin.refuses.store(true, SeqCst);
        let before = plugin.attempts.load(SeqCst);
        while plugin.attempts.load(SeqCst) == before {
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
        }
        let refused_at = plugin.attempts.load(SeqCst);
        // The plugin's bypass is moved on behind the host's back: processing
        // now differs from what it holds.
        plugin.held.store(true, SeqCst);
        sync.reconcile(&handle, &mirror, NodeMode::Active);
        assert_eq!(
            plugin.attempts.load(SeqCst),
            refused_at + 1,
            "attempted on the tick that lifted the refusal"
        );
    }

    /// **Muted lets the plugin process: after Bypassed, it writes processing.**
    #[test]
    fn muted_writes_the_plugin_processing() {
        let (handle, plugin, mirror) = rig(false);
        let mut sync = BypassSync::default();
        sync.assert_mode(&handle, &mirror, NodeMode::Bypassed);
        assert_eq!(handle.bypassed(), Some(true));
        sync.assert_mode(&handle, &mirror, NodeMode::Muted);
        assert_eq!(handle.bypassed(), Some(false), "a muted node's plugin runs");
        assert_eq!(plugin.writes.load(SeqCst), 2);
    }

    /// **Only a bypassed node bypasses its plugin.**
    #[test]
    fn only_bypassed_maps_to_the_bypassed_setting() {
        assert_eq!(Setting::of(NodeMode::Active), Setting::Processing);
        assert_eq!(Setting::of(NodeMode::Muted), Setting::Processing);
        assert_eq!(Setting::of(NodeMode::Bypassed), Setting::Bypassed);
    }

    /// **A plugin that refuses the mode it already holds is not counted.** It
    /// enters holding processing and refuses to be written processing.
    #[test]
    fn a_refusal_at_the_held_value_counts_no_episode() {
        let (handle, plugin, mirror) = rig(false);
        plugin.refuses.store(true, SeqCst);
        let mut sync = BypassSync::default();
        sync.assert_mode(&handle, &mirror, NodeMode::Active);
        for _ in 0..10 {
            sync.reconcile(&handle, &mirror, NodeMode::Active);
        }
        assert_eq!(plugin.attempts.load(SeqCst), 1);
        assert_eq!(sync.unapplied(), 0, "it holds what the node wants");
    }

    /// **A refusal counts once per refused mode.** Bypassed is refused; the
    /// plugin's bypass is then moved to on behind the host's back, so Active is
    /// refused while it holds the other value and counts again; moved off again,
    /// Bypassed is refused and counts a third time.
    #[test]
    fn a_refusal_of_a_later_mode_counts_again() {
        let (handle, plugin, mirror) = rig(false);
        plugin.refuses.store(true, SeqCst);
        let mut sync = BypassSync::default();
        sync.assert_mode(&handle, &mirror, NodeMode::Bypassed);
        assert_eq!(sync.unapplied(), 1);
        // The plugin is moved to bypassed behind the host's back; Active is now
        // refused while it holds the other value.
        plugin.held.store(true, SeqCst);
        sync.assert_mode(&handle, &mirror, NodeMode::Active);
        assert_eq!(sync.unapplied(), 2, "the second refused mode");
        plugin.held.store(false, SeqCst);
        sync.assert_mode(&handle, &mirror, NodeMode::Bypassed);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
        }
        assert_eq!(sync.unapplied(), 3, "and the third");
        assert_eq!(plugin.attempts.load(SeqCst), 3, "one attempt each");
    }

    /// **An out-of-process write the link drops is written again and counted once, with
    /// no read to tell.** The mirror is not told of a write that was not
    /// delivered, so it cannot claim the node's mode; the retries back off, and
    /// the write that gets through is the last.
    #[test]
    fn a_dropped_served_write_is_retried_and_counted_once() {
        let (handle, plugin, mirror) = rig(true);
        plugin.delivers.store(false, SeqCst);
        let mut sync = BypassSync::default();
        sync.assert_mode(&handle, &mirror, NodeMode::Bypassed);
        assert_eq!(plugin.writes.load(SeqCst), 0, "the write was dropped");
        assert_eq!(sync.unapplied(), 0, "nothing is counted before a tick");
        for _ in 0..12 {
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
        }
        assert_eq!(sync.unapplied(), 1, "one episode for all the retries");
        assert_eq!(plugin.writes.load(SeqCst), 0, "still nothing arrived");

        plugin.delivers.store(true, SeqCst);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
        }
        assert!(plugin.held.load(SeqCst), "the retry arrived");
        assert_eq!(sync.unapplied(), 1);
        assert_eq!(plugin.writes.load(SeqCst), 1, "written once it could be");
    }

    /// **A bypass the plugin reports moved is written back, from the mirror.** A
    /// out-of-process plugin is never read: the mirror holds the plugin's own report, and
    /// differs from the node's mode.
    #[test]
    fn a_served_plugins_own_report_that_differs_is_written_back() {
        let (handle, plugin, mirror) = rig(true);
        plugin.held.store(true, SeqCst);
        let mut sync = BypassSync::default();
        sync.reconcile(&handle, &mirror, NodeMode::Active);
        assert_eq!(
            plugin.writes.load(SeqCst),
            0,
            "the mirror holds the mode and a read would have said otherwise"
        );
        mirror.report(ID, bypass_value(true));
        sync.reconcile(&handle, &mirror, NodeMode::Active);
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
        sync.assert_mode(&handle, &mirror, NodeMode::Bypassed);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
        }
        // The backoff is at its longest now. The mode changes, and the link is
        // back: the write goes at once.
        plugin.delivers.store(true, SeqCst);
        sync.assert_mode(&handle, &mirror, NodeMode::Active);
        assert_eq!(plugin.writes.load(SeqCst), 1, "written at once");

        // The same change lost: the retry is on the next tick, not 32 ticks on.
        let (handle, plugin, mirror) = rig(true);
        plugin.delivers.store(false, SeqCst);
        let mut sync = BypassSync::default();
        sync.assert_mode(&handle, &mirror, NodeMode::Bypassed);
        for _ in 0..40 {
            sync.reconcile(&handle, &mirror, NodeMode::Bypassed);
        }
        sync.assert_mode(&handle, &mirror, NodeMode::Active);
        plugin.delivers.store(true, SeqCst);
        sync.reconcile(&handle, &mirror, NodeMode::Active);
        assert_eq!(plugin.writes.load(SeqCst), 1, "retried on the next tick");
    }

    /// A plugin that holds the mode is left alone.
    #[test]
    fn a_plugin_in_the_nodes_mode_is_not_written() {
        let (handle, plugin, mirror) = rig(false);
        let mut sync = BypassSync::default();
        for _ in 0..4 {
            sync.reconcile(&handle, &mirror, NodeMode::Active);
        }
        assert_eq!(plugin.writes.load(SeqCst), 0);
        assert_eq!(sync.unapplied(), 0);
    }
}
