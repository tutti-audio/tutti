//! What a test of the session reads that no host should: the audio side, and
//! the plan the graph last sent. Behind the off-by-default `testing` feature,
//! which this crate's own tests turn on through a dev-dependency on itself.

use std::sync::Arc;

use tutti_buffers::{Float, InterleavedMut, PlanesMut};
use tutti_units::Samples;

use tutti_graph::{
    Compensation, Editor, Executor, NodeKey, Prepare, Topology, TransportPosition, fade_gains,
};

use super::{Graph, render};

impl<F: Float> Graph<F> {
    /// What a test reads of the graph's plumbing: [`Testing`].
    pub fn testing(&self) -> Testing<'_, F> {
        Testing { graph: self }
    }

    /// The audio thread's half, for a test that renders what a device would
    /// hear: every commit lands on it, and ticking it plays the graph.
    ///
    /// # Panics
    ///
    /// If the audio side was already taken, by an earlier call or by the
    /// engine the session builder built over it.
    pub fn take_audio_side(&mut self) -> AudioSide<F> {
        AudioSide::live(self.take_executor())
    }
}

/// The graph's plumbing as a test reads it: what the session drives and no
/// host calls. Made by [`Graph::testing`].
#[derive(Debug, Clone, Copy)]
pub struct Testing<'g, F: Float = f32> {
    graph: &'g Graph<F>,
}

impl<F: Float> Testing<'_, F> {
    /// What every node is prepared for: the rate, the largest block, and the
    /// device's callback size when the session knows it.
    pub fn prepared(&self) -> Prepare {
        self.graph.prepared()
    }

    /// Whether a re-prepare is between its two commits.
    pub fn is_repreparing(&self) -> bool {
        self.graph.is_repreparing()
    }

    /// The plan last sent to the audio thread (`Editor::base`).
    pub fn sent_plan(&self) -> Option<Arc<tutti_graph::Plan>> {
        self.graph.editor.base().cloned()
    }

    /// How many commits have sent a plan.
    pub fn commits_sent(&self) -> u64 {
        self.graph.commits
    }

    /// The audio wiring as it stands: the nodes, the edges into every input
    /// port, the main output's output sources and its input width, including edits
    /// no commit has sent yet ([`Graph::has_uncommitted_edits`]). A read-only
    /// view of what [`Graph::connect`] and its kin write; the
    /// graph's other declarations (event edges, params, modes) are not part
    /// of it.
    ///
    /// An edge that is fading out stays here until its fade ends.
    pub fn topology(&self) -> &Topology {
        &self.graph.editor.spec().topology
    }

    /// What compensation this graph needs, folded over the graph as edited
    /// (committed or not): audio edges plus each node's event and
    /// param-modulation sources. Mutates nothing.
    pub fn latency_plan(&self) -> Compensation {
        // The bus edges as the lanes they commit to; a spec whose bus edges
        // do not lower has none to fold (the commit will say why).
        let rate = self.graph.editor.prepare().sample_rate();
        match self.graph.editor.lowered() {
            Ok(lowered) => tutti_graph::latency_plan(&lowered.spec, rate),
            Err(_) => tutti_graph::latency_plan(self.graph.editor.spec(), rate),
        }
    }

    /// The compensation of the plan sent last: what the session publishes,
    /// since it is what the executor is handed.
    pub fn sent_compensation(&self) -> Option<Compensation> {
        self.graph.sent_compensation()
    }

    /// Whether the graph keeps a reclaimer for `node`
    /// ([`SessionNode::reclaimer`](crate::SessionNode::reclaimer)): for a test that a path inserts a node
    /// through [`Graph::add`].
    pub fn keeps_reclaimer(&self, node: impl Into<NodeKey>) -> bool {
        let node = node.into();
        self.graph
            .nodes
            .get(&node)
            .is_some_and(|entry| entry.reclaimer.is_some())
    }

    /// Whether the node of the loaded plugin at `node` was re-inserted for new
    /// ports and its Processor has not reached it yet (its record is
    /// `Settling`): `None` for a node with no such record.
    #[cfg(feature = "plugin")]
    pub fn rebuild_settling(&self, node: impl Into<NodeKey>) -> Option<bool> {
        let node = node.into();
        self.graph
            .plugins
            .get(&node)?
            .rebind
            .as_ref()
            .map(crate::plugin::RebindState::is_settling)
    }

    /// How many replacements wait for a re-prepare to resume.
    pub fn waiting_replacements(&self) -> usize {
        self.graph.parked.len()
    }

    /// The compensation the next commit's plan carries: the spec compiled
    /// against the shapes, as the commit will compile it. Nothing is
    /// inserted — PDC is the compiler's. `None` when the spec does not
    /// compile (the commit will say why).
    pub fn planned_compensation(&self) -> Option<Compensation> {
        let valid = self.graph.editor.lowered().ok()?.spec.validate().ok()?;
        let (plan, _) = tutti_graph::compile(
            &valid,
            self.graph.editor.shapes(),
            self.graph.editor.prepare(),
            self.graph.editor.base().map(|p| &**p),
        )
        .ok()?;
        Some(super::compensation_of(&plan))
    }

    /// A live duplicate of the whole graph that shares no state with it
    /// (`Editor::fork`, prepared as the live graph is), as an audio side to render. Each
    /// node is forked from its own fork source: a `ParamNode` from the
    /// values its set's cells hold. What an export renders from, so a test
    /// can check that a control write reaches it.
    ///
    /// # Errors
    ///
    /// When a node cannot be forked.
    pub fn fork_live(&self) -> Result<AudioSide<F>, tutti_graph::ForkError> {
        let (editor, exec) = self.graph.editor.fork(
            tutti_graph::ForkTarget::MainOutput,
            *self.graph.editor.prepare(),
        )?;
        Ok(AudioSide::forked(editor, exec))
    }
}

