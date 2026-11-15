//! [`Loadable`]: a plugin a catalog resolved, the only thing a load takes; and
//! [`Resolved`]: what a saved [`PluginId`](tutti_plugin_types::PluginId) names
//! among the plugins the catalog holds now.
//!
//! A load starts from the catalog, so its blacklist cannot be skipped: a
//! [`Loadable`] carries the catalog's [`BlacklistView`], and the load asks it
//! where it opens the plugin. A plugin blacklisted after it was resolved, by a
//! scan that ran meanwhile, is refused there.
//!
//! The refusal is the client's: the check runs in the host process before a
//! server is started or a library is opened, and the `plugin-server` never sees
//! the blacklist.

use std::path::Path;
use tutti_plugin_types::{BridgeError, BridgeResult, PluginFormat};

use crate::host::catalog::{BlacklistView, Catalog};
use crate::host::plugin_ref::PluginRef;

/// Where a plugin runs: in the host's own process or in a `plugin-server`
/// process. A choice made per load ([`Loadable::hosting`]); everything a
/// loaded plugin offers is the same either way.
///
/// The default, [`Auto`](Self::Auto), is the format's own:
/// CLAP, VST3 and AU run [out of process](Self::OutOfProcess), which isolates a
/// crash and keeps the plugin out of the host's address space; VST2 runs [in
/// process](Self::InProcess), the only way it can run, since its `AEffect`
/// fuses the editor and the audio instance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Hosting {
    /// The format's default: out of process, except VST2, which is in process.
    #[default]
    Auto,
    /// In the host's process: no isolation, no IPC copy, and the editor is the
    /// audio instance. The load needs the main thread
    /// ([`MainThreadMarker`](tutti_plugin_types::MainThreadMarker)). Refused with
    /// [`BridgeError::NotInProcess`]
    /// when this build has no host for the format (its `tutti-plugin` feature
    /// is off).
    InProcess,
    /// In a `plugin-server` process, with its editor here. Refused with
    /// [`BridgeError::NotOutOfProcess`]
    /// for VST2, which cannot be served.
    OutOfProcess,
}

impl Hosting {
    /// Where a load of the `format` plugin at `path` runs: `InProcess` or
    /// `OutOfProcess`, never `Auto`.
    ///
    /// # Errors
    ///
    /// [`BridgeError::NotOutOfProcess`] for a VST2 plugin asked to be served;
    /// [`BridgeError::NotInProcess`] for a format this build has no in-process
    /// host for (VST2 included, with its `vst2` feature off).
    pub(crate) fn resolve(self, path: &Path, format: PluginFormat) -> BridgeResult<Self> {
        let name = format_name(format);
        match (self, format) {
            (Self::OutOfProcess, PluginFormat::Vst2) => Err(BridgeError::NotOutOfProcess {
                path: path.to_path_buf(),
                format: name,
            }),
            (Self::OutOfProcess, _) => Ok(Self::OutOfProcess),
            (Self::InProcess, _) | (Self::Auto, PluginFormat::Vst2) => {
                if in_process_built(format) {
                    Ok(Self::InProcess)
                } else {
                    Err(BridgeError::NotInProcess {
                        path: path.to_path_buf(),
                        format: name,
                    })
                }
            }
            (Self::Auto, _) => Ok(Self::OutOfProcess),
        }
    }
}

/// The format as a user knows it.
fn format_name(format: PluginFormat) -> &'static str {
    match format {
        PluginFormat::Vst3 => "VST3",
        PluginFormat::Vst2 => "VST2",
        PluginFormat::Clap => "CLAP",
        PluginFormat::AudioUnit => "AU",
        _ => "plugin",
    }
}

/// Whether this build holds the format's host for loading in this process.
fn in_process_built(format: PluginFormat) -> bool {
    let built = [
        (PluginFormat::Vst2, cfg!(feature = "vst2")),
        (PluginFormat::Clap, cfg!(feature = "clap")),
        (PluginFormat::Vst3, cfg!(feature = "vst3")),
        (
            PluginFormat::AudioUnit,
            cfg!(all(feature = "au", target_os = "macos")),
        ),
    ];
    built.iter().any(|&(has, on)| has == format && on)
}

/// A plugin the catalog holds, ready to load.
///
/// Made by [`Resolved::bind`] (or one of a resolution's arms) from
/// [`Catalog::resolve`]. It names the file the catalog's row is in, the
/// format whose loader opens it (which the file's extension need not give: a
/// link `Foo.clap` to `libfoo.so` is CLAP) and the plugin's id in the file. The
/// options chain by bare name: [`hosting`](Self::hosting).
///
/// **The blacklist gates every load and reload that goes through a
/// `Loadable`**: [`Plugin::open`](crate::catalog::Plugin::open), `PluginClient`'s
/// open and a session's crash reload ask it. Two reloads of a plugin that was
/// already admitted do not: the relaunch of a served plugin for a fork
/// (`IpcOrigin::reopen`) and the in-process editor instance of a served plugin
/// load the same file again from the origin the first load recorded, without
/// the view.
#[derive(Debug, Clone)]
pub struct Loadable {
    plugin: PluginRef,
    hosting: Hosting,
    blacklist: BlacklistView,
}

