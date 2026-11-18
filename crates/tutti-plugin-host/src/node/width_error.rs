//! [`WidthError`]: a plugin that does not process the width a session renders at.

use tutti_buffers::FloatWidth;
use tutti_plugin_types::Widths;

/// A plugin does not process audio at the width its graph renders at.
///
/// A plugin is never converted: an `f64` graph hosts a plugin that takes
/// 64-bit audio natively, and refuses one that does not. The error carries
/// the plugin, activated at 32-bit, so the caller decides: hand it to a
/// graph of its own width ([`into_inner`](Self::into_inner)), or wrap it in
/// `Bridged::of` to host it in this one. A 32-bit graph never gets this
/// error, because every format processes 32-bit audio.
///
/// `Debug` is written by hand so that `P` need not be `Debug`.
///
/// `P` is what was loaded: the pair [`load_client`](crate::load_client)
/// returns, a client and its handle.
#[derive(thiserror::Error)]
#[error("the plugin does not process {requested:?} audio (it processes {widths:?})")]
pub struct WidthError<P> {
    plugin: P,
    requested: FloatWidth,
    widths: Widths,
}

impl<P> WidthError<P> {
    /// An error for `plugin`, loaded at 32-bit when `requested` was asked of
    /// a plugin that processes `widths`.
    #[must_use]
    pub fn new(plugin: P, requested: FloatWidth, widths: Widths) -> Self {
        Self {
            plugin,
            requested,
            widths,
        }
    }

    /// The loaded plugin, activated at the width it processes, as
    /// `std::io::IntoInnerError::into_inner` returns the writer.
    #[must_use]
    pub fn into_inner(self) -> P {
        self.plugin
    }

    /// The width the graph renders at, which the plugin does not process.
    #[must_use]
    pub fn requested(&self) -> FloatWidth {
        self.requested
    }

    /// The widths the plugin processes.
    #[must_use]
    pub fn widths(&self) -> Widths {
        self.widths
    }
}

impl<P> std::fmt::Debug for WidthError<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WidthError")
            .field("requested", &self.requested)
            .field("widths", &self.widths)
            .finish_non_exhaustive()
    }
}
