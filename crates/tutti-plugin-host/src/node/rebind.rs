//! [`Rebinder`]: a new node over the plugin a node already drives, with the
//! ports the plugin now declares.
//!
//! A plugin that changes its audio ports (`IoChanged`) cannot be served by a
//! node whose shape is fixed when it is inserted. The control side builds a
//! successor over the same instance instead, and the session swaps it in for
//! the node it replaces. The Processor changes nodes through the instance's
//! [`Hand::rehand`](tutti_plugin_types::Hand::rehand): the node in the graph
//! hands it back at its next block boundary, the plugin restarts, and the
//! restarted Processor is lent to the successor's realtime end.
//!
//! The old node keeps rendering until the Processor has left it. The caller
//! keeps it in the graph until [`Rebinder::settled`] says the Processor's way
//! is the successor's: dropping a node whose end still holds the Processor
//! drops the Processor with it.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use parking_lot::Mutex;
use tutti_plugin_types::{Features, Loaded, MainThreadMarker, ParamId, PluginDescriptor};
use tutti_transport::TempoReader;
use tutti_types::Samples;

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
    /// The ports the plugin now has differ from the ones the node's slab was
    /// sized for.
    #[error(
        "the plugin now has {now_inputs} input and {now_outputs} output channels, \
         but its slab holds {inputs} and {outputs}"
    )]
    OutsideSlab {
        /// The input channels the pipeline was sized for.
        inputs: usize,
        /// The output channels the pipeline was sized for.
        outputs: usize,
        /// The input channels the plugin has now.
        now_inputs: usize,
        /// The output channels the plugin has now.
        now_outputs: usize,
    },
}

/// The ports a node's shared slab was sized for, which a rebuilt node must not
/// differ from: an out-of-process plugin's shared slab is laid out once, at
/// launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortLimit {
    /// The input channels.
    pub inputs: usize,
    /// The output channels.
    pub outputs: usize,
}

/// Builds the node that replaces a plugin node whose plugin changed its ports.
///
/// Made from the node it replaces ([`InProcClient::rebinder`]) and kept on the
/// control side. [`rebind`](Self::rebind) takes the instance lock, so never the
/// audio thread.
pub struct Rebinder<F: Loaded> {
    inner: Arc<Mutex<Instance<F>>>,
    descriptor: PluginDescriptor,
    reload: Option<Reload>,
    features: Features,
    sample_rate: f64,
    pending_sample_rate: Arc<AtomicU64>,
    unlent: Arc<AtomicU64>,
    parameters: Vec<ParamId>,
    spares: usize,
    controls: Option<PluginControls>,
    /// The pipeline the node had when the rebinder was made, for a node over
    /// a plugin whose `controls` hold none.
    pipeline: Samples,
    tempo: TempoReader,
    limit: Option<PortLimit>,
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
            unlent: Arc::clone(&self.unlent),
            parameters: self.parameters.clone(),
            spares: self.spares,
            controls: self.controls.clone(),
            pipeline: self.pipeline,
            tempo: self.tempo.clone(),
            limit: self.limit,
        }
    }
}

impl<F: Loaded> std::fmt::Debug for Rebinder<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rebinder")
            .field("descriptor", &self.descriptor)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl<F: Loaded + 'static> Rebinder<F>
