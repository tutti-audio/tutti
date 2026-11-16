//! Linux: the plugin servers this process has running, read from `/proc`.

use crate::support::plugin_server;

/// The plugin servers this process has running: its children whose command
/// line names the server binary.
pub(super) fn servers() -> Option<usize> {
    let me = std::process::id().to_string();
    let server = plugin_server();
    let server = server.file_name().unwrap().to_string_lossy().into_owned();
    let mut n = 0;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let dir = entry.path();
        let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
            continue;
        };
        // `pid (comm) state ppid ...`: comm may hold spaces, so split after its `)`.
        let after = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
        if after.split_whitespace().nth(1) != Some(me.as_str()) {
            continue;
        }
        let cmd = std::fs::read(dir.join("cmdline")).unwrap_or_default();
        if String::from_utf8_lossy(&cmd).contains(&server) {
            n += 1;
        }
    }
    Some(n)
}
