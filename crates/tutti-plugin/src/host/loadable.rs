//! [`Loadable`]: a plugin a catalog resolved, the only thing a load takes; and
//! [`Resolved`]: what a saved [`PluginId`](tutti_plugin_types::PluginId) names
//! among the plugins the catalog holds now.
//!
//! A load starts from the catalog, so its blacklist cannot be skipped: a
//! [`Loadable`] carries the catalog's [`BlacklistView`], and the load asks it
//! where it opens the plugin. A plugin blacklisted after it was resolved, by a
//! scan that ran meanwhile, is refused there.

use std::path::Path;
use tutti_plugin_db::PluginFormat;
use tutti_plugin_types::{BridgeError, BridgeResult};

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
/// fuses the editor and the audio instance. `Auto` never moves a plugin the
/// catalog has blacklisted into the host's process: a blacklisted
/// plugin is refused at the load, whatever the hosting.
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
    /// plugin the scanner crashed on loads. For a caller that holds a
    /// [`PluginRef`] and no catalog; [`under`](Self::under) brings one back.
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

    /// The hosting the load asked for ([`Hosting::Auto`] unless
    /// [`hosting`](Self::hosting) said otherwise): not where it will run, which
    /// is [`placement`](Self::placement).
    #[must_use]
    pub fn requested_hosting(&self) -> Hosting {
        self.hosting
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
/// Only [`Exact`](Self::Exact) is the saved plugin itself: the near matches say
/// the catalog holds something else the id may stand for, and
/// [`bind`](Self::bind) takes one only as far as its [`BindPolicy`] says.
#[derive(Clone, Debug)]
pub enum Resolved {
    /// Exactly one plugin of the catalog has the saved format and id.
    Exact(Loadable),
    /// The saved format and id, in another version of the plugin.
    Version(Loadable),
    /// A plugin that declares it replaces the saved id (a VST3 module's
    /// compatibility list, the VST2 id a VST3 plugin derives its class id from,
    /// a CLAP plugin's compatibility ids).
    Alias(Loadable),
    /// Several plugins of the catalog have the saved format and id (one
    /// installed in two directories). These are the candidates, ordered by
    /// file; an app asks.
    Ambiguous(Vec<Loadable>),
    /// No plugin of the catalog has the saved format and id: not installed,
    /// not scanned yet, or blacklisted.
    Missing,
}

/// Which resolutions [`Resolved::bind`] takes without asking, each taking
/// everything the one before it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BindPolicy {
    /// Only an [`Exact`](Resolved::Exact) plugin: the one that is safe to bind
    /// unasked.
    ExactOnly,
    /// An exact plugin, or [another version](Resolved::Version) of it.
    OrVersion,
    /// An exact plugin, another version, or [a plugin that declares it replaces
    /// the saved one](Resolved::Alias): for an app that has nothing to ask with.
    OrAlias,
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
    /// Only another version of the saved plugin is present, and the policy
    /// binds exact matches only.
    #[error("only another version of the saved plugin is present")]
    VersionDeclined(Loadable),
    /// Only a plugin that replaces the saved one is present, and the policy
    /// does not bind aliases.
    #[error("only a plugin that replaces the saved one is present")]
    AliasDeclined(Loadable),
}

impl Resolved {
    /// The plugin to load without asking, by `policy`.
    ///
    /// # Errors
    ///
    /// [`BindError::Missing`] and [`BindError::Ambiguous`] for those
    /// resolutions, [`BindError::VersionDeclined`] and
    /// [`BindError::AliasDeclined`] for a near match `policy` does not take.
    pub fn bind(self, policy: BindPolicy) -> Result<Loadable, BindError> {
        match (self, policy) {
            (Self::Exact(plugin), _)
            | (Self::Version(plugin), BindPolicy::OrVersion | BindPolicy::OrAlias)
            | (Self::Alias(plugin), BindPolicy::OrAlias) => Ok(plugin),
            (Self::Version(plugin), BindPolicy::ExactOnly) => {
                Err(BindError::VersionDeclined(plugin))
            }
            (Self::Alias(plugin), BindPolicy::ExactOnly | BindPolicy::OrVersion) => {
                Err(BindError::AliasDeclined(plugin))
            }
            (Self::Ambiguous(candidates), _) => Err(BindError::Ambiguous(candidates)),
            (Self::Missing, _) => Err(BindError::Missing),
        }
    }