where
    F::Processor: 'static,
{
    /// A node over the plugin with the ports it declares now, and the move
    /// of its Processor to the node: the plugin keeps running, and the
    /// Processor is recalled from the node it replaces and lent to this one
    /// at the instance's next pump, with no deactivation.
    ///
    /// Call it after the pump has read the plugin's new ports (the changes it
    /// returned said `io`): the node declares the layout the lifecycle holds.
    /// Until the Processor arrives the node renders silence, counted by its own
    /// `unlent_blocks`. It starts with a fresh meter and counters; its
    /// parameter slots are the plugin's, and its pipeline figure is the
    /// replaced node's until a `prepare` that retunes the Processor reads it.
    ///
    /// Drop the node it replaces once [`settled`](Self::settled) says
    /// [`Moved`](Settlement::Moved), or earlier: a node dropped while it holds
    /// the Processor hands it back to the control side if a recall is raised,
    /// which this call raises.
    ///
    /// A call before the previous node's Processor has moved replaces the
    /// earlier successor: the node it built never receives a Processor.
    ///
    /// # Errors
    ///
    /// [`NoPorts`](RebindError::NoPorts) for a lost plugin;
    /// [`OutsideSlab`](RebindError::OutsideSlab) when a [`PortLimit`] is set and
    /// the plugin's channels differ from it;
    /// [`NotRunning`](RebindError::NotRunning) for a plugin that is loaded or
    /// refused; [`Restarting`](RebindError::Restarting) while its own restart is
    /// under way. The plugin is left as it was in each, and no node is built.
    pub fn rebind(&self, main: MainThreadMarker) -> Result<PluginNode<F>, RebindError> {
        let mut instance = self.inner.lock();
        let (layout, latency, tail) = instance.reading().ok_or(RebindError::NoPorts)?;
        self.fits(layout)?;
        let declared = Declared::from_layout(layout, latency, tail);
        let rt = instance.rehand(main)?;
        drop(instance);
        let pipeline = self
            .controls
            .as_ref()
            .map_or(self.pipeline, PluginControls::pipeline);
        Ok(InProcClient::new(
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
            Arc::default(),
            self.parameters.iter().copied(),
            self.spares,
            self.controls.clone(),
            Some(pipeline),
        )
        .bind(self.tempo.clone()))
    }

    /// The ports the plugin has now, as its lifecycle last read them: what a
    /// node [`rebind`](Self::rebind) builds now would declare. `None` for a
    /// lost plugin or before a layout was read.
    #[must_use]
    pub fn layout(&self) -> Option<tutti_plugin_types::Layout> {
        self.inner
            .lock()
            .reading()
            .map(|(layout, ..)| layout.clone())
    }

    /// What [`rebind`](Self::rebind) would refuse before it changes anything:
    /// no ports, or ports outside the [`PortLimit`]. Changes nothing.
    ///
    /// # Errors
    ///
    /// [`NoPorts`](RebindError::NoPorts) and [`OutsideSlab`](RebindError::OutsideSlab).
    pub fn check_ports(&self) -> Result<(), RebindError> {
        let instance = self.inner.lock();
        let (layout, ..) = instance.reading().ok_or(RebindError::NoPorts)?;
        self.fits(layout)
    }

    fn fits(&self, layout: &tutti_plugin_types::Layout) -> Result<(), RebindError> {
        if let Some(limit) = self.limit {
            let (now_inputs, now_outputs) = (layout.total_inputs(), layout.total_outputs());
            if (now_inputs, now_outputs) != (limit.inputs, limit.outputs) {
                return Err(RebindError::OutsideSlab {
                    inputs: limit.inputs,
                    outputs: limit.outputs,
                    now_inputs,
                    now_outputs,
                });
            }
        }
        Ok(())
    }

    /// Where the Processor's way is: see [`Settlement`].
    #[must_use]
    pub fn settled(&self) -> Settlement {
        self.inner.lock().settlement()
    }
}

/// Where the Processor is on its way to the newest node a [`Rebinder`] built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Settlement {
    /// The Processor has not been lent to the newest node. The node it
    /// replaces may still hold it and keeps rendering with it; it stays in
    /// the graph until the pump moves it. A plugin that is deactivated meanwhile
    /// leaves this until it is activated again.
    Pending,
    /// The Processor's way is the newest node's: the node it replaces holds
    /// none and may be dropped.
    Moved,
    /// The plugin died. No Processor will reach the newest node, and the
    /// node it replaces may still hold one whose plugin is gone: detach that
    /// node from the graph (the session's deferred drop) before it is dropped,
    /// and treat both nodes as silent.
    Lost,
}

impl<F: Loaded> PluginNode<F> {
    /// The [`Rebinder`] for this node, binding its successors to the tempo
    /// reader the node was bound to. See [`InProcClient::rebinder`].
    #[must_use]
    pub fn rebinder(&self, limit: Option<PortLimit>) -> Rebinder<F> {
        self.client.rebinder(self.tempo.clone(), limit)
    }
}

