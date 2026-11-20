#![doc = include_str!("../README.md")]
//! The CPAL device layer: it connects Tutti's audio graph to a sound card.
//!
//! Device enumeration, the device's output stream and microphone capture.
//! `tutti-engine` knows how to render a block and owns the block interface
//! under every clock (`StreamDriver`, `OutputBlock`); this crate knows how to open a device,
//! hand it blocks on time, and pull audio back in from a microphone. It is the
//! only Tutti crate that links a platform audio API, so a headless render, an
//! offline export or a test never pulls one in.
//!
//! Use it directly when you assemble the engine yourself; the `tutti` facade
//! names its `AudioHost` and `DeviceId` at the root (feature `device`).
//!
//! ## Quick start
//!
//! List the output devices:
//!
//! ```rust,no_run
//! use tutti_cpal::{AudioHost, DeviceHost, Direction};
//!
//! # fn main() -> tutti_cpal::Result<()> {
//! let snapshot = DeviceHost::open(AudioHost::Default)?.snapshot()?;
//! for device in snapshot.devices(Direction::Output) {
//!     println!("{}: {}", device, device.name());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Play a graph: open a device, build the graph at the device's rate, and hand
//! both to a [`TuttiDriver`] with [`TuttiDriver::new`]:
//!
//! ```rust,no_run
//! use tutti_buffers::MaxBlock;
//! use std::sync::Arc;
//! use tutti_graph::{OutPort, Source};
//! use tutti_engine::Engine;
//! use tutti_transport::Transport;
//! use tutti_units::{Hz, Samples};
//! use tutti_graph::NodeKey;
//! use tutti_graph::{Editor, Prepare};
//! use tutti_nodes::testing::Osc;
//! use tutti_engine::AudioCallbackState;
//! use tutti_cpal::{AudioEngine, TuttiDriver};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Open a device first: it reports the rate the graph must be built at.
//! let mut audio_engine = AudioEngine::new(None)?;
//! let sample_rate = audio_engine.sample_rate();
//!
//! // The graph: an editor on the control thread, its executor for the audio
//! // thread. Edits are `commit`ted across.
//! let transport = Transport::new(sample_rate);
//! let (mut editor, executor) = Editor::new(Prepare::new(sample_rate, MaxBlock::of(Samples::new(512))));
//! let tone = NodeKey(1);
//! editor.insert(tone, "tone", Osc::sine(Hz::new(440.0)));
//! editor.spec_mut().topology.set_outputs(vec![Source::Node(OutPort { node: tone, port: 0 }); 2]);
//! editor.commit()?;
//!
//! // The engine owns the executor and the transport's clock; the editor stays
//! // with the host.
//! let engine = Engine::new(&transport, &mut editor, executor, tutti_engine::BlockDates::Os)?;
//! let state = Arc::new(AudioCallbackState::new(engine));
//!
//! audio_engine.start(Arc::clone(&state))?;
//! let driver = TuttiDriver::new(audio_engine, state);
//! assert!(driver.stream().is_running());
//! # Ok(())
//! # }
//! ```
//!
//! ## Main types
//!
//! - [`TuttiDriver`] — what a host holds: start, restart on another device or
//!   rate ([`TuttiDriver::restart_with`] re-rates the graph in a hook), stop.
//! - [`AudioEngine`] — one output device, its [`OutputSpec`](tutti_engine::OutputSpec),
//!   and the stream while it runs.
//! - [`CpalDriver`] — the `tutti_engine::StreamDriver` that runs the output
//!   callback (`AudioCallbackState`, `process_audio`) on CPAL's thread.
//! - [`DeviceHost`], [`AudioHost`], [`DeviceSelector`] — choose a platform host
//!   (default or JACK) and a device on it, by name (and order among same-named devices).
//! - `tutti_engine::StreamFaults` — backend errors (such as an unplugged device),
//!   which CPAL's error callback cannot return anywhere else.
//! - `MicIn` (feature `capture`) — a microphone as a `tutti_buffers::Source`,
//!   optionally with a live-monitor graph node. It delivers the inputs it was
//!   opened with, as unassigned channels: the first two by default, any
//!   list (`MicConfig::inputs` of `[2, 3]` is inputs 3 and 4 of 16).
//!
//! ## Real-time behaviour
//!
//! The output callback runs on CPAL's audio thread and does not allocate, lock or
//! block. Its buffers are sized once at stream start to `tutti_engine::MAX_FRAMES` frames; a
//! larger callback is clamped and its tail silenced rather than reallocating. Every
//! other method runs on the control thread.
//!
//! This crate does not own the graph (rendering a block is `tutti-engine`'s) or
//! recording (the pump that drives a mic into a file is `tutti-session`'s
//! `Recorder`, which is device-free).
//!
//! ## Features
//!
//! No features are enabled by default.
//!
//! - `capture` — microphone capture (`MicIn`), which hands back a monitor ring for an
//!   inlet (`tutti-graph`'s `InletNode`) and a `tutti-codec` sink. Without it the crate is output-only.
//! - `jack` — the JACK host ([`AudioHost::Jack`]). Linux and the BSDs only, and
//!   needs libjack at build time; elsewhere, or without the feature, opening JACK
//!   returns [`Error::HostUnavailable`].
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]

mod driver;
mod driver_seam;
mod edge;
mod error;
mod host;
mod listing;
mod output;
mod owner;
mod poller;
mod roster;
mod supported;

#[cfg(feature = "capture")]
mod mic;

pub use driver::TuttiDriver;
pub use driver_seam::{CpalDriver, CpalStream, PREFERRED_QUANTUM};
pub use error::{Error, Result};
pub use host::{AudioHost, DeviceHost, DeviceSelector, available_hosts};
pub use listing::{DeviceListing, Listed};
pub use output::{AudioEngine, OutputConfig, Resolved};
pub use poller::{DevicePoller, MIN_POLL_INTERVAL};
pub use roster::{DeviceChange, DeviceId, DeviceRoster, Direction, Snapshot, SnapshotSource};
pub use supported::SupportedConfig;

// Re-exported because this crate's public surface already names cpal types
// (cpal errors inside `Error`), so a consumer needs the vocabulary and should
// not have to pin a matching cpal version of its own.
pub use cpal;

#[cfg(feature = "capture")]
pub use mic::{MONITOR_RING, MicConfig, MicIn};
