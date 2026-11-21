//! [`Rebinder`]: a new node over the plugin a node already drives, with the
//! ports the plugin now declares.
//!
//! A plugin that changes its audio ports (`IoChanged`) cannot be hosted out of process by a
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

use std::marker::PhantomData;
use std::sync::Arc;
use tutti_buffers::Float;

use parking_lot::Mutex;
use tutti_plugin_types::{Features, Loaded, MainThreadMarker, PluginDescriptor, Successor};
use tutti_rt::PendingSampleRate;
use tutti_timeline::TempoReader;
use tutti_units::ParamId;
use tutti_units::Samples;

use super::client::{Declared, InProcessClient, PluginNode, Reload};
use super::control_backend::Reporting;
use super::instance::Instance;
use crate::{DeliveryCounters, PluginControls};

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
/// Made from the node it replaces ([`InProcessClient::rebinder`]) and kept on the
/// control side. [`prepare`](Self::prepare) takes the instance lock, so never the
/// audio thread.
///
/// `F` is the graph float of the node it replaces. The default `F = f32` is the
/// common width and converts nothing; it is staging until every caller names
/// its graph's float.
pub struct Rebinder<L: Loaded, F: Float = f32> {
    inner: Arc<Mutex<Instance<L>>>,
    descriptor: PluginDescriptor,
    reload: Option<Reload>,
    features: Features,
    sample_rate: tutti_units::SampleRate,
    pending_sample_rate: Arc<PendingSampleRate>,
    /// Where a tick delivers what its pump finds: the handle's listeners.
    /// `None` for an out-of-process plugin, whose own listener reports it.
    reporting: Option<Reporting>,
    /// The host's writes' reader, which every successor node claims too.
    edits: Option<super::host_edits::EditPort>,
    /// The delivery counters of the newest node a commit gave the Processor
    /// (the one the rebinder was made from until then), shared by every clone:
    /// what a successor's parameter delivery is seeded from.
    delivery: Arc<Mutex<Arc<DeliveryCounters>>>,
    parameters: Vec<ParamId>,
    spares: usize,
    controls: Option<PluginControls>,
    /// The pipeline the node had when the rebinder was made, for a node over
    /// a plugin whose `controls` hold none.
    pipeline: Samples,
    /// The Processor's event room the node last saw: the room of a successor
    /// until its own Processor reaches it.
    event_room: std::num::NonZeroU32,
    /// The node declares a bypass of the plugin's own.
    own_bypass: bool,
    tempo: TempoReader,
    width: PhantomData<fn() -> F>,
}

impl<L: Loaded, F: Float> Clone for Rebinder<L, F> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            descriptor: self.descriptor.clone(),
            reload: self.reload.clone(),
            features: self.features,
            sample_rate: self.sample_rate,
            pending_sample_rate: Arc::clone(&self.pending_sample_rate),
            reporting: self.reporting.clone(),
            edits: self.edits.clone(),
            delivery: Arc::clone(&self.delivery),
            parameters: self.parameters.clone(),
            spares: self.spares,
            controls: self.controls.clone(),
            pipeline: self.pipeline,
            event_room: self.event_room,
            own_bypass: self.own_bypass,
            tempo: self.tempo.clone(),
            width: PhantomData,
        }
    }
}

