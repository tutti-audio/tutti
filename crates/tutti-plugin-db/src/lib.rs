#![doc = include_str!("../README.md")]
//! Plugin discovery and persistence.
//!
//! The pieces, from pure to stateful:
//!
//! - [`FileFacts`] is what the scan reads once from disk for a candidate; the
//!   decisions of what to probe are pure functions of it and a [`FileRow`].
//! - [`PluginDb`] is a plain table, `path -> FileRow` (modification time, then
//!   either the plugins the file holds or a blacklist). The file is the unit
//!   because a crash blacklists a file before any plugin id is known. A
//!   [`RowChange`] replaces or drops one row, and [`PluginDb::apply`] is the only
//!   code that changes the table. With feature `serde` it is `Serialize` and
//!   `Deserialize` as a versioned record: the app picks the file format and where
//!   it lives, and a database of another version, or a path listed twice, does
//!   not deserialize.
//! - [`Scanner`] probes the files that are new or changed since their row was
//!   stamped, through the probe it is given (the host supplies an out-of-process
//!   one). A scan is a [`ScanPlugins`] over a snapshot of the table: its
//!   `run` blocks the caller and returns the changes as [`ScannedPlugins`], which
//!   the owner applies with [`Catalog::apply`], so the table stays readable for
//!   the whole scan. A sentinel file (the dead-man's pedal)
//!   outlives a host that dies mid-probe and blacklists the culprit on the next
//!   run.
//!
//! - [`Catalog`] wraps a [`PluginDb`] with the persistence the app supplies and
//!   what a bare table cannot do: scans, registrations and measurements as
//!   cancellable values, crash recovery, the blacklist ([`BlacklistView`]) and
//!   [`resolve`](Catalog::resolve), which names a saved id's plugin as a
//!   [`Loadable`]. Its probe spawns `plugin-probe` on a file, waits under a
//!   deadline ([`DEFAULT_PROBE_TIMEOUT`], [`DEFAULT_MEASURE_TIMEOUT`]) and reads
//!   the result file, killing the child at the deadline or a stop. A
//!   [`CatalogConfig`] says where plugins live and what a scan does.
//!
//! The crate never loads a plugin and never touches the audio thread.

#![forbid(unsafe_code)]

mod catalog;
mod config;
mod facts;
mod fs;
mod loadable;
mod mtime;
mod os_path;
mod pedal;
mod probe;
mod probing;
mod scanner;
mod stop;
mod table;
#[cfg(feature = "serde")]
mod wire;

pub use catalog::{BlacklistView, Catalog, PersistError, SaveCatalog};
pub use config::CatalogConfig;
pub use facts::FileFacts;
pub use fs::canonical_path;
pub use loadable::{BindError, Hosting, Loadable, Resolved};
pub use mtime::{BeforeEpoch, Mtime};
pub use probe::DEFAULT_PROBE_TIMEOUT;
pub use probing::DEFAULT_MEASURE_TIMEOUT;
pub use scanner::{
    FileFailure, FilesError, ProbeFailure, ScanBusy, ScanCanceller, ScanOutcome, ScanPlugins,
    ScanProgress, ScanProgressReader, ScanReport, ScannedFile, ScannedPlugins, Scanner, StaleScan,
    probe_plugin_fallback,
};
pub use stop::Stop;
pub use table::{
    BlacklistedFile, FileRow, FileState, FormatMismatch, PluginDb, PluginEntry, Probed, RowChange,
};
#[cfg(feature = "serde")]
pub use wire::{DB_VERSION, InvalidDb};
