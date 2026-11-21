//! The plugin scan on the session: [`Session::scan_plugins`], [`Session::register_plugin`]
//! and [`Session::measure_plugin`] hand the catalog the session's executor, and return
//! the [`ScanPlugins`] the catalog makes, which the app awaits.
//!
//! The executor reaches the scan weakly: the scan's blocking walk and the exit wake of
//! each probe run on it, and it never keeps the session's executor alive. A session that
//! drops while a scan is pending ends the scan with [`ScanError::ShutDown`]
//! (`tutti_plugin_db`), never a hang and never a partial list.

use std::path::Path;

use tutti_plugin_db::{Catalog, FilesError, ScanBusy, ScanPlugins};
use tutti_plugin_types::PluginRef;

use crate::Session;

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// A scan of `catalog`'s directories, as a value the app awaits: the future is the
    /// scan itself, polled wherever the app awaits it, and it resolves the
    /// [`ScannedPlugins`](tutti_plugin_db::ScannedPlugins) that
    /// [`Catalog::apply`] applies. See [`Catalog::scan`].
    ///
    /// Read the scan's progress with [`ScanPlugins::progress`] before awaiting it.
    /// **Dropping the future cancels the scan**: the probe in flight is killed, the
    /// catalog's pedal is released and nothing is recorded. An app with a cancel
    /// button selects the future against the button, or drops it. The future resolves
    /// [`ScanError::ShutDown`](tutti_plugin_db::ScanError::ShutDown) once this session has
    /// dropped.
    ///
    /// # Errors
    ///
    /// [`ScanBusy`] while a scan or a crash recovery is using the catalog's pedal.
    pub fn scan_plugins(&self, catalog: &mut Catalog) -> Result<ScanPlugins, ScanBusy> {
        catalog.scan(self.plugins.executor.clone())
    }

    /// The probing of the plugin file at `path`, as [`scan_plugins`](Self::scan_plugins)
    /// returns a scan: the way to add a plugin outside the scanned directories. See
    /// [`Catalog::register`].
    ///
    /// # Errors
    ///
    /// [`FilesError`] as for [`Catalog::register`].
    pub fn register_plugin(
        &self,
        catalog: &mut Catalog,
        path: impl AsRef<Path>,
    ) -> Result<ScanPlugins, FilesError> {
        catalog.register(path, self.plugins.executor.clone())
    }

    /// The measurement of the file `plugin` names, as
    /// [`scan_plugins`](Self::scan_plugins) returns a scan. See [`Catalog::measure`].
    ///
    /// # Errors
    ///
    /// [`FilesError`] as for [`Catalog::measure`].
    pub fn measure_plugin(
        &self,
        catalog: &mut Catalog,
        plugin: &PluginRef,
        state: Option<&Path>,
    ) -> Result<ScanPlugins, FilesError> {
        catalog.measure(plugin, state, self.plugins.executor.clone())
    }
}
