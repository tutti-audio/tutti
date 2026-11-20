//! Host-local configuration for a plugin bridge. Not wire data —
//! controls how *this* process connects to and manages a plugin-server.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use tutti_buffers::FloatWidth;
use tutti_plugin_types::{IpcError, Timeouts};

/// The operating system's random source could not be read, so no name that
/// another local user cannot guess can be made.
#[derive(Error, Debug)]
#[error("the operating system's random source is unreadable: {0}")]
pub struct RandomSourceError(getrandom::Error);

impl From<RandomSourceError> for IpcError {
    fn from(e: RandomSourceError) -> Self {
        Self::Io(std::io::Error::other(e))
    }
}

/// A socket path no other bridge will pick, and no other user can guess.
///
/// Every bridge needs its own: the path is a rendezvous point for exactly one
/// host/server pair, and `bind` unlinks whatever is already there. Two bridges
/// sharing one path means the second unlinks the first's *live* socket, and
/// then either `ProcessGuard::drop` deletes the survivor's socket out from
/// under it. The symptom is a plugin that dies when an unrelated plugin is
/// unloaded — far from the cause.
///
/// The pid and the counter keep it unique; the random part keeps it secret.
/// The directory is the shared temporary one, so a name another local user
/// can predict is a name they can claim first: a socket of theirs there is a
/// plugin wire they speak.
///
/// The directory is the temporary one when the path fits a Unix socket
/// address there (`transport::socket_path_in`), so a long `TMPDIR` (a deep worktree, or
/// macOS's per-user `/var/folders/…/T/`) moves the socket rather than failing
/// the launch.
///
/// # Errors
///
/// The operating system's random source cannot be read.
pub fn unique_socket_path() -> Result<PathBuf, RandomSourceError> {
    Ok(crate::transport::socket_path_in(
        &std::env::temp_dir(),
        &format!("{}.sock", unique_name("tutti-bridge")?),
    ))
}

/// `stem`, this process's id, a count shared by every name this process
/// makes, and [`unguessable`]: a name for something in a shared namespace
/// (the control socket) that is unique and not predictable.
///
/// # Errors
///
/// The operating system's random source cannot be read.
fn unique_name(stem: &str) -> Result<String, RandomSourceError> {
    static COUNTER: tutti_rt::Mint = tutti_rt::Mint::new();
    Ok(format!(
        "{stem}-{}-{}-{}",
        std::process::id(),
        COUNTER.mint(),
        unguessable()?,
    ))
}

/// Sixteen hex digits from the operating system's random source, for a name in
/// a shared directory that must not be predictable (the control socket).
/// Sixty-four bits: enough that
/// guessing is not an attack, short enough that a macOS socket path (104 bytes,
/// under a long per-user temporary directory) still fits.
///
/// # Errors
///
/// The operating system's random source cannot be read.
fn unguessable() -> Result<String, RandomSourceError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(RandomSourceError)?;
    Ok(format!("{:016x}", u64::from_ne_bytes(bytes)))
}

/// Low-level settings for one plugin-server connection.
///
/// Most hosts never build one: [`AudioConfig`](crate::AudioConfig)
/// covers the settings an application tunes, and each load derives a
/// `IpcConfig` from it. `tutti-plugin-server` takes one to know where to
/// listen.
///
/// [`at`](Self::at) takes the socket path and uses 8192-frame blocks, the default
/// [`Timeouts`] and 32-bit float samples. There is no `Default`: a fresh path ([`unique_socket_path`]) needs
/// the operating system's random source, which can fail, and a fixed one would
/// make every default a collision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcConfig {
    /// Local-socket path this bridge's host/server pair meet on (on Windows,
    /// its final component names a named pipe).
    ///
    /// Must be unique per bridge: binding unlinks whatever is already there,
    /// so two bridges sharing a path break each other. [`unique_socket_path`]
    /// makes one.
    pub socket_path: PathBuf,
    /// Largest block, in **frames**, the plugin will be asked to process. Sizes
    /// the slab, so a later block may not exceed it.
    pub max_buffer_size: usize,
    /// How long each wait on the subprocess runs before erroring.
    pub timeouts: Timeouts,
    /// Sample format to request; the subprocess replies with what it negotiated.
    #[serde(default = "crate::protocol::default_format")]
    pub preferred_format: FloatWidth,
    /// Clear the host's dumpable flag (Linux: PR_SET_DUMPABLE 0) before the
    /// first server is spawned, so a plugin, which runs as the same user, cannot
    /// `ptrace` the host or write its memory. Off by default: the flag also
    /// stops the host dumping core and an unprivileged debugger or crash
    /// handler attaching to it, which is the app's to choose. Irreversible for the
    /// process: the flag stays cleared, and a later `false` does not undo an
    /// earlier `true`. Nothing elsewhere
    /// (macOS's hardened runtime is the app's signing).
    #[serde(default)]
    pub protect_host_memory: bool,
}

impl IpcConfig {
    /// A configuration whose bridge meets its server at `socket_path`: a fresh
    /// [`unique_socket_path`] for a host, the path the host chose for the
    /// server.
    ///
    /// **A host's path is unique per bridge.** Two bridges on one path break each
    /// other: the second to bind unlinks the first's live socket, and the
    /// symptom is a plugin dying when an unrelated plugin is unloaded.
    #[must_use]
    pub fn at(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            max_buffer_size: 8192,
            timeouts: Timeouts::default(),
            preferred_format: FloatWidth::F32,
            protect_host_memory: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserted on `new()` itself rather than on `unique_socket_path`: the
    /// property that matters is that a fresh config does not collide, and a
    /// test of the helper alone passes even when `new()` stops calling it.
    #[test]
    fn each_new_config_gets_its_own_socket_path() {
        let a = IpcConfig::at(unique_socket_path().expect("the OS has a random source"));
        let b = IpcConfig::at(unique_socket_path().expect("the OS has a random source"));
        assert_ne!(
            a.socket_path, b.socket_path,
            "two fresh configs share a socket path: the second bridge to bind \
             will unlink the first's live socket"
        );
    }

    /// The socket's name carries sixty-four random bits, so another local user
    /// cannot predict it and bind it first. The pid and the
    /// counter alone keep names unique, which is why
    /// `each_new_config_gets_its_own_socket_path` cannot see this.
    #[test]
    fn the_socket_name_has_an_unguessable_part() {
        let path = unique_socket_path().expect("a random source");
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("a UTF-8 name");
        let random = stem.rsplit('-').next().expect("a last field");
        assert_eq!(random.len(), 16, "{stem}");
        assert!(random.chars().all(|c| c.is_ascii_hexdigit()), "{stem}");
        assert_ne!(
            unguessable().expect("a random source"),
            unguessable().expect("a random source"),
            "two draws should differ"
        );
    }

    /// A partial-update construction is the exact shape `to_bridge_config` uses,
    /// and the shape through which a shared path would propagate.
    #[test]
    fn partial_update_from_default_still_gets_a_unique_socket() {
        let a = IpcConfig {
            max_buffer_size: 1,
            ..IpcConfig::at(unique_socket_path().expect("the OS has a random source"))
        };
        let b = IpcConfig {
            max_buffer_size: 2,
            ..IpcConfig::at(unique_socket_path().expect("the OS has a random source"))
        };
        assert_ne!(a.socket_path, b.socket_path);
    }
}
