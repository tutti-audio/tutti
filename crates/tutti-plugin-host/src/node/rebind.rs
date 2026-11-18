//! [`Rebinder`]: a new node over the plugin a node already drives, with the
//! ports the plugin now declares.
//!
//! A plugin that changes its audio ports (`IoChanged`) cannot be served by a
//! node whose shape is fixed when it is inserted. The control side builds a
//! successor over the same instance instead, in two steps so the plugin is
//! touched last: [`Rebinder::prepare`] builds the node and a [`Pending`] and
//! changes nothing; the caller puts the node in its graph; and only then
//! [`Pending::commit`] names the node's realtime end the Processor's way
//! ([`Hand::install`](tutti_plugin_types::Hand::install)) and recalls the
//! Processor (a [`HostOp::Relend`](tutti_plugin_types::HostOp::Relend), with no
//! restart). The node it replaces hands the Processor back at its next block,
//! and the instance's next pump lends it to the successor.
//!
//! A node dropped while it holds the Processor and a recall is raised returns
//! the Processor to the control side, so the replaced node need not stay in the
//! graph until [`Rebinder::settled`] says [`Moved`](Settlement::Moved). When
//! the plugin dies meanwhile ([`Lost`](Settlement::Lost)) no Processor moves,
//! and neither node renders.

use std::sync::Arc;

use parking_lot::Mutex;
use tutti_plugin_types::{Features, Loaded, MainThreadMarker, PluginDescriptor, Successor};
use tutti_rt::Tally;
use tutti_transport::TempoReader;
use tutti_types::ParamId;
use tutti_types::{ParkedSampleRate, Samples};

use super::client::{Declared, InProcClient, PluginNode, Reload};
use super::instance::Instance;
use crate::PluginControls;

/// Why a [`Rebinder`] built no node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
pub enum RebindError {
    /// The plugin is lost, or has reported no ports.
    #[error("the plugin has no ports to rebuild a node with: it is lost or has reported none")]
    NoPorts,
    /// The plugin is loaded or refused: it has no Processor to move.
    #[error("the plugin is not running, so there is no Processor to move")]
    NotRunning,
    /// The plugin's own restart, or a request it raised, is not finished: the
    /// layout read is not the one it will run with. Pump and try again.
    #[error("the plugin is restarting; its ports are not settled")]
    Restarting,
    /// The plugin now has more channels in a direction than a block serves.
    #[error(transparent)]
    TooWide(#[from] tutti_plugin_types::PluginTooWide),
}

/// Builds the node that replaces a plugin node whose plugin changed its ports.
///
/// Made from the node it replaces ([`InProcClient::rebinder`]) and kept on the
/// control side. [`prepare`](Self::prepare) takes the instance lock, so never the
/// audio thread.
pub struct Rebinder<F: Loaded> {
    inner: Arc<Mutex<Instance<F>>>,
    descriptor: PluginDescriptor,
    reload: Option<Reload>,
    features: Features,
    sample_rate: tutti_types::SampleRate,
    pending_sample_rate: Arc<ParkedSampleRate>,
    unlent: Tally<u64>,
    parameters: Vec<ParamId>,
    spares: usize,
    controls: Option<PluginControls>,
    /// The pipeline the node had when the rebinder was made, for a node over
    /// a plugin whose `controls` hold none.
    pipeline: Samples,
    /// The node declares a bypass of the plugin's own.
    own_bypass: bool,
    tempo: TempoReader,
}

impl<F: Loaded> Clone for Rebinder<F> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            descriptor: self.descriptor.clone(),
            reload: self.reload.clone(),
            features: self.features,
            sample_rate: self.sample_rate,
            pending_sample_rate: Arc::clone(&self.pending_sample_rate),
            unlent: self.unlent.clone(),
            parameters: self.parameters.clone(),
            spares: self.spares,
            controls: self.controls.clone(),
            pipeline: self.pipeline,
            own_bypass: self.own_bypass,
            tempo: self.tempo.clone(),
        }
    }
}