/// The audio thread's half of a graph, for a test that renders what a device
/// would hear: every commit lands on it, and rendering it plays the graph.
/// Taken with [`Graph::take_audio_side`] or [`Testing::fork_live`].
pub struct AudioSide<F: Float = f32> {
    exec: Executor<F>,
    transport: TransportPosition,
    scratch: Vec<Vec<F>>,
    /// A fork's own editor, kept for as long as its executor runs: the
    /// executor sends its boxes back to it. `None` for the live graph's
    /// audio side, whose editor stays in the graph.
    _editor: Option<Editor<F>>,
}

impl<F: Float> std::fmt::Debug for AudioSide<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSide")
            .field("exec", &self.exec)
            .field("transport", &self.transport)
            .field("_editor", &self._editor)
            .finish_non_exhaustive()
    }
}

impl<F: Float> AudioSide<F> {
    pub(super) fn live(exec: Executor<F>) -> Self {
        Self {
            exec,
            transport: TransportPosition::default(),
            scratch: Vec::new(),
            _editor: None,
        }
    }

    /// A fork's pair, rendered as an audio side.
    pub(super) fn forked(editor: Editor<F>, exec: Executor<F>) -> Self {
        Self {
            exec,
            transport: TransportPosition::default(),
            scratch: Vec::new(),
            _editor: Some(editor),
        }
    }

    /// Renders one frame, with silence on every main input, into `output`:
    /// one value per main output. Commits sent since the last call land
    /// first.
    pub fn tick(&mut self, output: InterleavedMut<'_, F>) {
        render(&mut self.exec, &mut self.scratch, &self.transport, output);
    }

    /// The transport every block from here on renders under, as a device's
    /// engine hands its graph the playhead: a node that reads the transport
    /// from its `Env` (a placed sampler voice) plays on it. Stopped at beat
    /// zero until set.
    pub fn set_transport(&mut self, transport: TransportPosition) {
        self.transport = transport;
    }

    /// Renders `output`'s frames with no main input into it, planar, one
    /// plane per main output, in the blocks a device would hand over:
    /// executor blocks of up to `block` frames, each under the transport
    /// last [set](Self::set_transport) (stopped at beat zero until one is). A
    /// plane the graph has no output for stays silent.
    ///
    /// # Panics
    ///
    /// If `block` is zero, or past the graph's prepared maximum.
    pub fn render(&mut self, block: usize, mut output: PlanesMut<'_, '_, F>) {
        assert!(block > 0, "a zero-frame block");
        let frames = output.frames().get();
        for c in 0..output.channels() {
            output.plane_mut(c).samples_mut().fill(F::ZERO);
        }
        let exec = &mut self.exec;
        let mut done = 0;
        while done < frames {
            let len = (frames - done).min(block);
            exec.apply_pending();
            let (ins, outs, main_output) = exec.plan().map_or((0, 0, 0), |p| {
                (
                    p.main_inputs() as usize,
                    p.rendered_outputs(),
                    p.main_outputs(),
                )
            });
            let zeros = vec![F::ZERO; len];
            let inputs: Vec<&[F]> = (0..ins).map(|_| &zeros[..]).collect();
            let mut planes: Vec<Vec<F>> = vec![vec![F::ZERO; len]; outs];
            let mut slices: Vec<&mut [F]> = planes.iter_mut().map(|p| &mut p[..]).collect();
            let fade = exec.main_output_fade();
            if exec.plan().is_some() {
                exec.process(len, &self.transport, &inputs, &mut slices);
            }
            for c in 0..output.channels() {
                let Some(new) = planes.get(c).filter(|_| c < main_output) else {
                    continue;
                };
                let o = output.plane_mut(c).into_samples_mut();
                o[done..done + len].copy_from_slice(new);
                // A main output change crossfades by index, the old main output's channels
                // after the new main output's.
                if let Some(f) = fade {
                    for (i, x) in o[done..done + len].iter_mut().enumerate() {
                        let was = if c < f.width {
                            planes[main_output + c][i]
                        } else {
                            F::ZERO
                        };
                        let (go, gn) = if f.done.get() + i < f.fade.duration.get() {
                            fade_gains::<F>(f.fade.law, f.done + Samples::new(i), f.fade.duration)
                        } else {
                            (F::ZERO, F::ONE)
                        };
                        *x = was * go + *x * gn;
                    }
                }
            }
            done += len;
        }
    }
}
