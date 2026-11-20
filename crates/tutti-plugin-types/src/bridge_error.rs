//! [`IpcError`], the host-side error most calls return, and
//! [`PluginForkError`], why forking a plugin for an offline render failed.
//!
//! `IpcError` names the subprocess, the shared memory and the wire, none of
//! which a format loader knows about; the lean [`PluginError`] the loaders'
//! capability traits return converts to and from it with `From`.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::SlabError;
use thiserror::Error;

use crate::{
    EditorError, FrameTooLarge, LoadStage, PluginError, PluginFailure, ResultError,
    SlabAccessError, SlabLayoutError, StateError,
};

/// Anything that can go wrong hosting a plugin out of process.
///
/// Covers both the plugin's own failures and the bridge's — the subprocess, the
/// socket and the shared-memory slab are all represented, which is what
/// distinguishes this from the format-agnostic [`PluginError`].
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum IpcError {
    /// A [`Timeouts`](crate::Timeouts) with a zero deadline was configured.
    #[error(transparent)]
    Timeouts(#[from] crate::ZeroTimeout),

    /// The thread that watches the subprocess's end could not start, so the
    /// subprocess is not run.
    #[error("the plugin-server's exit cannot be watched")]
    ExitUnwatched(#[source] std::io::Error),

    /// The subprocess ended before it bound its control socket.
    #[error("the plugin-server exited before binding its socket")]
    ServerExited {
        /// How the server ended.
        #[source]
        cause: PluginFailure,
    },

    /// The subprocess did not take the audio slab the host offered.
    #[error("the plugin-server did not take the new audio slab")]
    SlabRefused,

    /// A frame's length is over the protocol's limit.
    #[error(transparent)]
    FrameTooLarge(#[from] FrameTooLarge),

    /// The plugin failed to load, with the stage it reached before failing.
    #[error("the plugin{} failed to load at the {stage} stage: {reason}", of_path(.path))]
    LoadFailed {
        /// The plugin that failed to load; `None` when the failure came from a
        /// loader that is handed no path (a [`PluginError::Load`]).
        path: Option<PathBuf>,
        /// How far the load got, which narrows the cause.
        stage: LoadStage,
        /// Human-readable reason, from the loader or the plugin itself.
        reason: String,
    },

    /// The plugin's format cannot be hosted out of process: a VST2 plugin's
    /// `AEffect` fuses its editor and its audio instance, so the instance
    /// that renders cannot run apart from its window. It is
    /// refused before any `plugin-server` is started; a VST2 plugin runs in
    /// the host's own process.
    #[error(
        "{path} is a {format} plugin, which cannot be hosted out of process: \
         its editor and its audio instance are one object"
    )]
    NotOutOfProcess {
        /// The plugin file.
        path: PathBuf,
        /// The format's name, as a user knows it.
        format: &'static str,
    },

    /// The plugin's format cannot be hosted in this process by this build: the
    /// format's host is not compiled in (its cargo feature is off). Asked for
    /// with `Hosting::InProcess`; refused before anything is loaded.
    #[error(
        "{path} is a {format} plugin, and this build has no host for {format} in the \
         host's own process (enable the format's feature of `tutti-plugin`)"
    )]
    NotInProcess {
        /// The plugin file.
        path: PathBuf,
        /// The format's name, as a user knows it.
        format: &'static str,
    },

    /// The plugin names no id and its file's extension names no plugin format,
    /// so no format's loader can be chosen for it.
    #[error("{} names no plugin format", path.display())]
    UnknownFormat {
        /// The plugin file.
        path: PathBuf,
        /// What its extension was.
        #[source]
        source: crate::UnknownExtension,
    },

    /// The `plugin-server` executable could not be located.
    #[error(
        "plugin-server binary not found. Build it with `cargo build -p tutti-plugin-server` \
         and either place it next to the application binary or set \
         TUTTI_PLUGIN_SERVER=/path/to/plugin-server"
    )]
    ServerNotFound,

    /// The `plugin-probe` executable could not be located.
    #[error(
        "plugin-probe binary not found. Build it with `cargo build -p tutti-plugin-probe` \
         and either place it next to the application binary or set \
         TUTTI_PLUGIN_PROBE=/path/to/plugin-probe"
    )]
    ProbeNotFound,

    /// The catalog records this plugin as having brought a scan down.
    ///
    /// A load of a plugin a catalog resolved (`Catalog::resolve`) returns this
    /// when the catalog blacklisted it since the resolve: the
    /// `Loadable` asks the catalog's blacklist where the load opens the plugin.
    /// `reason` lets a host say *which* plugin misbehaved and offer to
    /// unblacklist it and resolve again.
    ///
    /// False positives are expected: the scanner's dead-man's pedal fires on a
    /// force-quit, a power loss, or an OOM kill as readily as on a real crash.
    /// The record stores the file's mtime, so a reinstall or vendor update
    /// re-admits the plugin without the host doing anything.
    #[error("the plugin at {} is blacklisted: {reason}", path.display())]
    Blacklisted {
        /// The plugin the catalog refuses to load.
        path: std::path::PathBuf,
        /// Why it was blacklisted, so a host can offer to load it anyway.
        reason: String,
    },

    /// The subprocess replied with a message the host was not waiting for.
    #[error("unexpected bridge message: expected {expected}, got {got}")]
    UnexpectedMessage {
        /// The variant name the host awaited.
        expected: &'static str,
        /// The message that actually arrived: `Debug`-formatted by a reader
        /// that may allocate, a fixed name from one that may not.
        got: std::borrow::Cow<'static, str>,
    },

    /// Host and subprocess speak different wire versions and refuse to proceed.
    #[error(
        "plugin protocol version mismatch: host speaks {expected}, subprocess speaks {got} (rebuild the plugin-server)"
    )]
    ProtocolMismatch {
        /// The host's `PROTOCOL_VERSION` (`tutti-plugin-ipc`).
        expected: u32,
        /// The version the subprocess reported at handshake.
        got: u32,
    },

    /// The shared-memory audio slab could not be made, opened or trusted.
    #[error(transparent)]
    Slab(#[from] SlabError),

    /// The audio slab's layout is a shape this build does not serve.
    #[error(transparent)]
    SlabLayout(#[from] SlabLayoutError),

    /// A probe's result file could not be read.
    #[error(transparent)]
    ProbeResult(#[from] ResultError),

    /// A slab channel was read or written wrongly.
    #[error(transparent)]
    SlabAccess(#[from] SlabAccessError),

    /// The subprocess did not answer within the operation's deadline.
    #[error("{operation} timed out after {after:?}")]
    Timeout {
        /// What the host was waiting on.
        operation: &'static str,
        /// How long it waited.
        after: Duration,
        /// Whether the deadline expired **part-way through a frame**, leaving
        /// bytes of it consumed.
        ///
        /// This is the difference between a timeout a caller may resume from and
        /// one it may not. Nothing consumed means the stream is still on a frame
        /// boundary and the next read starts a whole message; a partial frame
        /// means the remaining bytes will be misread as a length prefix, and
        /// every later frame with them. The desynchronisation is silent at the
        /// point it happens and surfaces later as a decode error, so the flag
        /// has to be carried out from the only place that knows: the read loop.
        partial: bool,
    },

    /// The subprocess died. Any plugin state it held is gone.
    #[error("the bridge process crashed")]
    ProcessCrashed,

    /// The plugin's state could not be saved or restored, for the reason the
    /// [`StateError`] names.
    #[error(transparent)]
    State(#[from] StateError),

    /// A launch's staged state did not finish restoring: the server began the
    /// restore and the bridge failed while waiting on it. `cause` is the
    /// failure itself. A failure before the restore began is a load failure.
    #[error("the plugin did not finish restoring its state")]
    StateRestore {
        /// The bridge failure the restore ended in.
        #[source]
        cause: Box<IpcError>,
    },

    /// The plugin failed while processing audio.
    #[error("the plugin failed processing: {0}")]
    ProcessError(Cow<'static, str>),

    /// The plugin's editor could not be opened, embedded or closed.
    #[error(transparent)]
    Editor(#[from] EditorError),

    /// A failure a format loader reported in its own words that fits no other
    /// variant ([`PluginError::Other`]).
    #[error("the plugin failed: {0}")]
    Other(String),

    /// The peer broke the protocol: a message on a connection that does not
    /// carry it, or a call out of its phase.
    #[error("the peer broke the wire protocol: {0}")]
    ProtocolError(&'static str),

    /// The audio connection was misused; the failure it ended with.
    #[error("the audio connection failed")]
    AudioConnection {
        /// What the connection ended with.
        #[source]
        cause: Arc<IpcError>,
    },

    /// An underlying I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A wire message could not be postcard-encoded or -decoded.
    #[error("a wire message failed to encode or decode")]
    Serialization(#[source] postcard::Error),

    /// A frame's body holds bytes after its one message.
    #[error("{left} bytes are left after the message in a {frame}-byte frame")]
    TrailingBytes {
        /// Bytes after the message.
        left: usize,
        /// The frame body's length.
        frame: usize,
    },

    /// The handshake's first message does not decode: a server from another build.
    #[error(
        "the plugin-server's handshake does not decode: a server from another build \
         (protocol 26 or older?), rebuild the plugin-server"
    )]
    StaleServer {
        /// The decode failure.
        #[source]
        cause: Box<IpcError>,
    },
}

/// A host operation's result, erroring as [`IpcError`].
pub type IpcResult<T> = std::result::Result<T, IpcError>;

/// Why a hosted plugin could not be forked by state transfer.
///
/// Returned by
/// `tutti-plugin`'s `PluginClient::fork_instance`.
///
/// Every arm means **no fork exists**: a fresh instance that was started is
/// shut down with this value, never returned half-restored, and never replaced
/// by anything that shares the live instance. Which step failed is the arm,
/// so a host can tell "this plugin cannot save its state" (it will never fork)
/// from "the fresh instance did not start" (it may on a retry).
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum PluginForkError {
    /// The live instance is gone — every node and handle holding it dropped
    /// — so there is no state to fork from. Answered without any IPC.
    #[error("the live instance is gone; there is no state to fork from")]
    LiveGone,

    /// The live instance did not hand over its state — the plugin cannot save
    /// it (no state extension, or it refused), or the live bridge is gone.
    /// Asked before anything is loaded, so this costs no subprocess.
    #[error("the live instance did not hand over its state")]
    SaveState(#[source] StateError),

    /// A fresh instance of the same plugin did not load.
    #[error("a fresh instance at {} did not load", path.display())]
    Load {
        /// The plugin file the live instance was loaded from.
        path: PathBuf,
        /// Why the load failed.
        #[source]
        source: IpcError,
    },

    /// The live instance has no origin (it was not loaded from a file the
    /// host can load again), so there is nothing to load a second instance
    /// from. Answered before anything is loaded.
    #[error("the live instance has no origin to load a fresh instance from")]
    NoOrigin,

    /// The fresh instance refused the live instance's state.
    #[error("the fresh instance refused the live instance's state")]
    LoadState(#[source] StateError),

    /// The fork was asked for off the main thread, where an in-process plugin
    /// (VST2) cannot be loaded a second time. Nothing was loaded.
    #[error("an in-process plugin forks on the main thread only")]
    OffMainThread,
}

/// Renders ` at <path>` for a load failure that knows its plugin, nothing for
/// one that does not.
fn of_path(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map_or_else(String::new, |p| format!(" at {}", p.display()))
}

/// Widens the lean [`PluginError`] a format loader returns into a
/// `IpcError`, keeping every payload.
///
/// A [`PluginError::Load`] becomes [`IpcError::LoadFailed`] with no `path`:
/// the loader names none. A state failure keeps its [`StateError`] (a bare
/// [`PluginError::State`] message is a plugin's refusal,
/// [`StateError::Rejected`]); a [`PluginError::Bridge`] unwraps to the
/// `IpcError` it carried.
impl From<PluginError> for IpcError {
    fn from(e: PluginError) -> Self {
        match e {
            PluginError::Load { stage, reason } => IpcError::LoadFailed {
                path: None,
                stage,
                reason,
            },
            PluginError::State(msg) => IpcError::State(StateError::Rejected(msg)),
            PluginError::StateFailed(err) => IpcError::State(err),
            PluginError::Process(msg) => IpcError::ProcessError(Cow::Owned(msg)),
            PluginError::Block(why) => IpcError::ProcessError(Cow::Borrowed(why)),
            PluginError::Editor(err) => IpcError::Editor(err),
            PluginError::Bridge(err) => *err,
            PluginError::Other(msg) => IpcError::Other(msg),
        }
    }
}

/// Narrows a `IpcError` into the lean [`PluginError`] a format loader
/// returns.
///
/// A variant the lean type has a counterpart for keeps its payload, except that
/// a [`IpcError::LoadFailed`] drops its `path` (a [`PluginError::Load`] names
/// none). Every other variant (`ServerNotFound`, `ProtocolMismatch`, ...)
/// travels whole as [`PluginError::Bridge`], so widening it again returns the
/// same value.
impl From<IpcError> for PluginError {
    fn from(e: IpcError) -> Self {
        match e {
            IpcError::LoadFailed { stage, reason, .. } => PluginError::Load { stage, reason },
            IpcError::State(err) => PluginError::StateFailed(err),
            IpcError::ProcessError(msg) => PluginError::Process(msg.into_owned()),
            IpcError::Editor(err) => PluginError::Editor(err),
            IpcError::Other(msg) => PluginError::Other(msg),
            other => PluginError::Bridge(Box::new(other)),
        }
    }
}

/// A [`PluginError`] from a state call as the [`StateError`] a state caller
/// takes: a failure already typed as a state failure keeps its variant, a
/// dead server is [`StateError::PluginCrashed`], and any other failure is the
/// plugin's own refusal with its text.
impl From<PluginError> for StateError {
    fn from(e: PluginError) -> Self {
        match e {
            PluginError::StateFailed(err) => err,
            PluginError::State(msg) => StateError::Rejected(msg),
            PluginError::Bridge(b) if matches!(*b, IpcError::ProcessCrashed) => {
                StateError::PluginCrashed
            }
            other => StateError::Rejected(other.to_string()),
        }
    }
}

impl IpcError {
    /// Turn a server-reported error message into a `LoadFailed` at the
    /// `Opening` stage. The stage is coarse because the server's own
    /// error-message protocol doesn't carry a stage tag.
    #[must_use]
    pub fn load_from_server(path: &std::path::Path, message: String) -> Self {
        Self::LoadFailed {
            path: Some(path.to_path_buf()),
            stage: LoadStage::Opening,
            reason: message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state failure converted through `From<PluginError>` stays a state
    /// failure with its typed cause, and converts back to the same
    /// `StateError`; a flattening to text would leave `Busy` unrecoverable.
    #[test]
    fn a_state_failure_keeps_its_typed_cause_both_ways() {
        let stalled = StateError::Stalled {
            bytes: 7,
            after: Duration::from_millis(250),
        };
        match IpcError::from(PluginError::StateFailed(stalled.clone())) {
            IpcError::State(err) => assert_eq!(err, stalled),
            other => panic!("expected State, got {other:?}"),
        }
        assert!(matches!(
            IpcError::from(PluginError::State("truncated".into())),
            IpcError::State(StateError::Rejected(m)) if m == "truncated"
        ));
        match PluginError::from(IpcError::State(StateError::Busy)) {
            PluginError::StateFailed(StateError::Busy) => {}
            other => panic!("expected StateFailed(Busy), got {other:?}"),
        }
    }

    /// A load error widens with stage and reason intact and no invented path.
    #[test]
    fn widening_a_load_error_names_no_path() {
        match IpcError::from(PluginError::Load {
            stage: LoadStage::Factory,
            reason: "no factory entry point".into(),
        }) {
            IpcError::LoadFailed {
                path,
                stage,
                reason,
            } => {
                assert_eq!(path, None);
                assert_eq!(stage, LoadStage::Factory);
                assert_eq!(reason, "no factory entry point");
            }
            other => panic!("expected LoadFailed, got {other:?}"),
        }
    }

    /// A state call's `PluginError` becomes the `StateError` it already was, a
    /// crash stays a crash, and only an untyped failure reads as a refusal.
    #[test]
    fn a_state_calls_plugin_error_keeps_its_state_error() {
        assert_eq!(
            StateError::from(PluginError::StateFailed(StateError::Busy)),
            StateError::Busy
        );
        assert_eq!(
            StateError::from(PluginError::Bridge(Box::new(IpcError::ProcessCrashed))),
            StateError::PluginCrashed
        );
        assert_eq!(
            StateError::from(PluginError::State("old format".into())),
            StateError::Rejected("old format".into())
        );
    }

    /// Process, editor and other failures keep their own variant, and the
    /// editor error stays typed.
    #[test]
    fn process_editor_and_other_failures_keep_their_variant() {
        assert!(matches!(
            IpcError::from(PluginError::Process("underrun".into())),
            IpcError::ProcessError(m) if m == "underrun"
        ));
        assert!(matches!(
            IpcError::from(PluginError::Other("odd".into())),
            IpcError::Other(m) if m == "odd"
        ));
        let widened = IpcError::from(PluginError::Editor(EditorError::GuiNotSupported {
            format: "vst3".into(),
        }));
        assert!(matches!(
            &widened,
            IpcError::Editor(EditorError::GuiNotSupported { format }) if format == "vst3"
        ));
        assert!(matches!(
            PluginError::from(widened),
            PluginError::Editor(EditorError::GuiNotSupported { .. })
        ));
    }

    /// A bridge-only variant narrows to `PluginError::Bridge` and widens back
    /// to the same variant, so nothing is flattened to text on the way.
    #[test]
    fn a_bridge_only_failure_round_trips_whole() {
        let narrowed = PluginError::from(IpcError::ProcessCrashed);
        assert!(matches!(
            narrowed,
            PluginError::Bridge(ref b) if matches!(**b, IpcError::ProcessCrashed)
        ));
        assert!(matches!(IpcError::from(narrowed), IpcError::ProcessCrashed));
    }

    /// A load failure names its path only when it has one, and keeps its stage
    /// and reason through the narrowing.
    #[test]
    fn a_load_failure_names_its_path_only_when_it_has_one() {
        let with = IpcError::load_from_server(std::path::Path::new("/x.vst3"), "refused".into());
        assert_eq!(
            with.to_string(),
            "the plugin at /x.vst3 failed to load at the opening library stage: refused"
        );
        let without = IpcError::from(PluginError::Load {
            stage: LoadStage::Activation,
            reason: "refused".into(),
        });
        assert_eq!(
            without.to_string(),
            "the plugin failed to load at the activating stage: refused"
        );
        match PluginError::from(with) {
            PluginError::Load { stage, reason } => {
                assert_eq!(stage, LoadStage::Opening);
                assert_eq!(reason, "refused");
            }
            other => panic!("expected Load, got {other:?}"),
        }
    }

    /// A timeout and a restore failure read as one sentence each.
    #[test]
    fn a_timeout_and_a_restore_failure_read_as_one_sentence() {
        let timeout = IpcError::Timeout {
            operation: "recv",
            after: Duration::from_millis(250),
            partial: false,
        };
        assert_eq!(timeout.to_string(), "recv timed out after 250ms");
        let restore = IpcError::StateRestore {
            cause: Box::new(IpcError::ProcessCrashed),
        };
        assert_eq!(
            restore.to_string(),
            "the plugin did not finish restoring its state"
        );
    }

    /// Each cause's text appears once across a error's `source` chain and its
    /// own text: an error that prints its source and also returns it makes a
    /// reporter that walks the chain print the cause twice.
    #[test]
    fn a_cause_is_printed_once_along_the_source_chain() {
        use crate::PluginFailure;
        use std::error::Error;
        let errors: Vec<(Box<dyn Error>, &str)> = vec![
            (
                Box::new(IpcError::StateRestore {
                    cause: Box::new(IpcError::Timeout {
                        operation: "a reply",
                        after: Duration::from_secs(5),
                        partial: false,
                    }),
                }),
                "timed out after 5s",
            ),
            (
                Box::new(IpcError::State(StateError::Stalled {
                    bytes: 3,
                    after: Duration::from_secs(2),
                })),
                "stalled after 3 bytes",
            ),
            (
                Box::new(IpcError::Editor(EditorError::PluginCrashed)),
                "has crashed",
            ),
            (
                Box::new(IpcError::ExitUnwatched(std::io::Error::other(
                    "no thread to spare",
                ))),
                "no thread to spare",
            ),
            (
                Box::new(IpcError::Serialization(
                    postcard::Error::DeserializeUnexpectedEnd,
                )),
                "expected more data",
            ),
            (
                Box::new(IpcError::ServerExited {
                    cause: PluginFailure::StreamClosed,
                }),
                "stream closed",
            ),
            (
                Box::new(IpcError::AudioConnection {
                    cause: Arc::new(IpcError::ProtocolError("a reset on the wrong socket")),
                }),
                "a reset on the wrong socket",
            ),
            (
                Box::new(PluginForkError::SaveState(StateError::Busy)),
                "bridge is busy",
            ),
            (
                Box::new(PluginForkError::Load {
                    path: PathBuf::from("plugin.clap"),
                    source: IpcError::ProcessCrashed,
                }),
                "bridge process crashed",
            ),
            (
                Box::new(PluginForkError::LoadState(StateError::Rejected(
                    "older format".into(),
                ))),
                "older format",
            ),
        ];
        for (error, cause) in &errors {
            let mut chain = vec![error.to_string()];
            let mut next = error.source();
            while let Some(e) = next {
                chain.push(e.to_string());
                next = e.source();
            }
            assert_eq!(
                chain.iter().filter(|t| t.contains(cause)).count(),
                1,
                "{cause:?} in {chain:?}"
            );
        }
    }
}
