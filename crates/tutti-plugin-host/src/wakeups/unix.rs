//! The unix look: `poll(2)` through the process edge's one OS layer.

use tutti_plugin_ipc_ffi::Ready;
use tutti_plugin_types::Wakeups;

/// Looks, without waiting, for which of the plugin's descriptors are ready,
/// into `ready` (replacing its contents).
pub(super) fn poll_now(wakeups: &Wakeups, ready: &mut Vec<Ready>) {
    ready.clear();
    if wakeups.watches().is_empty() {
        return;
    }
    tutti_plugin_ipc_ffi::poll_now(wakeups.watches(), ready);
}
