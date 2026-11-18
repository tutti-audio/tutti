//! The rebinder of a plugin hosted in this process, in any in-process format.

use super::{BoundPlaced, BoundPlugin, Commit, PluginRebinder, Rebind, Rebuilt};

/// An in-process format's rebinder, and the variant of [`BoundPlaced`] its
/// nodes go in.
struct InProcessRebind<F: tutti_plugin_types::InProcess> {
    rebinder: tutti_plugin_host::Rebinder<F>,
    wrap: fn(Box<tutti_plugin_host::InProcNode<F>>) -> BoundPlaced,
}

impl<F> Rebind for InProcessRebind<F>
where
    F: tutti_plugin_types::InProcess + 'static,
    F::Processor: 'static,
{
    fn layout(&self) -> Option<tutti_plugin_types::Layout> {
        self.rebinder.layout()
    }

    fn settled(&self) -> tutti_plugin_host::Settlement {
        self.rebinder.settled()
    }

    fn prepare(&self) -> std::result::Result<Rebuilt, tutti_plugin_host::RebindError> {
        let (node, pending) = self.rebinder.prepare()?;
        let controls = node.controls();
        Ok(Rebuilt {
            bound: BoundPlugin {
                bound: (self.wrap)(Box::new(node)),
            },
            controls: Some(controls),
            commit: Commit(Box::new(move |main| pending.commit(main))),
        })
    }
}

impl PluginRebinder {
    /// The rebinder of an in-process plugin, whose rebuilt nodes go in `wrap`.
    pub(super) fn in_process<F>(
        rebinder: tutti_plugin_host::Rebinder<F>,
        wrap: fn(Box<tutti_plugin_host::InProcNode<F>>) -> BoundPlaced,
    ) -> Self
    where
        F: tutti_plugin_types::InProcess + 'static,
        F::Processor: 'static,
    {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(InProcessRebind { rebinder, wrap })),
        }
    }
}
