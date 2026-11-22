//! An export's fork of the live graph, in the two halves it is made in and
//! the third it is edited in.
//!
//! The mechanism lives here and not in `tutti-export`, because it needs the
//! live graph and the hosted plugins: `tutti-export` renders a
//! [`RenderGraph`], which it can only be given
//! sealed (`RenderGraph::assemble`), so a graph that is half edited, or whose
//! editor feeds some other executor, is not one a render can be handed.
//!
//! 1. [`PendingExport`]: gathered on the main thread (what the fork reads of
//!    the live graph and its nodes), `Send`, holding nothing of the live graph.
//! 2. [`ForkedPair`]: built on any thread (a hosted plugin's `plugin-server`
//!    launch is the slow part), installed, and open to the edit hook.
//! 3. [`ForkedPair::assemble`]: sealed into the graph the render takes.

use std::sync::Arc;

use tutti_buffers::ChannelTopology;
use tutti_buffers::Float;
use tutti_export::{RenderError, RenderGraph, RtLogSink, Setup};
use tutti_graph::{
    CommitError, Editor, Executor, ForkError, ForkRemains, ForkTarget, PendingFork, Pool,
};
use tutti_units::SampleRate;

/// A fork's refusal as the export reports it: [`RenderError::NotForkable`] for
/// the one a host acts on, [`RenderError::Fork`] for the rest.
fn fork_error(e: ForkError) -> RenderError {
    match e {
        ForkError::NotForkable { key } => RenderError::NotForkable { key },
        other => RenderError::Fork(other),
    }
}

/// An export's fork, gathered on the main thread
/// ([`Graph::gather_for_export`](crate::Graph)) and not yet built: `Send`,
/// holding nothing of the live graph.
pub(crate) struct PendingExport<F: Float> {
    fork: PendingFork<F>,
    pool: Option<Arc<dyn Pool>>,
    /// The live main output's declared topology, for an export of the main output: the
    /// file is placed from it, as the device is.
    main_output: Option<ChannelTopology>,
    /// Where the fork's audio-thread log goes, named as the live graph names
    /// its nodes.
    rt_log: Arc<dyn RtLogSink>,
}

impl<F: Float> PendingExport<F> {
    /// `Editor::fork_gather` on `live` at the render's `Prepare`.
    ///
    /// # Errors
    ///
    /// What the gather refuses; a node that cannot be forked is
    /// [`RenderError::NotForkable`], checked before anything is forked.
    ///
    /// # Threading
    ///
    /// The gather is where an in-process VST2 plugin's fork is built and
    /// prepared (it is pinned, `ForkBuild::pinned`): call it on the **main
    /// thread**, and drop what it returns there.
    pub(crate) fn gather(
        live: &Editor<F>,
        target: ForkTarget,
        rate: SampleRate,
        pool: Option<Arc<dyn Pool>>,
        main_output: Option<ChannelTopology>,
    ) -> Result<Self, RenderError> {
        let fork = live
            .fork_gather(target, RenderGraph::prepare(rate))
            .map_err(fork_error)?;
        let rt_log = Arc::new(crate::rt_log::Emit::of(live));
        Ok(Self {
            fork,
            pool,
            main_output,
            rt_log,
        })
    }

    /// Build the fork (on a worker: a hosted plugin's `plugin-server` launch
    /// is here) into the pair the edit hook and the render work on.
    ///
    /// # Errors
    ///
    /// A deferred fork source's failure ([`RenderError::Fork`] with
    /// `ForkError::Source`, naming its node) or a fork that does not commit,
    /// with what was built, to drop where a pinned unit belongs (the main
    /// thread).
    pub(crate) fn build(self) -> Result<ForkedPair<F>, (RenderError, ForkRemains<F>)> {
        let (editor, executor) = self.fork.build().map_err(|(e, r)| (fork_error(e), r))?;
        let mut setup = Setup::new().rt_log(self.rt_log);
        if let Some(pool) = self.pool {
            setup = setup.pool(pool);
        }
        if let Some(main_output) = self.main_output {
            setup = setup.main_output(main_output);
        }
        Ok(ForkedPair {
            editor,
            executor,
            setup,
        })
    }
}

/// A built fork, installed and not yet sealed: the editor is open to the
/// export's edit hook.
#[derive(Debug)]
pub(crate) struct ForkedPair<F: Float> {
    editor: Editor<F>,
    executor: Executor<F>,
    setup: Setup,
}

impl<F: Float> ForkedPair<F> {
    /// The fork's editor, for the edit hook.
    pub(crate) fn editor_mut(&mut self) -> &mut Editor<F> {
        &mut self.editor
    }

    /// Send what the editor holds, and install it on the executor now, so the
    /// render's first block already runs it.
    ///
    /// # Errors
    ///
    /// The editor's [`CommitError`] if the edit does not compile (a cycle, a
    /// port out of range) or the editor is poisoned; nothing is installed.
    pub(crate) fn commit(&mut self) -> Result<(), CommitError> {
        let _rewired = self.editor.commit()?;
        self.executor.apply_pending();
        self.editor.collect();
        Ok(())
    }

    /// Whether the editor still feeds the executor: the edit hook holds an
    /// `&mut Editor`, which `std::mem::replace` can swap for another graph's.
    pub(crate) fn is_paired(&self) -> bool {
        self.editor.is_paired_with(&self.executor)
    }

    /// Seal the pair into the graph the render takes.
    ///
    /// # Errors
    ///
    /// What `RenderGraph::assemble` refuses; ask [`is_paired`](Self::is_paired)
    /// first when the pair must be dropped where it was made.
    pub(crate) fn assemble(self) -> tutti_export::Result<RenderGraph<F>> {
        RenderGraph::assemble(self.editor, self.executor, self.setup)
    }
}