impl<F: Loaded> InProcClient<F> {
    /// The [`Rebinder`] for this node, over the same plugin, binding its
    /// successors to `tempo`. `limit` is the ports a rebuilt node may have:
    /// `None` for a plugin whose node has no fixed pipeline, `Some` for a
    /// served one.
    #[must_use]
    pub fn rebinder(&self, tempo: TempoReader, limit: Option<PortLimit>) -> Rebinder<F> {
        Rebinder {
            inner: Arc::clone(&self.inner),
            descriptor: self.descriptor.clone(),
            reload: self.reload.clone(),
            features: self.features,
            sample_rate: self.sample_rate,
            pending_sample_rate: Arc::clone(&self.pending_sample_rate),
            unlent: Arc::clone(&self.unlent),
            parameters: self.parameters.clone(),
            spares: self.spares,
            controls: self.controls.clone(),
            pipeline: self.pipeline,
            tempo,
            limit,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
    fn rig(
        limit: Option<PortLimit>,
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
        let wiring = loaded.wiring(main);
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
        );
        let rebinder = client.rebinder(tempo(), limit);
        let inner = Arc::clone(&client.inner);
        let node = contract::prepared(client.bind(tempo()), SR, BLOCK);
        (fake, rebinder, node, inner)
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
        drive_input(node, &env(frame), &[&x, &x], &[], &[])
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
        let (fake, rebinder, mut old, inner) = rig(None);
        assert_eq!(old.shape().audio_out().count(), 2);
        assert!(block(&mut old, 0).audio[0].iter().any(|&s| s != 0.0));

        change_ports(&fake, &mut old, &inner, 4);
        let restarts = (activations(&fake), deactivations(&fake));
        let mut new = rebinder.rebind(main_thread()).expect("a node");
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

    /// **A rebind over unchanged ports moves the Processor with no restart:**
    /// one activation, no deactivation, in the whole run.
    #[test]
    fn a_rebind_over_unchanged_ports_moves_the_processor() {
        let (fake, rebinder, mut old, inner) = rig(None);
        block(&mut old, 0);
        let mut new = rebinder.rebind(main_thread()).expect("a node");
        assert_eq!(new.shape().audio_out().count(), 2);
        let frame = settle(&rebinder, &mut old, &inner, BLOCK as u64);
        assert!(block(&mut new, frame).audio[0].iter().any(|&s| s != 0.0));
        assert_eq!(activations(&fake), 1, "the load's activation, no other");
        assert_eq!(deactivations(&fake), 0);
    }

    /// **A rebinder with a port limit refuses ports that differ from it, with
    /// a named error and without touching the plugin.** The Processor stays
    /// where it was, and the node in the graph still renders.
    #[test]
    fn a_port_limit_refuses_new_ports_by_name_and_leaves_the_plugin_alone() {
        let (fake, rebinder, mut old, inner) = rig(Some(PortLimit {
            inputs: 2,
            outputs: 2,
        }));
        change_ports(&fake, &mut old, &inner, 4);
        let refused = rebinder
            .rebind(main_thread())
            .expect_err("outside the slab");
        assert_eq!(
            refused,
            RebindError::OutsideSlab {
                inputs: 2,
                outputs: 2,
                now_inputs: 2,
                now_outputs: 4,
            }
        );
        assert_eq!(
            rebinder.settled(),
            Settlement::Moved,
            "nothing was rehanded"
        );
        let mut frame = 0;
        for _ in 0..4 {
            block(&mut old, frame);
            inner.lock().pump(main_thread());
            frame += BLOCK as u64;
        }
        assert!(
            block(&mut old, frame).audio[0].iter().any(|&s| s != 0.0),
            "the old node still holds the Processor"
        );
    }

    /// **Dropping the old node while it holds the Processor hands the
    /// Processor back instead of dropping it:** the rebind raised the recall,
    /// the drop returns the Processor through the handoff, and the pump lends
    /// it to the new node.
    #[test]
    fn an_old_node_dropped_with_the_processor_returns_it_to_the_new_one() {
        let (_fake, rebinder, mut old, inner) = rig(None);
        block(&mut old, 0);
        let mut new = rebinder.rebind(main_thread()).expect("a node");
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
        let (fake, rebinder, mut old, inner) = rig(None);
        block(&mut old, 0);
        fake.set_outputs(4);
        assert!(Request::IoChanged.raise(fake.request_word()));
        inner.lock().pump(main_thread());
        assert_eq!(
            rebinder.rebind(main_thread()).expect_err("parked"),
            RebindError::Restarting
        );
        assert_eq!(rebinder.settled(), Settlement::Moved, "nothing rehanded");
        let mut frame = 0;
        while rebinder.rebind(main_thread()).is_err() {
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
        let (fake, rebinder, mut old, inner) = rig(None);
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
            rebinder.rebind(main).expect_err("refused"),
            RebindError::NotRunning
        );
        assert_eq!(rebinder.settled(), Settlement::Moved);
    }

    /// **A plugin that dies after a rebind settles as `Lost`, not `Moved`:**
    /// the caller detaches the old node before dropping it.
    #[test]
    fn a_plugin_that_dies_after_a_rebind_is_lost_not_moved() {
        let (fake, rebinder, mut old, inner) = rig(None);
        block(&mut old, 0);
        let _new = rebinder.rebind(main_thread()).expect("a node");
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
}