impl<F: Loaded> std::fmt::Debug for Rebinder<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rebinder")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl<F: Loaded + 'static> Rebinder<F>
where
    F::Processor: 'static,
{
    /// A node over the plugin with the ports it declares now, and the
    /// [`Pending`] that moves the plugin's Processor to it.
    ///
    /// Nothing is changed in the plugin: the node's realtime end is empty and
    /// renders silence until [`Pending::commit`] names it the Processor's way,
    /// after the caller has put the node in its graph. A caller that cannot
    /// place the node drops both, and the plugin is as it was.
    ///
    /// Call it after the pump has read the plugin's new ports (the changes it
    /// returned said `io`): the node declares the layout the lifecycle holds.
    /// The node starts with a fresh meter and counters, silent blocks counted by
    /// its own `unlent_blocks`; its parameter slots are the plugin's, and its
    /// pipeline figure is the replaced node's until a `prepare` that retunes
    /// the Processor reads it.
    ///
    /// # Errors
    ///
    /// [`NoPorts`](RebindError::NoPorts) for a lost plugin;
    /// [`TooWide`](RebindError::TooWide) for ports wider than a block serves;
    /// [`NotRunning`](RebindError::NotRunning) for a plugin that is loaded or
    /// refused; [`Restarting`](RebindError::Restarting) while its own restart is
    /// under way.
    pub fn prepare(&self) -> Result<(PluginNode<F>, Pending<F>), RebindError> {
        let instance = self.inner.lock();
        let (layout, latency, tail) = instance.reading().ok_or(RebindError::NoPorts)?;
        let mut declared = Declared::from_layout(layout, latency, tail)?;
        declared.ports.own_bypass = self.own_bypass;
        instance.ready_to_rehand()?;
        drop(instance);
        let (successor, rt) = Successor::new();
        let pipeline = self
            .controls
            .as_ref()
            .map_or(self.pipeline, PluginControls::pipeline);
        let node = InProcClient::new(
            Arc::clone(&self.inner),
            rt,
            self.descriptor.clone(),
            self.reload.clone(),
            declared,
            self.features,
            self.sample_rate,
            Arc::clone(&self.pending_sample_rate),
            // Its own: the node it replaces keeps counting its silent blocks
            // into the old cell, which nothing reads once it is replaced.
            tutti_rt::tally(),
            self.parameters.iter().copied(),
            self.spares,
            self.controls.clone(),
            Some(pipeline),
        )
        .bind(self.tempo.clone());
        Ok((
            node,
            Pending {
                inner: Arc::clone(&self.inner),
                successor,
            },
        ))
    }

    /// The ports the plugin has now, as its lifecycle last read them: what a
    /// node [`prepare`](Self::prepare) built now would declare. `None` for a
    /// lost plugin or before a layout was read.
    #[must_use]
    pub fn layout(&self) -> Option<tutti_plugin_types::Layout> {
        self.inner
            .lock()
            .reading()
            .map(|(layout, ..)| layout.clone())
    }

    /// Runs the plugin's lifecycle once: takes what the plugin asked for (a
    /// restart for new buses among it) and carries it out. What a handle's
    /// `editor_idle` does for an in-process plugin; a served plugin's handle
    /// has no instance to tick, so its rebinder does. The changes it finds are
    /// reported by the plugin's own listener already. Main thread.
    pub fn tick(&self, main: MainThreadMarker) {
        let _ = self.inner.lock().pump(main);
    }

    /// Where the Processor's way is: see [`Settlement`].
    #[must_use]
    pub fn settled(&self) -> Settlement {
        self.inner.lock().settlement()
    }
}

/// The move of a plugin's Processor to a node [`Rebinder::prepare`] built,
/// held until the node is in its graph.
///
/// Dropping it without [`commit`](Self::commit) changes nothing: the node it
/// came with stays empty and silent.
#[must_use = "a node renders nothing until its move is committed"]
pub struct Pending<F: Loaded> {
    inner: Arc<Mutex<Instance<F>>>,
    successor: Successor<F::Processor>,
}

impl<F: Loaded> std::fmt::Debug for Pending<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending").finish_non_exhaustive()
    }
}