    /// Every plugin that could be the saved one: the one of an exact or near
    /// resolution, the several of an [`Ambiguous`](Self::Ambiguous) one, and
    /// none of a [`Missing`](Self::Missing) one. What an app that asks lists.
    #[must_use]
    pub fn into_candidates(self) -> Vec<Loadable> {
        match self {
            Self::Exact(plugin) | Self::Version(plugin) | Self::Alias(plugin) => vec![plugin],
            Self::Ambiguous(candidates) => candidates,
            Self::Missing => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loadable(name: &str) -> Loadable {
        Loadable::uncatalogued(PluginRef::file(format!("/p/{name}.clap")).expect("a plugin file"))
    }

    /// **`bind` takes a resolution as far as its policy says and no further**:
    /// an exact plugin under every policy, a version from `OrVersion` up, an
    /// alias only under `OrAlias`; a refused near match comes back in its error
    /// for the app to ask about.
    #[test]
    fn bind_takes_a_resolution_as_far_as_the_policy_says() {
        use BindPolicy::{ExactOnly, OrAlias, OrVersion};
        let bound = |resolved: Resolved, policy| {
            resolved
                .bind(policy)
                .map(|l| l.plugin().path().display().to_string())
        };
        for policy in [ExactOnly, OrVersion, OrAlias] {
            assert_eq!(
                bound(Resolved::Exact(loadable("e")), policy).unwrap(),
                "/p/e.clap"
            );
        }
        assert!(matches!(
            bound(Resolved::Version(loadable("v")), ExactOnly),
            Err(BindError::VersionDeclined(l)) if l.plugin().path().ends_with("v.clap")
        ));
        assert_eq!(
            bound(Resolved::Version(loadable("v")), OrVersion).unwrap(),
            "/p/v.clap"
        );
        assert_eq!(
            bound(Resolved::Version(loadable("v")), OrAlias).unwrap(),
            "/p/v.clap"
        );
        for policy in [ExactOnly, OrVersion] {
            assert!(matches!(
                bound(Resolved::Alias(loadable("a")), policy),
                Err(BindError::AliasDeclined(l)) if l.plugin().path().ends_with("a.clap")
            ));
        }
        assert_eq!(
            bound(Resolved::Alias(loadable("a")), OrAlias).unwrap(),
            "/p/a.clap"
        );
    }

    /// **Nothing binds an ambiguous or a missing resolution**, under the widest
    /// policy: the candidates are the app's to choose from.
    #[test]
    fn an_ambiguous_or_missing_resolution_never_binds() {
        let two = Resolved::Ambiguous(vec![loadable("x"), loadable("y")]);
        assert!(matches!(
            two.clone().bind(BindPolicy::OrAlias),
            Err(BindError::Ambiguous(c)) if c.len() == 2
        ));
        assert_eq!(two.into_candidates().len(), 2);
        assert!(matches!(
            Resolved::Missing.bind(BindPolicy::OrAlias),
            Err(BindError::Missing)
        ));
        assert!(Resolved::Missing.into_candidates().is_empty());
        assert_eq!(Resolved::Exact(loadable("e")).into_candidates().len(), 1);
    }

    /// **Where a load runs is the load's option, not the plugin's**: the same
    /// plugin asked two ways names one file and two placements.
    #[test]
    fn hosting_is_an_option_of_the_load() {
        let served = loadable("h").hosting(Hosting::OutOfProcess);
        let here = served.clone().hosting(Hosting::InProcess);
        assert_eq!(served.plugin(), here.plugin());
        assert_eq!(served.requested_hosting(), Hosting::OutOfProcess);
        assert_eq!(here.requested_hosting(), Hosting::InProcess);
        assert_eq!(loadable("h").requested_hosting(), Hosting::Auto);
    }

    /// **`Auto` is the format's default**: CLAP, VST3 and AU
    /// are served, whatever the build holds.
    #[test]
    fn auto_serves_every_format_but_vst2() {
        for format in [
            PluginFormat::Clap,
            PluginFormat::Vst3,
            PluginFormat::AudioUnit,
        ] {
            let path = Path::new("P");
            assert_eq!(
                Hosting::Auto.resolve(path, format).unwrap(),
                Hosting::OutOfProcess,
                "{format:?}"
            );
        }
    }

    /// **VST2 cannot be served, and is not quietly hosted here instead.**
    #[test]
    fn vst2_asked_for_out_of_process_is_refused() {
        let err = Hosting::OutOfProcess
            .resolve(Path::new("P.vst"), PluginFormat::Vst2)
            .unwrap_err();
        assert!(matches!(
            err,
            BridgeError::NotOutOfProcess { format: "VST2", .. }
        ));
    }

    /// **A hosting the build has no host for is refused, typed**: the format's
    /// feature decides, for `Auto` on VST2 as for an explicit `InProcess`.
    #[cfg(not(feature = "vst2"))]
    #[test]
    fn vst2_in_process_needs_its_feature() {
        for hosting in [Hosting::Auto, Hosting::InProcess] {
            let err = hosting
                .resolve(Path::new("P.vst"), PluginFormat::Vst2)
                .unwrap_err();
            assert!(matches!(
                err,
                BridgeError::NotInProcess { format: "VST2", .. }
            ));
        }
    }

    #[cfg(not(feature = "clap"))]
    #[test]
    fn clap_in_process_needs_its_feature() {
        let err = Hosting::InProcess
            .resolve(Path::new("P.clap"), PluginFormat::Clap)
            .unwrap_err();
        assert!(matches!(
            err,
            BridgeError::NotInProcess { format: "CLAP", .. }
        ));
    }

    #[cfg(feature = "clap")]
    #[test]
    fn clap_in_process_is_admitted_with_its_feature() {
        assert_eq!(
            Hosting::InProcess
                .resolve(Path::new("P.clap"), PluginFormat::Clap)
                .unwrap(),
            Hosting::InProcess
        );
    }
}