impl<L: Loaded, F: Float> std::fmt::Debug for Rebinder<L, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rebinder")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl<L: Loaded + 'static, F: Float> Rebinder<L, F>
where
    L::Processor: 'static,
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
    /// The node starts with a fresh meter and counters, its silent blocks
    /// counted in its own [`NodeControls::block_losses`](crate::NodeControls::block_losses).
    /// Its parameter slots are the replaced node's, and its delivery starts from
    /// what the newest committed node had delivered, so a value the host holds
    /// still is not sent again over an edit made in the plugin's editor; what
    /// the replaced node delivers after this call is not carried. Its pipeline
    /// figure is the replaced node's until a `prepare` that retunes the
    /// Processor reads it.
    ///
    /// # Errors
    ///
    /// [`NoPorts`](RebindError::NoPorts) for a lost plugin;
    /// [`TooWide`](RebindError::TooWide) for ports wider than a block serves;
    /// [`NotRunning`](RebindError::NotRunning) for a plugin that is loaded or
    /// refused; [`Restarting`](RebindError::Restarting) while its own restart is
    /// under way.
    pub fn prepare(&self) -> Result<(PluginNode<L, F>, Pending<L>), RebindError> {
        let instance = self.inner.lock();
        let layout = instance.layout().ok_or(RebindError::NoPorts)?;
        let mut declared = Declared::from_layout(layout)?;
        declared.ports.own_bypass = self.own_bypass;
        instance.ready_to_rehand()?;
        drop(instance);
        let (successor, rt) = Successor::new();
        let pipeline = self
            .controls
            .as_ref()
            .map_or(self.pipeline, PluginControls::pipeline);
        // Its own counters: the node it replaces keeps counting into the old
        // ones, which the session banks when it takes the successor's controls.
        let counters = Arc::<DeliveryCounters>::default();
        let mut client = InProcessClient::new(
            Arc::clone(&self.inner),
            rt,
            self.descriptor.clone(),
            self.reload.clone(),
            declared,
            self.features,
            self.sample_rate,
            Arc::clone(&self.pending_sample_rate),
            Some(Arc::clone(&counters)),
            self.parameters.iter().copied(),
            self.spares,
            self.controls.clone(),
            Some(pipeline),
            Some(self.event_room),
        );
        client.reporting = self.reporting.clone();
        client.edits = self.edits.clone();
        // What the node it replaces delivered: the plugin holds those values.
        let delivered = self.delivery.lock().delivered();
        client.seed_delivery(&delivered);
        // A served plugin's controls own the counters its nodes share.
        let counters = self
            .controls
            .as_ref()
            .map_or(counters, PluginControls::delivery_counters);
        Ok((
            client.bind(self.tempo.clone()),
            Pending {
                inner: Arc::clone(&self.inner),
                successor,
                delivery: Arc::clone(&self.delivery),
                counters,
            },
        ))
    }

    /// The ports the plugin has now, as its lifecycle last read them: what a
    /// node [`prepare`](Self::prepare) built now would declare. `None` for a
    /// lost plugin or before a layout was read.
    #[must_use]
    pub fn layout(&self) -> Option<tutti_plugin_types::Layout> {
        self.inner.lock().layout().cloned()
    }

    /// Runs the plugin's lifecycle once: takes what the plugin asked for (a
    /// restart for new buses among it) and carries it out. What a handle's
    /// `editor_idle` does for an in-process plugin; an out-of-process plugin's handle
    /// has no instance to tick, so its rebinder does. What it finds (a
    /// latency or tail that moved, a request of the transport, a refused rate,
    /// the plugin's death) is delivered to the handle's listeners as the
    /// handle's own `editor_idle` delivers it, so what the plugin raised while
    /// that ran is not lost to this pump; an out-of-process plugin's own listener
    /// reports its changes already. Main thread.
    pub fn tick(&self, main: MainThreadMarker) {
        let changes = self.inner.lock().pump(main);
        if let Some(reporting) = &self.reporting {
            reporting.deliver(changes);
        }
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
pub struct Pending<L: Loaded> {
    inner: Arc<Mutex<Instance<L>>>,
    successor: Successor<L::Processor>,
    /// The rebinder's record of the newest committed node's counters.
    delivery: Arc<Mutex<Arc<DeliveryCounters>>>,
    /// The node's own, which `delivery` holds once it is committed.
    counters: Arc<DeliveryCounters>,
}

impl<L: Loaded> std::fmt::Debug for Pending<L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending").finish_non_exhaustive()
    }
}