impl<F: Loaded> Pending<F> {
    /// Names the node's realtime end the Processor's way, and recalls the
    /// Processor from the node it replaces: the plugin keeps running, and the
    /// Processor is lent to this node at the instance's next pump, with no
    /// deactivation. The node it replaces hands the Processor back at its
    /// next block, or when it is dropped (the recall is raised, so a node
    /// dropped while it holds the Processor returns it to the control side).
    ///
    /// Call it once the node is in the graph. It does not fail, whatever the
    /// plugin's state is by then (it may have changed since
    /// [`Rebinder::prepare`]): the Processor goes to this node at its next
    /// lend, and a plugin that is not active has none lent. See
    /// [`Rebinder::settled`] for when it has.
    ///
    /// A second node's commit before the first one's Processor has moved
    /// replaces the first: its node never receives a Processor.
    pub fn commit(self, main: MainThreadMarker) {
        self.inner.lock().commit(main, self.successor);
    }
}

/// Where the Processor is on its way to the newest node a [`Rebinder`] built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Settlement {
    /// Nothing was committed: the Processor's way is the node the plugin
    /// started with.
    Idle,
    /// The Processor has not been lent to the newest node. The node it
    /// replaces may still hold it and keeps rendering with it; it stays in
    /// the graph until the pump moves it. A plugin that is deactivated meanwhile
    /// leaves this until it is activated again.
    Pending,
    /// The Processor's way is the newest node's: the node it replaces holds
    /// none and may be dropped.
    Moved,
    /// The plugin died. No Processor will reach the newest node, and the
    /// node it replaces may still hold one whose plugin is gone: the plugin
    /// is destroyed already, so that node's drop does not run `stop` on it, and
    /// both nodes render silence. The plugin's own death path reports it.
    Lost,
}

impl<F: Loaded> PluginNode<F> {
    /// The [`Rebinder`] for this node, binding its successors to the tempo
    /// reader the node was bound to. See [`InProcClient::rebinder`].
    #[must_use]
    pub fn rebinder(&self) -> Rebinder<F> {
        self.client.rebinder(self.tempo.clone())
    }
}

