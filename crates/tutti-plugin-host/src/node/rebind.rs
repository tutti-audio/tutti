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
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RebindError {
    /// The plugin is lost, or has reported no ports.
    #[error("the plugin has no ports to rebuild a node with: it is lost or has reported none")]
    NoPorts,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// A node over the plugin with the ports it declares now, and the
    /// restart that brings its Processor to the node. Call it after the pump
    /// has read the plugin's new ports (the changes it returned said `io`):
    /// the node declares the layout the lifecycle holds when it is called.
    ///
    /// The plugin is restarted at the activation it has
    /// (`Hand::rehand`); the node's realtime end holds no Processor
    /// until the node that holds the Processor now has handed it back and the
    /// plugin's tick has reactivated it, so the node renders silence (counted
    /// by `unlent_blocks`) until then. The node starts with a fresh meter and
    /// delivery counters; its parameter slots are the plugin's.
    ///
    /// The node it replaces must stay in the graph until
    /// [`settled`](Self::settled): dropping it earlier drops the Processor with
    /// it and leaves the plugin unusable.
    ///
    /// A call before the previous node's Processor has moved replaces the
    /// earlier successor: the node it built never receives a Processor.
    ///
    /// # Errors
    ///
    /// [`NoPorts`](RebindError::NoPorts) for a lost plugin;
    /// [`OutsideSlab`](RebindError::OutsideSlab) when a [`PortLimit`] is set and
    /// the plugin's channels differ from it. The instance is not rehanded then.
    ///
    /// A plugin that refuses the restart is not an error here: the node is
    /// built and renders nothing until the plugin takes a restart, and the
    /// refusal reaches the control side as a rate's refusal does.
    pub fn rebind(&self, main: MainThreadMarker) -> Result<PluginNode<F>, RebindError> {
        let mut instance = self.inner.lock();
        let (layout, latency, tail) = instance.reading().ok_or(RebindError::NoPorts)?;
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
        let declared = Declared::from_layout(layout, latency, tail);
        let rt = instance.rehand(main);
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
            Arc::clone(&self.unlent),
            self.parameters.iter().copied(),
            self.spares,
            self.controls.clone(),
            Some(pipeline),
        )
        .bind(self.tempo.clone()))
    }

    /// Whether the Processor's way is to the newest node
    /// [`rebind`](Self::rebind) built: the node it replaces holds none, and
    /// may leave the graph.
    #[must_use]
    pub fn settled(&self) -> bool {
        self.inner.lock().handed_over()
    }
}

impl<F: Loaded> InProcClient<F> {
    /// The [`Rebinder`] for this node, over the same plugin, binding its
    /// successors to `tempo`. `limit` is the ports a rebuilt node may have:
    /// `None` for a plugin whose node has no fixed pipeline, `Some` for a
    /// served one.
    ///
    /// `parameters` and `spares` are what the node's delivery was made from.
    #[must_use]
    pub fn rebinder(
        &self,
        tempo: TempoReader,
        parameters: Vec<ParamId>,
        spares: usize,
        limit: Option<PortLimit>,
    ) -> Rebinder<F> {
        Rebinder {
            inner: Arc::clone(&self.inner),
            descriptor: self.descriptor.clone(),
            reload: self.reload.clone(),
            features: self.features,
            sample_rate: self.sample_rate,
            pending_sample_rate: Arc::clone(&self.pending_sample_rate),
            unlent: Arc::clone(&self.unlent),
            parameters,
            spares,
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
    use tutti_plugin_types::fake::{FakeLoaded, FakePlugin};
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
        let rebinder = client.rebinder(tempo(), Vec::new(), 0, limit);
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

    /// **A rebuilt node declares the ports the plugin has now, and the
    /// restarted Processor lands in it, not in the node it replaces.** The old
    /// node declares two outputs and renders silence once the Processor has
    /// left it; the new one declares four and renders the plugin's sound.
    #[test]
    fn the_processor_lands_in_the_rebuilt_node_and_the_old_one_is_left_empty() {
        let (fake, rebinder, mut old, inner) = rig(None);
        assert_eq!(old.shape().audio_out().count(), 2);
        assert!(block(&mut old, 0).audio[0].iter().any(|&s| s != 0.0));

        change_ports(&fake, &mut old, &inner, 4);
        let mut new = rebinder.rebind(main_thread()).expect("a node");
        assert_eq!(new.shape().audio_out().count(), 4, "the plugin's ports now");
        assert_eq!(
            old.shape().audio_out().count(),
            2,
            "the old node is as it was"
        );

        // The old node hands the Processor back at a block, the pump
        // restarts and lends it to the new node's end.
        let mut frame = BLOCK as u64;
        while !rebinder.settled() {
            block(&mut old, frame);
            inner.lock().pump(main_thread());
            frame += BLOCK as u64;
            assert!(frame < 16 * BLOCK as u64, "the Processor never moved");
        }
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

    /// **A rebinder with a port limit refuses ports that differ from it, with
    /// a named error and without touching the plugin.** The Processor stays
    /// where it was (`settled`), and the node in the graph still renders.
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
        assert!(rebinder.settled(), "nothing was rehanded");
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

    /// **The ports a rebuilt node declares are the plugin's own, bus by bus,
    /// when they did not change:** a rebind over unchanged ports restarts the
    /// plugin once and hands the Processor over.
    #[test]
    fn a_rebind_over_unchanged_ports_moves_the_processor() {
        let (_fake, rebinder, mut old, inner) = rig(None);
        let mut new = rebinder.rebind(main_thread()).expect("a node");
        assert_eq!(new.shape().audio_out().count(), 2);
        let mut frame = 0;
        while !rebinder.settled() {
            block(&mut old, frame);
            inner.lock().pump(main_thread());
            frame += BLOCK as u64;
            assert!(frame < 16 * BLOCK as u64);
        }
        assert!(block(&mut new, frame).audio[0].iter().any(|&s| s != 0.0));
    }
}