impl<L: Loaded> Pending<L> {
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
        *self.delivery.lock() = self.counters;
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

impl<L: Loaded, F: Float> PluginNode<L, F> {
    /// The [`Rebinder`] for this node, binding its successors to the tempo
    /// reader the node was bound to. See [`InProcessClient::rebinder`].
    #[must_use]
    pub fn rebinder(&self) -> Rebinder<L, F> {
        self.client.rebinder(self.tempo.clone())
    }
}

impl<L: Loaded, F: Float> InProcessClient<L, F> {
    /// The [`Rebinder`] for this node, over the same plugin, binding its
    /// successors to `tempo`.
    #[must_use]
    pub fn rebinder(&self, tempo: TempoReader) -> Rebinder<L, F> {
        Rebinder {
            inner: Arc::clone(&self.inner),
            descriptor: self.descriptor.clone(),
            reload: self.reload.clone(),
            features: self.features,
            sample_rate: self.sample_rate,
            pending_sample_rate: Arc::clone(&self.pending_sample_rate),
            reporting: self.reporting.clone(),
            edits: self.edits.clone(),
            delivery: Arc::new(Mutex::new(self.params.counters())),
            parameters: self.parameters.clone(),
            spares: self.spares,
            controls: self.controls.clone(),
            pipeline: self.pipeline,
            event_room: self.event_room,
            own_bypass: self.declared.ports.own_bypass,
            tempo,
            width: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tutti_buffers::Planes;

    use tutti_buffers::MaxBlock;
    use tutti_fixtures::main_thread;
    use tutti_graph::contract::{self, Driven, drive_input_params};
    use tutti_graph::{
        Env, Node, Offset, ParamEvent, ParamRamp, PlayClock, TransportChanges, TransportPosition,
    };
    use tutti_plugin_types::fake::{Call, FakeLoaded, FakePlugin};
    use tutti_plugin_types::{Control, Layout, Loaded, PluginInvalidation, Prepare, Request};
    use tutti_tempo::TempoMap;
    use tutti_units::{Bpm, Frame, ParamId, SampleRate, Samples};

    use super::*;
    use crate::client_over;

    const SR: SampleRate = SampleRate::SR_48K;
    const BLOCK: usize = 64;

    fn tempo() -> TempoReader {
        TempoReader::fixed(Arc::new(
            TempoMap::constant(Bpm::of(120.0)).expect("a tempo"),
        ))
    }

    /// A fake plugin hosted as an out-of-process one is, with a rebinder over it, and
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
        rig_over(own_bypass, [])
    }

    /// [`rig`], the node addressing `parameters`.
    fn rig_params(
        parameters: impl IntoIterator<Item = ParamId>,
    ) -> (
        FakePlugin,
        Rebinder<FakeLoaded>,
        PluginNode<FakeLoaded>,
        Arc<Mutex<Instance<FakeLoaded>>>,
    ) {
        rig_over(false, parameters)
    }

    fn rig_over(
        own_bypass: bool,
        parameters: impl IntoIterator<Item = ParamId>,
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
                tutti_units::ParamId::new(4),
            ));
        }
        let descriptor = loaded.info().clone();
        let prepare = Prepare::new(SR, MaxBlock::new(Samples::new(BLOCK)).expect("a block"));
        let Ok((processor, controller)) = loaded.activate(main, prepare) else {
            panic!("the fake activates");
        };
        let client = client_over::<FakeLoaded, f32>(
            processor, controller, prepare, descriptor, &wiring, parameters, 0, None,
        )
        .expect("the fake is narrow");
        let inner = Arc::clone(&client.inner);
        let rebinder = client.rebinder(tempo());
        let node = contract::prepared(client.bind(tempo()), SR, BLOCK);
        (fake, rebinder, node, inner)
    }

    /// A node over the fake with the host's parameter slots attached, driven by
    /// hand; the post that feeds them.
    fn edited_node() -> (
        FakePlugin,
        super::super::host_edits::EditPost,
        tutti_graph::contract::Direct<PluginNode<FakeLoaded>>,
        Arc<Mutex<Instance<FakeLoaded>>>,
    ) {
        let (fake, loaded) = FakePlugin::new();
        fake.declare(&[(ParamId::new(1), false)]);
        let main = main_thread();
        let wiring = loaded.wiring(main);
        let descriptor = loaded.info().clone();
        let prepare = Prepare::new(SR, MaxBlock::new(Samples::new(BLOCK)).expect("a block"));
        let Ok((processor, controller)) = loaded.activate(main, prepare) else {
            panic!("the fake activates");
        };
        let mut client = client_over::<FakeLoaded, f32>(
            processor,
            controller,
            prepare,
            descriptor,
            &wiring,
            [ParamId::new(1)],
            0,
            None,
        )
        .expect("the fake is narrow");
        let (post, port) = super::super::host_edits::host_edits([ParamId::new(1)]);
        client.edits = Some(port);
        let inner = Arc::clone(&client.inner);
        let unit = tutti_graph::contract::Direct::new(client.bind(tempo()), SR, BLOCK);
        (fake, post, unit, inner)
    }

    fn block_params(fake: &FakePlugin) -> Vec<(ParamId, f64)> {
        fake.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::BlockParam(id, value) => Some((id, value.get())),
                _ => None,
            })
            .collect()
    }

    /// **A write owed after a block the plugin failed is dropped when a load
    /// comes before the next block**: the write was drained into the failed
    /// block's chunk, which did not go and so owes it to the next; a state load
    /// since made it stale, and the next block, whole-block path, does not send it.
    #[test]
    fn a_write_owed_after_a_failed_block_is_not_sent_after_a_load() {
        let (fake, post, mut unit, _inner) = edited_node();
        fake.fail_blocks(true);
        post.write(ParamId::new(1), tutti_units::Normalized::of(0.75))
            .expect("a declared parameter");
        unit.block();
        assert!(
            block_params(&fake).is_empty(),
            "the failed block took nothing"
        );
        post.reset();
        fake.fail_blocks(false);
        unit.block();
        assert!(
            block_params(&fake).is_empty(),
            "the stale owed write reached the plugin: {:?}",
            block_params(&fake)
        );
    }

    /// **A write made after a flush is the plugin's last word**, through a node:
    /// the first write is applied by a flush (the plugin's own `Control`), the
    /// second rides the next block, and nothing the plugin is told after the
    /// block undoes it.
    #[test]
    fn a_write_after_a_flush_ends_the_plugin_on_the_later_write() {
        let (fake, post, mut unit, inner) = edited_node();
        let id = ParamId::new(1);
        post.write(id, tutti_units::Normalized::of(0.25))
            .expect("a declared parameter");
        post.flush(main_thread(), inner.lock().control().expect("alive"));
        post.write(id, tutti_units::Normalized::of(0.75))
            .expect("a declared parameter");
        unit.block();
        let told: Vec<Call> = fake
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::SetParameter(_) | Call::BlockParam(..)))
            .collect();
        assert_eq!(
            told,
            [
                Call::SetParameter(id),
                Call::BlockParam(id, tutti_units::Normalized::of(0.75)),
            ],
            "the flushed write first, the block's later write last"
        );
    }

    /// **A write owed after a failed block, with no load, is sent by the next**
    /// (the positive case of the test above).
    #[test]
    fn a_write_owed_after_a_failed_block_is_sent_by_the_next() {
        let (fake, post, mut unit, _inner) = edited_node();
        fake.fail_blocks(true);
        post.write(ParamId::new(1), tutti_units::Normalized::of(0.75))
            .expect("a declared parameter");
        unit.block();
        fake.fail_blocks(false);
        unit.block();
        assert_eq!(block_params(&fake), [(ParamId::new(1), 0.75)]);
    }

    /// **The node declares what its own MIDI intake takes**
    /// (`MIDI_STACK_CAPACITY`, 256 events a block), at every block: 64
    /// frames, 2048 and 1 << 20 all declare 256.
    #[test]
    fn the_node_declares_no_more_than_its_midi_intake_takes() {
        let (_fake, _rebinder, node, _inner) = rig();
        let bound = |node: &PluginNode<FakeLoaded>| node.shape().event_input_capacity;
        assert_eq!(bound(&node).map(|n| n.get()), Some(256));
        let node = contract::prepared(node, SR, 2048);
        assert_eq!(bound(&node).map(|n| n.get()), Some(256));
        let node = contract::prepared(node, SR, 1 << 20);
        assert_eq!(bound(&node).map(|n| n.get()), Some(256));
    }

    /// **The node never declares more than its Processor's input storage
    /// takes**, whatever the graph's block: a Processor holding 100 events a
    /// block bounds the node at 100 at 64 and 2048 frames; one holding more
    /// than the intake leaves the intake's 256.
    #[test]
    fn the_node_declares_no_more_than_its_processor_takes() {
        let (fake, _rebinder, node, _inner) = rig();
        fake.hold_events(100);
        let bound = |node: &PluginNode<FakeLoaded>| node.shape().event_input_capacity;
        let node = contract::prepared(node, SR, 2048);
        assert_eq!(bound(&node).map(|n| n.get()), Some(100));
        let node = contract::prepared(node, SR, 64);
        assert_eq!(bound(&node).map(|n| n.get()), Some(100));
        fake.hold_events(4096);
        let node = contract::prepared(node, SR, 2048);
        assert_eq!(bound(&node).map(|n| n.get()), Some(256));
    }

    /// **A node with no Processor to read declares the room its predecessor
    /// last saw**, not the intake's alone: a successor built (a restart's) with
    /// its Processor not yet lent declares 100 where the intake gives 256.
    #[test]
    fn a_node_without_its_processor_declares_the_last_room_it_saw() {
        let (fake, _rebinder, node, _inner) = rig();
        fake.hold_events(100);
        let node = contract::prepared(node, SR, 2048);
        let (mut successor, _pending) = node
            .client
            .rebinder(tempo())
            .prepare()
            .expect("a successor");
        assert!(
            successor.client.rt_unlent(),
            "the successor holds no Processor yet"
        );
        let successor = contract::prepared(successor, SR, 2048);
        assert_eq!(
            successor.shape().event_input_capacity.map(|n| n.get()),
            Some(100)
        );
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
            block_len: Samples::new(BLOCK),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        }
    }

    /// One block with an impulse on both inputs: a sounding plugin answers it.
    fn block(node: &mut PluginNode<FakeLoaded>, frame: u64) -> Driven {
        block_with(node, frame, &[])
    }

    /// [`block`], with `params` on the node's event input.
    fn block_with(node: &mut PluginNode<FakeLoaded>, frame: u64, params: &[ParamEvent]) -> Driven {
        let mut x = vec![0.0f32; BLOCK];
        x[0] = 1.0;
        drive_input_params(
            node,
            &env(frame),
            Planes::new(&[&x, &x]).unwrap(),
            &[],
            &[],
            &[params],
        )
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
        assert!(Request::IoChanged.raise(fake.request_flags()));
        let main = main_thread();
        // The pump asks for the Processor back, the old node returns it at its
        // block, and a later pump restarts the plugin and reads its layout.
        let read =
            |inner: &Mutex<Instance<FakeLoaded>>| inner.lock().layout().map(Layout::total_outputs);
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
        assert!(Request::IoChanged.raise(fake.request_flags()));
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
        assert!(Request::LatencyChanged.raise(fake.request_flags()));
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
        assert!(Request::LatencyChanged.raise(fake.request_flags()));
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

    const PARAM: ParamId = ParamId::new(5);

    /// A ramp of `PARAM` to `value` at the block's first frame.
    fn ramp(value: f64) -> [ParamEvent; 1] {
        [ParamEvent::ramp(
            Offset::of(Samples::new(0), Samples::new(BLOCK)),
            ParamRamp::by_id(PARAM, value, Samples::new(0)),
        )]
    }

    /// **A rebuilt node does not send again a value the node it replaced
    /// delivered**, across every rebuild: the plugin holds the value (and what
    /// its editor changed since), so a host value held still is not written over
    /// it. A value that moved is sent.
    #[test]
    fn a_rebuilt_node_does_not_resend_the_value_the_replaced_node_delivered() {
        let (_fake, rebinder, mut first, inner) = rig_params([PARAM]);
        let sent =
            |node: &PluginNode<FakeLoaded>| node.client.params.counters().snapshot().points_sent;
        block_with(&mut first, 0, &ramp(0.5));
        assert_eq!(sent(&first), 1, "the first node delivers it");

        let mut frame = BLOCK as u64;
        let mut old = first;
        let mut held = 0.5;
        for round in 1..=2 {
            let (mut new, pending) = rebinder.prepare().expect("a node");
            pending.commit(main_thread());
            frame = settle(&rebinder, &mut old, &inner, frame);
            block_with(&mut new, frame, &ramp(held));
            assert_eq!(sent(&new), 0, "round {round}: the plugin holds {held}");
            held += 0.25;
            block_with(&mut new, frame + BLOCK as u64, &ramp(held));
            assert_eq!(sent(&new), 1, "round {round}: {held} is a move");
            // The next rebuild starts from what this node delivered last.
            frame += 2 * BLOCK as u64;
            old = new;
        }
    }

    /// **A tick delivers what its pump finds** to the handle's listeners, as the
    /// handle's own `editor_idle` does: the tail the plugin reported reaches the
    /// invalidation sink, so what the plugin raised while the first pump of an
    /// update ran is not lost to the second.
    #[test]
    fn a_tick_delivers_the_tail_its_pump_found() {
        let (fake, mut rebinder, _node, _inner) = rig();
        let heard: Arc<Mutex<Vec<PluginInvalidation>>> = Arc::default();
        let reporting = Reporting {
            invalidate: crate::InvalidateSink::new(),
            transport: crate::TransportSink::new(),
        };
        let sink = Arc::clone(&heard);
        let _hearing = reporting.invalidate.subscribe(move |e| sink.lock().push(e));
        rebinder.reporting = Some(reporting);

        fake.set_tail(Samples::new(77));
        assert!(Request::TailChanged.raise(fake.request_flags()));
        rebinder.tick(main_thread());
        let heard = heard.lock();
        assert!(
            heard.iter().any(|e| matches!(
                e,
                PluginInvalidation::Tail { tail }
                    if *tail == tutti_plugin_types::PluginTail::Finite(Samples::new(77))
            )),
            "{heard:?}"
        );
    }

    /// **A node's shape is the figures the control side last read, even while the
    /// control side holds the instance**: a graph compile that meets a plugin
    /// inside `editor_idle` or a fork reads the tail the plugin reports now,
    /// not the one it reported at load.
    #[test]
    fn the_shape_is_the_current_figures_while_the_instance_is_held() {
        let (fake, _rebinder, node, inner) = rig();
        let at_load = node.shape();
        fake.set_tail(Samples::new(77));
        assert!(Request::TailChanged.raise(fake.request_flags()));
        inner.lock().pump(main_thread());
        let free = node.shape();
        assert_ne!(free, at_load, "the tail moved");
        let _control_side = inner.lock();
        assert_eq!(node.shape(), free, "the same with the instance held");
    }
}