impl<F: Loaded> InProcClient<F> {
    /// The [`Rebinder`] for this node, over the same plugin, binding its
    /// successors to `tempo`.
    #[must_use]
    pub fn rebinder(&self, tempo: TempoReader) -> Rebinder<F> {
        Rebinder {
            inner: Arc::clone(&self.inner),
            descriptor: self.descriptor.clone(),
            reload: self.reload.clone(),
            features: self.features,
            sample_rate: self.sample_rate,
            pending_sample_rate: Arc::clone(&self.pending_sample_rate),
            unlent: self.unlent.clone(),
            parameters: self.parameters.clone(),
            spares: self.spares,
            controls: self.controls.clone(),
            pipeline: self.pipeline,
            own_bypass: self.declared.ports.own_bypass,
            tempo,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tutti_buffers::Planes;

    use tutti_fixtures::main_thread;
    use tutti_graph::contract::{self, Driven, drive_input};
    use tutti_graph::{Env, Node, PlayClock, TransportChanges, TransportPosition};
    use tutti_plugin_types::fake::{Call, FakeLoaded, FakePlugin};
    use tutti_plugin_types::{Control, Loaded, Prepare, Request};
    use tutti_tempo::TempoMap;
    use tutti_types::{Bpm, Frame, MaxBlock, SampleRate, Samples};

    use super::*;
    use crate::client_over;

    const SR: SampleRate = SampleRate::SR_48K;
    const BLOCK: usize = 64;

    fn tempo() -> TempoReader {
        TempoReader::fixed(Arc::new(
            TempoMap::constant(Bpm::of(120.0)).expect("a tempo"),
        ))
    }

    /// A fake plugin hosted as a served one is, with a rebinder over it, and
    /// the node it replaces, prepared.
    fn rig() -> (
        FakePlugin,
        Rebinder<FakeLoaded>,
        PluginNode<FakeLoaded>,
        Arc<Mutex<Instance<FakeLoaded>>>,
    ) {
        rig_with(false)
    }

    /// [`rig`], the plugin declaring a bypass of its own when `own_bypass`.
    fn rig_with(
        own_bypass: bool,
    ) -> (
        FakePlugin,
        Rebinder<FakeLoaded>,
        PluginNode<FakeLoaded>,
        Arc<Mutex<Instance<FakeLoaded>>>,
    ) {
        let (fake, loaded) = FakePlugin::new();
        // A voice that sounds for a block after each impulse.
        fake.ring_out(100);
        let main = main_thread();
        let mut wiring = loaded.wiring(main);
        if own_bypass {
            wiring.own_bypass = Some(tutti_plugin_types::OwnBypass::Parameter(
                tutti_types::ParamId::new(4),
            ));
        }
        let descriptor = loaded.info().clone();
        let prepare = Prepare::new(SR, MaxBlock::new(Samples(BLOCK)).expect("a block"));
        let Ok((processor, controller)) = loaded.activate(main, prepare) else {
            panic!("the fake activates");
        };
        let client = client_over::<FakeLoaded>(
            processor,
            controller,
            prepare,
            descriptor,
            &wiring,
            [],
            0,
            None,
        )
        .expect("the fake is narrow");
        let inner = Arc::clone(&client.inner);
        let rebinder = client.rebinder(tempo());
        let node = contract::prepared(client.bind(tempo()), SR, BLOCK);
        (fake, rebinder, node, inner)
    }

    /// **The node declares the bound one event per frame of its block gives,
    /// within the graph's port limits** (`EventDensity::per_frame`: a floor
    /// of 256 events, a cap of 8192): 256 at the 64-frame block it was
    /// built for, 2048 at 2048 frames, 8192 at 1 << 20.
    #[test]
    fn the_node_declares_one_event_per_frame_of_its_block_as_its_input_bound() {
        let (_fake, _rebinder, node, _inner) = rig();
        let bound = |node: &PluginNode<FakeLoaded>| node.shape().event_input_capacity;
        assert_eq!(bound(&node).map(|n| n.get()), Some(256));
        let node = contract::prepared(node, SR, 2048);
        assert_eq!(bound(&node).map(|n| n.get()), Some(2048));
        let node = contract::prepared(node, SR, 1 << 20);
        assert_eq!(bound(&node).map(|n| n.get()), Some(8_192));
    }

    /// A node over the plugin's ports now, committed at once: the caller's
    /// swap that cannot fail.
    fn rebind(rebinder: &Rebinder<FakeLoaded>) -> Result<PluginNode<FakeLoaded>, RebindError> {
        let (node, pending) = rebinder.prepare()?;
        pending.commit(main_thread());
        Ok(node)
    }

    fn env(frame: u64) -> Env {
        Env {
            frame: Frame::new(frame),
            sample_rate: SR,
            block_len: Samples(BLOCK),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        }
    }

    /// One block with an impulse on both inputs: a sounding plugin answers it.
    fn block(node: &mut PluginNode<FakeLoaded>, frame: u64) -> Driven {
        let mut x = vec![0.0f32; BLOCK];
        x[0] = 1.0;
        drive_input(node, &env(frame), Planes::new(&[&x, &x]).unwrap(), &[], &[])
    }

    /// The plugin changes its ports to `outputs` channels and the pump reads
    /// it, as the host's editor idle does: the old node hands the Processor
    /// back and gets the restarted one.
    fn change_ports(
        fake: &FakePlugin,
        node: &mut PluginNode<FakeLoaded>,
        inner: &Mutex<Instance<FakeLoaded>>,
        outputs: u16,
    ) {
        fake.set_outputs(outputs);
        assert!(Request::IoChanged.raise(fake.request_word()));
        let main = main_thread();
        // The pump asks for the Processor back, the old node returns it at its
        // block, and a later pump restarts the plugin and reads its layout.
        let read = |inner: &Mutex<Instance<FakeLoaded>>| {
            inner
                .lock()
                .reading()
                .map(|(layout, ..)| layout.total_outputs())
        };
        let mut frame = 0;
        while read(inner) != Some(usize::from(outputs)) {
            inner.lock().pump(main);
            block(node, frame);
            frame += BLOCK as u64;
            assert!(frame < 16 * BLOCK as u64, "the restart never finished");
        }
    }

    /// The pump and an old-node block until `rebinder` says the Processor
    /// moved.
    fn settle(
        rebinder: &Rebinder<FakeLoaded>,
        old: &mut PluginNode<FakeLoaded>,
        inner: &Mutex<Instance<FakeLoaded>>,
        mut frame: u64,
    ) -> u64 {
        while rebinder.settled() == Settlement::Pending {
            block(old, frame);
            inner.lock().pump(main_thread());
            frame += BLOCK as u64;
            assert!(frame < 64 * BLOCK as u64, "the Processor never moved");
        }
        frame
    }

    fn activations(fake: &FakePlugin) -> usize {
        fake.calls()
            .iter()
            .filter(|c| matches!(c, Call::Activate(..)))
            .count()
    }

    fn deactivations(fake: &FakePlugin) -> usize {
        fake.calls()
            .iter()
            .filter(|c| matches!(c, Call::Deactivate))
            .count()
    }

    /// **A rebuilt node declares the ports the plugin has now, and the
    /// Processor lands in it, not in the node it replaces.** The old node
    /// declares two outputs and renders silence once the Processor has left
    /// it; the new one declares four and renders the plugin's sound. The
    /// move itself restarts nothing: only the plugin's own port change did.
    #[test]
    fn the_processor_lands_in_the_rebuilt_node_and_the_old_one_is_left_empty() {
        let (fake, rebinder, mut old, inner) = rig();
        assert_eq!(old.shape().audio_out().count(), 2);
        assert!(block(&mut old, 0).audio[0].iter().any(|&s| s != 0.0));

        change_ports(&fake, &mut old, &inner, 4);
        let restarts = (activations(&fake), deactivations(&fake));
        let mut new = rebind(&rebinder).expect("a node");
        assert_eq!(new.shape().audio_out().count(), 4, "the plugin's ports now");
        assert_eq!(
            old.shape().audio_out().count(),
            2,
            "the old node is as it was"
        );

        let frame = settle(&rebinder, &mut old, &inner, BLOCK as u64);
        assert_eq!(
            (activations(&fake), deactivations(&fake)),
            restarts,
            "the Processor moved without a deactivation or an activation"
        );
        let heard = block(&mut new, frame);
        assert_eq!(heard.audio.len(), 4);
        assert!(
            heard.audio[0].iter().any(|&s| s != 0.0),
            "the new node sounds"
        );
        let silent = block(&mut old, frame + BLOCK as u64);
        assert!(
            silent.audio.iter().flatten().all(|&s| s == 0.0),
            "the old node holds no Processor"
        );
    }

    /// **A rebuilt node declares the bypass of the plugin's own that the node
    /// it replaces declared.** A layout carries no bypass, so the rebinder keeps
    /// what the first node declared.
    #[test]
    fn a_rebuilt_node_keeps_the_own_bypass_of_the_original() {
        let (_fake, rebinder, old, _inner) = rig_with(true);
        assert!(old.shape().own_bypass, "the original declares one");
        let (new, _pending) = rebinder.prepare().expect("a node");
        assert!(new.shape().own_bypass, "and so does its successor");
        let (_fake, rebinder, old, _inner) = rig_with(false);
        assert!(!old.shape().own_bypass);
        let (new, _pending) = rebinder.prepare().expect("a node");
        assert!(!new.shape().own_bypass, "none where there was none");
    }

    /// **A prepared node that is never committed changes nothing:** the plugin
    /// still renders through the node it has, nothing is recalled, and the
    /// Processor's way is as it was ([`Settlement::Idle`]).
    #[test]
    fn a_prepared_node_that_is_dropped_uncommitted_leaves_the_plugin_alone() {
        let (fake, rebinder, mut old, inner) = rig();
        block(&mut old, 0);
        let (new, pending) = rebinder.prepare().expect("a node");
        drop((new, pending));
        assert_eq!(rebinder.settled(), Settlement::Idle);
        for frame in 0..4 {
            inner.lock().pump(main_thread());
            assert!(
                block(&mut old, frame * BLOCK as u64).audio[0]
                    .iter()
                    .any(|&s| s != 0.0),
                "the old node still holds the Processor"
            );
        }
        assert_eq!(activations(&fake), 1);
        assert_eq!(deactivations(&fake), 0);
    }

    /// **A rebind over unchanged ports moves the Processor with no restart:**
    /// one activation, no deactivation, in the whole run.
    #[test]
    fn a_rebind_over_unchanged_ports_moves_the_processor() {
        let (fake, rebinder, mut old, inner) = rig();
        block(&mut old, 0);
        let mut new = rebind(&rebinder).expect("a node");
        assert_eq!(new.shape().audio_out().count(), 2);
        let frame = settle(&rebinder, &mut old, &inner, BLOCK as u64);
        assert!(block(&mut new, frame).audio[0].iter().any(|&s| s != 0.0));
        assert_eq!(activations(&fake), 1, "the load's activation, no other");
        assert_eq!(deactivations(&fake), 0);
    }

    /// **Dropping the old node while it holds the Processor hands the
    /// Processor back instead of dropping it:** the rebind raised the recall,
    /// the drop returns the Processor through the handoff, and the pump lends
    /// it to the new node.
    #[test]
    fn an_old_node_dropped_with_the_processor_returns_it_to_the_new_one() {
        let (_fake, rebinder, mut old, inner) = rig();
        block(&mut old, 0);
        let mut new = rebind(&rebinder).expect("a node");
        assert_eq!(rebinder.settled(), Settlement::Pending);
        drop(old);
        inner.lock().pump(main_thread());
        assert_eq!(rebinder.settled(), Settlement::Moved);
        assert!(block(&mut new, 0).audio[0].iter().any(|&s| s != 0.0));
    }

    /// **A rebind while the plugin's own restart is parked is refused by name
    /// and changes nothing;** once the restart ends, it succeeds.
    #[test]
    fn a_rebind_during_the_plugins_own_restart_is_refused_until_it_ends() {
        let (fake, rebinder, mut old, inner) = rig();
        block(&mut old, 0);
        fake.set_outputs(4);
        assert!(Request::IoChanged.raise(fake.request_word()));
        inner.lock().pump(main_thread());
        assert_eq!(
            rebind(&rebinder).expect_err("parked"),
            RebindError::Restarting
        );
        assert_eq!(rebinder.settled(), Settlement::Idle, "nothing rehanded");
        let mut frame = 0;
        while rebind(&rebinder).is_err() {
            block(&mut old, frame);
            inner.lock().pump(main_thread());
            frame += BLOCK as u64;
            assert!(frame < 16 * BLOCK as u64, "the restart never ended");
        }
    }

    /// **A plugin that refused its rate is not running: a rebind is refused
    /// by name and nothing is rehanded,** so no node waits for a Processor.
    #[test]
    fn a_refused_plugin_is_not_rebound() {
        let (fake, rebinder, mut old, inner) = rig();
        block(&mut old, 0);
        fake.refuse_rate(Some(SampleRate::SR_44K1));
        let main = main_thread();
        let _ = inner
            .lock()
            .set_sample_rate(main, SampleRate::SR_44K1, None);
        let mut frame = BLOCK as u64;
        while inner.lock().renders() {
            block(&mut old, frame);
            inner.lock().pump(main);
            frame += BLOCK as u64;
            assert!(frame < 16 * BLOCK as u64, "the plugin never refused");
        }
        assert_eq!(
            rebind(&rebinder).expect_err("refused"),
            RebindError::NotRunning
        );
        assert_eq!(rebinder.settled(), Settlement::Idle);
    }

    /// **A plugin that dies after a rebind settles as `Lost`, not `Moved`:**
    /// the caller detaches the old node before dropping it.
    #[test]
    fn a_plugin_that_dies_after_a_rebind_is_lost_not_moved() {
        let (fake, rebinder, mut old, inner) = rig();
        block(&mut old, 0);
        let _new = rebind(&rebinder).expect("a node");
        fake.panic_in_latency(true);
        assert!(Request::LatencyChanged.raise(fake.request_word()));
        let mut frame = BLOCK as u64;
        while rebinder.settled() != Settlement::Lost {
            block(&mut old, frame);
            inner.lock().pump(main_thread());
            frame += BLOCK as u64;
            assert!(frame < 16 * BLOCK as u64, "the plugin never died");
        }
    }

    /// **A plugin that dies has no ports:** the rebuild is refused `NoPorts`.
    #[test]
    fn a_plugin_that_dies_has_no_ports() {
        let (fake, rebinder, mut old, inner) = rig();
        block(&mut old, 0);
        assert!(rebinder.layout().is_some(), "alive, it reports its ports");
        fake.panic_in_latency(true);
        assert!(Request::LatencyChanged.raise(fake.request_word()));
        let mut frame = BLOCK as u64;
        while rebinder.settled() != Settlement::Lost {
            block(&mut old, frame);
            inner.lock().pump(main_thread());
            frame += BLOCK as u64;
            assert!(frame < 16 * BLOCK as u64, "the plugin never died");
        }
        assert_eq!(rebinder.layout(), None);
        assert!(matches!(rebinder.prepare(), Err(RebindError::NoPorts)));
    }
}
