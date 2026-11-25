#![doc = include_str!("../README.md")]
//! The plugin host's process edge: the wire both ends speak, the
//! shared-memory slab and control socket it travels on, the plugin-server's
//! launch, and the client that drives a served plugin.
//!
//! `tutti-plugin` builds its out-of-process node on this crate and
//! `tutti-plugin-server` builds the other end of the same wire, so neither
//! can drift from the other. It knows no plugin format: the
//! editor-only instance a client opens in this process is loaded by a
//! function the host passes ([`EditorLoader`]).
//!
//! ## What the process boundary is
//!
//! Out-of-process hosting gives **crash and hang isolation**: a plugin that
//! crashes or hangs takes down its server, which the host sees as one plugin
//! failing. It is **not a security boundary**: the server runs as the host's
//! user, and a plugin there can do what that user can do, `ptrace` or write the
//! host's memory included. [`IpcConfig::protect_host_memory`] clears the
//! host's dumpable flag on Linux for the plugins it spawns afterwards (it also
//! stops core dumps and a same-user debugger or crash handler attaching, so it
//! is the app's choice and off by default). On macOS the equivalent is the app's
//! hardened-runtime signing, which is the app's to do.
//!
//! ## How a server is reached
//!
//! Nothing names the connection. On unix the host makes a socket pair and spawns
//! the server with one end as its standard input, so its command line and
//! environment say nothing and no other process can dial it; the server takes the
//! end as its first act, and `/dev/null` replaces its standard input. Windows has
//! no end to hand a child, so the host names an unguessable pipe in the server's
//! command line and the server listens on it. The first frame the server sends is
//! `Hello`, which carries the protocol version; the audio lane is a second
//! connection opened over the first.
//!
//! ## How a server ends
//!
//! Dropping the [`ProcessGuard`] asks the server to shut down (`Shutdown`), so
//! its plugin is destroyed, and gives it
//! [`Timeouts::shutdown`](tutti_plugin_types::Timeouts::shutdown) to end; then
//! it kills the server's process group (unix) and reaps it. On Windows the server
//! is in a Job Object that ends it when the host does, by any means.
//!
//! It has no `unsafe`: the slab's shared mapping and header, and the Windows
//! pipe peek behind the control socket's deadline, are `tutti-plugin-ipc-ffi`'s
//! safe calls.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

// The test build's allocator: without it every `assert_no_alloc` gate in
// this crate's tests is inert. Normal builds use the system allocator.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod batcher;
mod client;
mod config;
mod conversation;
mod error;
mod ipc;
mod offline_wait;
mod process_guard;
mod protocol;
mod server_watch;
mod subprocess;
mod transport;
mod wake;

#[cfg(test)]
mod process_pipeline_tests;
#[cfg(test)]
mod served_handle_tests;

pub(crate) use batcher::Batcher;
pub(crate) use batcher::{BlockPayload, Chunks};
pub(crate) use client::audio::HarmonyInputs;
pub(crate) use client::audio::IpcListener;
pub use client::audio::{AudioBridge, IpcEvent, IpcThread, Replies};
pub(crate) use client::audio::{BATCH_OUTCOMES, BatchOutcome};
pub use client::{EditorLoader, EditorSite, PluginBridge};
pub(crate) use wake::Wake;
// The bridge's free-buffer pool, for the delivery test in `tutti-plugin` that
// fills it the way the node does.
#[doc(hidden)]
pub use client::audio::{PARAM_BUFFERS, PayloadPool};
pub use config::{AudioConfig, IpcConfig};
pub use error::PluginRenderFault;
pub use ipc::{Ipc, IpcController, IpcParts, IpcProcessor, LaunchSpec, Launched};
pub use offline_wait::OfflineWait;
pub use process_guard::ProcessGuard;
pub use protocol::frame::{FrameBuf, read_frame, write_frame};
pub use protocol::{
    AUDIO_REPLY_FRAME_BYTES, AUDIO_REQUEST_FRAME_BYTES, Answered, AppMidiMapApplied,
    ArmPluginMidiLearn, AudioReply, AudioRequest, CONTROL_FRAME_BYTES, CloseEditor, ControlFrame,
    ControlReply, ControlRequest, CurrentPreset, EditorOpened, EditorSized, Framed,
    GetCurrentPreset, GetParameter, GetParameterList, GetParameterNormalized, GetParameterPlain,
    GetParameterText, GetParameterValueFromText, GetPresetList, GetWiring, Handle, Handling, Lane,
    LaneOpened, LoadPlugin, LoadPreset, LoadState, Message, ModeOutcome, News, OpenEditor,
    OpenLane, PROTOCOL_VERSION, ParameterList, ParameterNormalized, ParameterPlain, ParameterText,
    ParameterValue, ParameterValueFromText, PluginLoaded, PluginMidiLearn, PresetList,
    PresetLoaded, ProcessAudioData, Reaction, RenderModeSet, ReqId, Request, SampleRateSet,
    SaveState, SavedState, SetAppMidiMap, SetAutomationState, SetEditorSize, SetRenderMode,
    SetSampleRate, SetTrackInfo, SetupSharedMemory, SharedMemoryReady, Shutdown, SlabLayout,
    StageState, StagedState, StateLoaded, StateReady, StateStaged, Stop, Unanswered, Wiring,
    Working, serve,
};
pub use server_watch::ServerWatch;
pub(crate) use subprocess::launch;
pub use subprocess::{LoadRequest, Restore};
#[cfg(feature = "testing")]
pub use transport::broker_pair;
pub use transport::control::ControlStream;
#[cfg(feature = "testing")]
pub use transport::control::send_state_reply;
pub use transport::shm::{AudioSlab, BlobWire, RING_SLOTS, SlabHandle, SlabWire, SlabWriter};
pub use tutti_plugin_ipc_ffi::LaneWire;
