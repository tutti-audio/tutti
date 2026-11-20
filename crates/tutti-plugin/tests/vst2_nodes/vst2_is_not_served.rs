//! A VST2 file is refused for out-of-process hosting with a typed error, before
//! any `plugin-server` is looked for or started: its `AEffect` fuses the editor
//! and the audio instance, so the instance that renders cannot run apart from
//! its window.
//!
//! Needs no server binary: the refusal is the client's.

use std::path::PathBuf;

use tutti_plugin::PluginClient;
use tutti_plugin_ipc::IpcConfig;
use tutti_plugin_types::IpcError;

/// **A `.vst`, `.dll` or `.so` path is refused as `NotOutOfProcess`**, whether
/// or not the file exists and whether or not a server can be found: the answer
/// is about the format. Before the check this reached the launch and failed as
/// `ServerNotFound` (or, with a server, as a load failure of the server's).
#[test]
fn a_vst2_path_is_refused_with_a_typed_error_before_a_server_starts() {
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", "/nonexistent/plugin-server");
    for name in ["Synth.vst", "Legacy.dll", "Old.so"] {
        let path = PathBuf::from(name);
        match PluginClient::new(
            IpcConfig::at(
                tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source"),
            ),
            &tutti_fixtures::pumped_work(),
            &tutti_plugin_db::Loadable::uncatalogued(tutti_plugin_types::PluginRef::file(
                path.clone(),
            )),
            tutti_units::SampleRate::SR_48K,
        ) {
            Err(IpcError::NotOutOfProcess {
                path: refused,
                format,
            }) => {
                assert_eq!(refused, path);
                assert_eq!(format, tutti_plugin_types::PluginFormat::Vst2);
            }
            Err(other) => panic!("{name}: expected NotOutOfProcess, got {other:?}"),
            Ok(_) => panic!("{name}: a VST2 file must not be served"),
        }
    }
}