impl Loadable {
    /// `plugin` under `blacklist`, hosted as the format's default.
    pub(crate) fn new(plugin: PluginRef, blacklist: BlacklistView) -> Self {
        Self {
            plugin,
            hosting: Hosting::Auto,
            blacklist,
        }
    }

    /// A plugin that no catalog resolved: it consults no blacklist, so a
    /// plugin the scanner crashed on loads. The door for a caller that holds a
    /// [`PluginRef`] and no catalog; [`under`](Self::under) gives it a
    /// catalog's blacklist.
    #[must_use]
    pub fn uncatalogued(plugin: PluginRef) -> Self {
        Self::new(plugin, BlacklistView::empty())
    }

    /// This plugin under `catalog`'s blacklist as it is when the load opens it,
    /// including a change made after this call.
    #[must_use]
    pub fn under(mut self, catalog: &Catalog) -> Self {
        self.blacklist = catalog.blacklist_view();
        self
    }

    /// This plugin, hosted as `hosting` ([`Hosting::Auto`] until this says
    /// otherwise). Whether the format can be hosted that way is decided at the
    /// load ([`placement`](Self::placement)).
    #[must_use]
    pub fn hosting(mut self, hosting: Hosting) -> Self {
        self.hosting = hosting;
        self
    }

    /// This load's hosting and blacklist, for `plugin`: the reload of a plugin
    /// that loaded as this one did.
    #[must_use]
    pub fn retargeted(mut self, plugin: PluginRef) -> Self {
        self.plugin = plugin;
        self
    }

    /// The plugin: its file, its format and its id in the file.
    #[must_use]
    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// The format whose loader opens the file.
    #[must_use]
    pub fn format(&self) -> PluginFormat {
        self.plugin.format()
    }

    /// Where the load runs: [`Hosting::InProcess`] or
    /// [`Hosting::OutOfProcess`], never `Auto`. An in-process load (and the
    /// instance's drop) is confined to the main thread; an out-of-process one
    /// loads from any thread.
    ///
    /// # Errors
    ///
    /// [`BridgeError::NotOutOfProcess`] for a VST2 plugin asked to be served,
    /// [`BridgeError::NotInProcess`] for a format this build cannot host in
    /// process: the answer is the format's and the build's, so it comes before
    /// a server or a file is looked for.
    pub fn placement(&self) -> BridgeResult<Hosting> {
        self.hosting
            .resolve(self.plugin.path(), self.plugin.format())
    }

    /// Whether the catalog allows the load now: the check a load makes where it
    /// opens the plugin.
    ///
    /// # Errors
    ///
    /// [`BridgeError::Blacklisted`] with the recorded reason when the plugin's
    /// file is blacklisted and unchanged.
    pub fn check(&self) -> BridgeResult<()> {
        self.blacklist.check(&self.plugin)
    }
}

/// What a saved [`PluginId`](tutti_plugin_types::PluginId) names among the plugins the catalog holds now
/// ([`Catalog::resolve`]).
///
/// Only [`Exact`](Self::Exact) binds: [`bind`](Self::bind) takes it and
/// refuses the rest with the candidates for an app to ask about.
#[derive(Clone, Debug)]
pub enum Resolved {
    /// Exactly one plugin of the catalog has the saved format and id.
    Exact(Loadable),
    /// Several plugins of the catalog have the saved format and id (one
    /// installed in two directories). These are the candidates, ordered by
    /// file; an app asks.
    Ambiguous(Vec<Loadable>),
    /// No plugin of the catalog has the saved format and id: not installed,
    /// not scanned yet, or blacklisted.
    Missing,
}

/// Why [`Resolved::bind`] bound nothing. The candidates it carries are for an
/// app to ask about.
#[derive(Clone, Debug, thiserror::Error)]
pub enum BindError {
    /// No plugin of the catalog has the saved id.
    #[error("no plugin in the catalog has the saved id")]
    Missing,
    /// Several plugins of the catalog have the saved id.
    #[error("{} plugins in the catalog have the saved id", .0.len())]
    Ambiguous(Vec<Loadable>),
}

impl Resolved {
    /// The plugin to load without asking.
    ///
    /// # Errors
    ///
    /// [`BindError::Missing`] and [`BindError::Ambiguous`] for those
    /// resolutions.
    pub fn bind(self) -> Result<Loadable, BindError> {
        match self {
            Self::Exact(plugin) => Ok(plugin),
            Self::Ambiguous(candidates) => Err(BindError::Ambiguous(candidates)),
            Self::Missing => Err(BindError::Missing),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loadable(name: &str) -> Loadable {
        Loadable::uncatalogued(PluginRef::file(format!("/p/{name}.clap")).expect("a plugin file"))
    }

    /// **`bind` takes an exact resolution and nothing else**: an ambiguous one
    /// comes back with its candidates for the app to ask about, a missing one
    /// with nothing.
    #[test]
    fn bind_takes_only_an_exact_resolution() {
        let bound = Resolved::Exact(loadable("e")).bind().expect("exact binds");
        assert!(bound.plugin().path().ends_with("e.clap"));
        assert!(matches!(
            Resolved::Ambiguous(vec![loadable("x"), loadable("y")]).bind(),
            Err(BindError::Ambiguous(c)) if c.len() == 2
        ));
        assert!(matches!(Resolved::Missing.bind(), Err(BindError::Missing)));
    }
}
