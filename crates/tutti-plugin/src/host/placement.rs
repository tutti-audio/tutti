//! Where a load runs: a [`Loadable`]'s requested [`Hosting`] resolved against what
//! the format allows and what this build holds a host for.

use std::path::Path;
use tutti_plugin_db::{Hosting, Loadable};
use tutti_plugin_types::{IpcError, IpcResult, PluginFormat};

/// Where the load of `plugin` runs: [`Hosting::InProcess`] or
/// [`Hosting::OutOfProcess`], never `Auto`. An in-process load (and the
/// instance's drop) is confined to the main thread; an out-of-process one
/// loads from any thread.
///
/// # Errors
///
/// [`IpcError::NotOutOfProcess`] for a VST2 plugin asked to be served,
/// [`IpcError::NotInProcess`] for a format this build cannot host in
/// process: the answer is the format's and the build's, so it comes before
/// a server or a file is looked for.
pub fn placement(plugin: &Loadable) -> IpcResult<Hosting> {
    resolve(
        plugin.requested_hosting(),
        plugin.plugin().path(),
        format_of(plugin)?,
    )
}

/// The format whose loader opens `plugin`.
///
/// # Errors
///
/// [`IpcError::UnknownFormat`] for a plugin with no id whose file's extension
/// names no format.
pub(crate) fn format_of(plugin: &Loadable) -> IpcResult<PluginFormat> {
    plugin.format().map_err(|source| IpcError::UnknownFormat {
        path: plugin.plugin().path().to_path_buf(),
        source,
    })
}

/// Where a load of the `format` plugin at `path` runs when it asks for `hosting`:
/// `InProcess` or `OutOfProcess`, never `Auto`.
fn resolve(hosting: Hosting, path: &Path, format: PluginFormat) -> IpcResult<Hosting> {
    let name = format_name(format);
    match (hosting, format) {
        (Hosting::OutOfProcess, PluginFormat::Vst2) => Err(IpcError::NotOutOfProcess {
            path: path.to_path_buf(),
            format: name,
        }),
        (Hosting::OutOfProcess, _) => Ok(Hosting::OutOfProcess),
        (Hosting::InProcess, _) | (Hosting::Auto, PluginFormat::Vst2) => {
            if in_process_built(format) {
                Ok(Hosting::InProcess)
            } else {
                Err(IpcError::NotInProcess {
                    path: path.to_path_buf(),
                    format: name,
                })
            }
        }
        (Hosting::Auto, _) => Ok(Hosting::OutOfProcess),
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

#[cfg(test)]
mod tests {
    use super::*;

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
                resolve(Hosting::Auto, path, format).unwrap(),
                Hosting::OutOfProcess,
                "{format:?}"
            );
        }
    }

    /// **VST2 cannot be served, and is not quietly hosted here instead.**
    #[test]
    fn vst2_asked_for_out_of_process_is_refused() {
        let err = resolve(
            Hosting::OutOfProcess,
            Path::new("P.vst"),
            PluginFormat::Vst2,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            IpcError::NotOutOfProcess { format: "VST2", .. }
        ));
    }

    /// **A hosting the build has no host for is refused, typed**: the format's
    /// feature decides, for `Auto` on VST2 as for an explicit `InProcess`.
    #[cfg(not(feature = "vst2"))]
    #[test]
    fn vst2_in_process_needs_its_feature() {
        for hosting in [Hosting::Auto, Hosting::InProcess] {
            let err = resolve(hosting, Path::new("P.vst"), PluginFormat::Vst2).unwrap_err();
            assert!(matches!(err, IpcError::NotInProcess { format: "VST2", .. }));
        }
    }

    #[cfg(not(feature = "clap"))]
    #[test]
    fn clap_in_process_needs_its_feature() {
        let err = resolve(Hosting::InProcess, Path::new("P.clap"), PluginFormat::Clap).unwrap_err();
        assert!(matches!(err, IpcError::NotInProcess { format: "CLAP", .. }));
    }

    #[cfg(feature = "clap")]
    #[test]
    fn clap_in_process_is_admitted_with_its_feature() {
        assert_eq!(
            resolve(Hosting::InProcess, Path::new("P.clap"), PluginFormat::Clap).unwrap(),
            Hosting::InProcess
        );
    }

    /// **A ref with no id whose extension names no format is refused at
    /// placement**, typed, before anything is loaded.
    #[test]
    fn a_ref_of_no_format_is_refused_at_placement() {
        let loadable = Loadable::uncatalogued(tutti_plugin_types::PluginRef::file("/p/notes.txt"));
        match placement(&loadable) {
            Err(IpcError::UnknownFormat { path, .. }) => {
                assert_eq!(path, Path::new("/p/notes.txt"));
            }
            other => panic!("expected UnknownFormat, got {other:?}"),
        }
    }
}
