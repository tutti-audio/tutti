#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
//! The `plugin-server` subprocess behind Tutti's out-of-process plugin hosting.
//!
//! ## What this is
//!
//! [`tutti-plugin`](https://docs.rs/tutti-plugin) spawns one `plugin-server`
//! process per loaded plugin. This crate is that process: it hosts one VST3, CLAP
//! or AU plugin in isolation, speaks [`tutti_plugin_ipc`]'s wire protocol over a
//! Unix socket the host hands it as its standard input (a named pipe on Windows),
//! and moves audio through a shared-memory slab. A plugin that crashes takes down this process and nothing else.
//!
//! It is one generic shell, `Server<L: OutOfProcess>`, over what each served
//! format's host crate implements (`Loaded`, `Processor`, `Controller`, `Control`):
//! a format is a row of the `served!` table, which calls `F::load_served`, so a row
//! for a format that cannot be served from another process does not compile. VST2
//! is not one: its `AEffect` fuses editor and audio, so it runs in the host's
//! process; a VST2 file is refused by the client and, if one
//! arrives, by the table.
//!
//! Most users never call this library. They build the binary and let
//! `tutti-plugin` find it:
//!
//! ```text
//! cargo build -p tutti-plugin-server
//! ```
//!
//! `tutti-plugin` looks for `plugin-server` in the `TUTTI_PLUGIN_SERVER`
//! environment variable, next to the running executable, in its parent directory,
//! and on `PATH`.
//!
//! ## Running a server yourself
//!
//! The library has one entry point, [`PluginServer`]. The host makes the
//! connection and hands this process its end: [`adopt_host`] takes it (the stream
//! socket on standard input on unix, after which standard input is `/dev/null`;
//! the pipe name in the first argument on Windows), and [`run`][`PluginServer::run`]
//! serves one host and returns when the host disconnects or asks for shutdown.
//!
//! ```rust,no_run
//! use tutti_plugin_server::{PluginServer, adopt_host};
//!
//! // First thing the process does: it replaces standard input.
//! let host = adopt_host().expect("started by a host");
//!
//! PluginServer::new(host)
//!     .expect("on the process's main thread")
//!     .run()
//!     .expect("session");
//! ```
//!
//! Nothing names the connection: a process that was not handed one has nothing to
//! meet. `IpcError` is `tutti-plugin-types`'; the wire protocol and its version are
//! defined in `tutti-plugin-ipc`. A load carries which plugin of a file that
//! exports several to load (`LoadPlugin::load`).
//!
//! ## How a session runs
//!
//! The host's connection, made before it spawned this process, is the broker. It opens with a
//! `Hello` message carrying the protocol version, which the host refuses when it
//! does not know it. Over it the host does the handshake (plugin load, format
//! negotiation and shared-memory setup) and then opens the audio lane, handing the
//! server its end of a fresh stream. The audio lane carries per-block audio
//! traffic for the rest of the session, served by a render thread that asks the OS
//! for the highest priority it grants (not real time; a refusal is logged) and
//! sends plugin events to the host after each block. The broker stays the control
//! connection, served by the main thread's event loop.
//!
//! If the host dies, at any point of the session, the server notices on its own
//! and exits: from [`PluginServer::new`] on, a thread the OS wakes when the host
//! process ends (the PID the host passes in `HOST_PID_ENV`) ends the
//! server wherever it is blocked.
//!
//! ## Logs and the exit status
//!
//! The binary logs to standard error, at `info` unless `RUST_LOG` says otherwise.
//! It exits with status 0 when its session ended and every plugin was destroyed,
//! [`EXIT_PLUGIN_LEAKED`] when a plugin had to be leaked (its render thread never
//! returned the processor), [`EXIT_TASKS_DETACHED`] when every plugin was
//! destroyed but a task of the server did not stop in time and was left
//! running, [`EXIT_RENDER_LANE_LOST`] when a render lane panicked or was
//! dropped, [`EXIT_HOST_UNWATCHED`] when the thread that watches the host could
//! not start, and 1 on any other error ([`failure_status`]).
//!
//! ## Isolation
//!
//! Hosting a plugin in this process gives crash and hang isolation: a plugin
//! that crashes or hangs takes down this process, not the host's. It is **not a
//! security boundary**: the process runs as the host's user, so a hostile plugin
//! can do what that user can do. The host's side of this
//! (`IpcConfig::protect_host_memory`) is documented in `tutti-plugin-ipc`.
//!
//! ## Features
//!
//! | Feature | Enables |
//! |---|---|
//! | `vst3` | VST3 plugins, through `tutti-vst3-host` |
//! | `clap` | CLAP plugins, through `tutti-clap-host` |
//! | `au` | Audio Unit plugins, through `tutti-au-host` (macOS only; elsewhere it compiles to no AU support) |
//!
//! All three are on by default.
//!
//! ## Related crates
//!
//! - [`tutti-plugin`](https://docs.rs/tutti-plugin): the host side, which spawns
//!   and talks to this process.
//! - [`tutti-plugin-types`](https://docs.rs/tutti-plugin-types): the plugin
//!   capability traits the format adapters here implement.
//! - The format hosts: [`tutti-vst2-host`](https://docs.rs/tutti-vst2-host),
//!   [`tutti-vst3-host`](https://docs.rs/tutti-vst3-host),
//!   [`tutti-clap-host`](https://docs.rs/tutti-clap-host) and
//!   [`tutti-au-host`](https://docs.rs/tutti-au-host).
//!
//! [`PluginServer`]: crate::PluginServer
//! [`PluginServer::run`]: crate::PluginServer::run
//! [`tutti_plugin_ipc`]: tutti_plugin_ipc
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

mod audio_pipeline;
mod leak;
mod main_loop;
mod render;
// With no format served the table is an uninhabited enum and nothing above it
// runs.
#[cfg_attr(not(served), allow(dead_code))]
mod served;
mod server;
mod session;
mod ties;
mod transport;

pub use main_loop::{HostEnd, adopt_host};
pub use server::{
    EXIT_HOST_UNWATCHED, EXIT_PLUGIN_LEAKED, EXIT_RENDER_LANE_LOST, EXIT_TASKS_DETACHED, Ended,
    PluginServer, failure_status,
};

// Test-only global allocator for the audio-pipeline RT-safety regression
// test (`audio_pipeline::tests::process_is_alloc_free`). Active only in the
// test build; normal builds use the system allocator.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

/// Plugin-loading tests across the crate serialize on this lock: format
/// modules keep process globals of their own, and plugins' `main` may too.
///
/// Gated on a served format as well as `test`: every consumer is a format's
/// test, so a test build with no format on compiles this module with nothing
/// to serialize.
///
/// The lock is the only member shared by every format. Everything else is
/// gated to the *specific* format it serves — a `clap`-only build has no VST3
/// bundle — and the `env!` constants especially, since those read build-script
/// variables that only exist once that format has built its reference plugin.
#[cfg(all(test, served))]
pub(crate) mod test_utils {
    // Every import here belongs to the probe-path helpers, which are gated to
    // the format whose reference plugin they find.
    // `Path` belongs to the VST3 bundle walk; CLAP only resolves a path string.
    #[cfg(feature = "vst3")]
    use std::path::Path;
    use std::sync::MutexGuard;
    #[cfg(any(feature = "clap", feature = "vst3"))]
    use std::sync::OnceLock;
    #[cfg(feature = "clap")]
    use tutti_clap_test_plugin::{ClapProbe, ClapSwitch};
    use tutti_rt::Mutex;

    /// The reference CLAP plugin's probe, opened once on its two images and
    /// kept for the process: a switch set before a load must stay resident
    /// for the instance loaded after it.
    #[cfg(feature = "clap")]
    pub fn clap() -> &'static ClapProbe {
        static PROBE: OnceLock<ClapProbe> = OnceLock::new();
        PROBE.get_or_init(|| {
            ClapProbe::open(
                &tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
                tutti_fixtures::clap_probe(),
            )
        })
    }

    /// Taken by every test that loads a real plugin — CLAP, VST3 and, on macOS,
    /// AU. A plugin's entry point may touch process globals, so loads are kept
    /// one at a time in tests.
    pub static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

    /// The reference VST3 bundle's parent directory, forwarded from
    /// `tutti-vst3-host` by `build.rs`. Empty when that crate built no probe.
    #[cfg(feature = "vst3")]
    const VST3_PROBE_DIR: &str = env!("TUTTI_VST3_PROBE_DIR");

    /// Absolute path to `audio-probe`, the reference VST3 binary the loader
    /// tests drive.
    ///
    /// The counterpart to [`clap_probe_path`], and it panics for the same
    /// reason. `audio-probe` is built by `tutti-vst3-host`'s build script
    /// against the in-repo SDK submodules, as a dev-dependency of this crate,
    /// during this same `cargo test` — so its absence is a build failure, not a
    /// property of the machine.
    ///
    /// # Panics
    ///
    /// If the probe is absent — see above. The message names the likely cause.
    #[cfg(feature = "vst3")]
    pub fn vst3_probe_path() -> &'static str {
        static RESOLVED: OnceLock<String> = OnceLock::new();
        RESOLVED
            .get_or_init(|| {
                assert!(
                    !VST3_PROBE_DIR.is_empty(),
                    "no VST3 probe directory: `tutti-vst3-host` did not export one. \
                     It is a dev-dependency of this crate with `features = \
                     [\"conformance\"]`, so this means its build script did not run \
                     or did not build the probe."
                );

                let bundle = Path::new(VST3_PROBE_DIR).join("audio-probe.vst3");
                binary_in_bundle(&bundle).unwrap_or_else(|| {
                    panic!(
                        "audio-probe not found under {}. `tutti-vst3-host`'s build \
                         script builds it from the in-repo SDK submodules, so this is \
                         a build failure. If the SDK submodules are empty, run: \
                         git submodule update --init --recursive",
                        bundle.display()
                    )
                })
            })
            .as_str()
    }

    /// Absolute path to the SDK's `multiple_programchanges` sample, built beside
    /// the probe.
    ///
    /// The one fixture that can witness a VST3 **program list id**: it declares
    /// 16 lists whose ids are `kProgramStartId + i`, so an id is provably not a
    /// position. `audio-probe` publishes no program lists, so it cannot stand in.
    ///
    /// # Panics
    ///
    /// If absent. It is built from the in-repo SDK submodule by the same
    /// `cargo test`, so that is a build failure.
    #[cfg(feature = "vst3")]
    pub fn vst3_program_sample_path() -> &'static str {
        static RESOLVED: OnceLock<String> = OnceLock::new();
        RESOLVED
            .get_or_init(|| {
                assert!(
                    !VST3_PROBE_DIR.is_empty(),
                    "no VST3 probe directory: `tutti-vst3-host` did not export one."
                );
                let bundle = Path::new(VST3_PROBE_DIR).join("multiple-program-changes.vst3");
                binary_in_bundle(&bundle).unwrap_or_else(|| {
                    panic!(
                        "the multiple-program-changes sample was not found under {}. \
                         `tutti-vst3-host`'s build script builds it from the in-repo \
                         SDK submodule, so this is a build failure. If the submodules \
                         are empty, run: git submodule update --init --recursive",
                        bundle.display()
                    )
                })
            })
            .as_str()
    }

    /// The single binary inside a `.vst3` bundle.
    ///
    /// The layout is the plugin format's, not cargo's: one of these arch dirs
    /// holds it. All are probed rather than deriving one from `cfg!`, so a
    /// cross-build lands in the slower branch instead of a wrong answer.
    #[cfg(feature = "vst3")]
    fn binary_in_bundle(bundle: &Path) -> Option<String> {
        // `any_*`: the corpus may hold a cross-built fixture, and finding it is
        // better than skipping it. The arch list and the linker-by-product skip
        // both live in `tutti_plugin_types::bundle` now.
        tutti_plugin_types::bundle::any_module_in_bundle(
            bundle,
            tutti_plugin_types::bundle::ModuleKind::Vst3,
        )
        .map(|p| p.to_string_lossy().into_owned())
    }

    /// [`RenderMode::TagOnly`] on the reference plugin: every output sample is
    /// a nonzero per-`(port, channel)` tag, independent of input.
    ///
    /// Mirrors `tutti_clap_test_plugin::RenderMode`; kept as a bare constant
    /// because the value crosses a `dlopen` boundary as a `u32`.
    #[cfg(feature = "clap")]
    pub const CLAP_PROBE_RENDER_TAG_ONLY: u32 = 2;

    /// Set the reference plugin's render mode.
    ///
    /// The probe defaults to `Inert` — it writes nothing — so a test that
    /// asserts audio *came out* has to opt in. It is an effect, not a synth:
    /// there is no note-to-audio path to exercise, and `TagOnly` is what makes
    /// "the host collected this plugin's output at all" observable.
    ///
    /// Reaches the plugin through its already-loaded `dlopen` handle rather
    /// than the `rlib`: the running instance's `RENDER_MODE` static is the one
    /// in the *dynamic library*, and a value stored through the rlib's copy
    /// would be a different static that `process` never reads.
    #[cfg(feature = "clap")]
    pub fn set_clap_probe_render_mode(mode: u32) {
        clap().set(ClapSwitch::RenderMode, mode);
    }

    /// What a [`Restore`] puts back.
    #[cfg(feature = "clap")]
    enum Reset {
        /// A switch, to its default.
        Switch(ClapSwitch, u32),
        /// The plugin's pending thread command.
        ThreadCommand,
        /// The plugin's pending params command.
        ParamCommand,
        /// The plugin's refusal of an activation rate.
        ActivateRefusal,
    }

    /// The one guard of this module: puts the reference plugin back to its
    /// default when it drops. Every helper below that changes the plugin until
    /// the returned guard drops returns this.
    #[cfg(feature = "clap")]
    #[must_use = "the plugin goes back to its default when the guard drops"]
    pub struct Restore(Reset);

    #[cfg(feature = "clap")]
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.0 {
                Reset::Switch(switch, default) => clap().set(switch, default),
                Reset::ThreadCommand => {
                    clap().thread_command(tutti_clap_test_plugin::CMD_NONE);
                }
                Reset::ParamCommand => {
                    clap().param_command(tutti_clap_test_plugin::PARAM_CMD_NONE);
                }
                Reset::ActivateRefusal => clap().set_activate_refusal(u64::MAX),
            }
        }
    }

    /// Sets `switch` to `value` until the returned guard drops, which sets it
    /// to `default`.
    ///
    /// Reaches the plugin through its already-loaded `dlopen` handle rather
    /// than the `rlib`: the running instance's statics are the ones in the
    /// *dynamic library*, and a value stored through the rlib's copy would be a
    /// different static the plugin never reads.
    #[cfg(feature = "clap")]
    pub fn clap_switch_until_drop(switch: ClapSwitch, value: u32, default: u32) -> Restore {
        clap().set(switch, value);
        Restore(Reset::Switch(switch, default))
    }

    /// [`set_clap_probe_render_mode`] until the returned guard drops, which
    /// puts the plugin back to `Inert`.
    #[cfg(feature = "clap")]
    pub fn clap_probe_render_mode(mode: u32) -> Restore {
        clap_switch_until_drop(
            ClapSwitch::RenderMode,
            mode,
            tutti_clap_test_plugin::RenderMode::Inert as u32,
        )
    }

    /// Select the reference plugin's `process` status
    /// (`tutti_clap_test_plugin::StatusMode`) until the returned guard drops,
    /// which restores `Continue`. `Error` makes every block fail inside the
    /// plugin, a failure of the plugin's own and not of the block's shape.
    #[cfg(feature = "clap")]
    pub fn clap_probe_status_mode(mode: tutti_clap_test_plugin::StatusMode) -> Restore {
        clap_switch_until_drop(
            ClapSwitch::StatusMode,
            mode as u32,
            tutti_clap_test_plugin::StatusMode::Continue as u32,
        )
    }

    /// Select the reference plugin's audio-port layout
    /// (`tutti_clap_test_plugin::PortLayoutMode`) until the returned guard
    /// drops, which restores `SymmetricStereo`. Call before loading: the host
    /// reads the layout once, at load.
    #[cfg(feature = "clap")]
    pub fn clap_probe_port_layout(mode: u32) -> Restore {
        clap_switch_until_drop(
            ClapSwitch::PortLayout,
            mode,
            tutti_clap_test_plugin::PortLayoutMode::SymmetricStereo as u32,
        )
    }

    /// Make the reference plugin's ports on the sides in `mask` advertise
    /// `CLAP_AUDIO_PORT_SUPPORTS_64BITS` (`tutti_clap_test_plugin::PORTS_64BIT_*`)
    /// until the returned guard drops, which restores the f32-only default.
    /// Call before loading: the host reads the port flags once, at load.
    #[cfg(feature = "clap")]
    pub fn clap_probe_ports_64bit(mask: u32) -> Restore {
        clap_switch_until_drop(ClapSwitch::Ports64Bit, mask, 0)
    }

    /// Make the reference plugin call `audio-ports.rescan(flags)` at its next
    /// `on_main_thread` (see `tutti_test_plugin_request_ports_rescan`) until
    /// the returned guard drops, which clears a call not yet made.
    #[cfg(feature = "clap")]
    pub fn clap_probe_request_ports_rescan(flags: u32) -> Restore {
        clap_switch_until_drop(ClapSwitch::RequestPortsRescan, flags, 0)
    }

    /// Make the reference plugin call `host.request_restart()` from its next
    /// `process`, until the returned guard drops (which clears a call not yet
    /// made).
    #[cfg(feature = "clap")]
    pub fn clap_probe_request_restart() -> Restore {
        clap_probe_thread_command(tutti_clap_test_plugin::CMD_REQUEST_RESTART)
    }

    /// Make the reference plugin run `cmd` (`tutti_clap_test_plugin::CMD_*`)
    /// at its next legal call site until the returned guard drops, which
    /// clears one not yet run: `on_main_thread` for the timer commands,
    /// `process` for the restart ones.
    #[cfg(feature = "clap")]
    pub fn clap_probe_thread_command(cmd: u32) -> Restore {
        clap().thread_command(cmd);
        Restore(Reset::ThreadCommand)
    }

    /// Make the reference plugin call `host.params.rescan` as
    /// `tutti_clap_test_plugin::PARAM_CMD_*` says at its next `on_main_thread`,
    /// until the returned guard drops, which clears one not yet made.
    #[cfg(feature = "clap")]
    pub fn clap_probe_param_command(cmd: u32) -> Restore {
        clap().param_command(cmd);
        Restore(Reset::ParamCommand)
    }

    /// Make the reference plugin's latency `extra` frames longer from its next
    /// activation on (see `tutti_test_plugin_set_latency_at_activation`), until
    /// the returned guard drops. The plugin does not call `changed()`.
    #[cfg(feature = "clap")]
    pub fn clap_probe_latency_at_activation(extra: u32) -> Restore {
        clap_switch_until_drop(ClapSwitch::LatencyAtActivation, extra, 0)
    }

    /// Make the reference plugin call `latency.changed()` from inside every
    /// `activate`, until the returned guard drops.
    #[cfg(feature = "clap")]
    pub fn clap_probe_latency_changed_in_activate() -> Restore {
        clap_switch_until_drop(ClapSwitch::LatencyChangedInActivate, 1, 0)
    }

    /// What the reference plugin recorded of its threading and timer calls
    /// since the last reset.
    #[cfg(feature = "clap")]
    pub fn clap_probe_thread_capture() -> tutti_clap_test_plugin::ThreadCapture {
        clap().thread_capture()
    }

    /// Zeroes [`clap_probe_thread_capture`].
    #[cfg(feature = "clap")]
    pub fn clap_probe_thread_reset() {
        clap().thread_reset();
    }

    /// How many times the reference plugin accepted `activate` since
    /// [`clap_probe_reset_activation_count`]: one per activation, so a
    /// re-activation shows as one more.
    #[cfg(feature = "clap")]
    pub fn clap_probe_activation_count() -> u32 {
        clap().activation_count()
    }

    /// Zeroes [`clap_probe_activation_count`].
    #[cfg(feature = "clap")]
    pub fn clap_probe_reset_activation_count() {
        clap().reset_activation();
    }

    /// Make the reference plugin's `activate` refuse `rate` until the returned
    /// guard drops, which lifts the refusal.
    #[cfg(feature = "clap")]
    pub fn clap_probe_refuse_rate(rate: f64) -> Restore {
        clap().set_activate_refusal(rate.to_bits());
        Restore(Reset::ActivateRefusal)
    }

    /// The rate of the reference plugin's most recent accepted `activate`
    /// since [`clap_probe_reset_activation_count`], if any.
    #[cfg(feature = "clap")]
    pub fn clap_probe_last_accepted_rate() -> Option<f64> {
        clap().last_accepted_rate()
    }

    /// Make the reference plugin withhold `clap.tail` until the returned guard
    /// drops, which restores it. Call before loading: the host asks for the
    /// plugin's extensions once, at load.
    #[cfg(feature = "clap")]
    pub fn clap_probe_without_tail() -> Restore {
        clap_switch_until_drop(ClapSwitch::TailExtension, 0, 1)
    }

    /// Absolute path to `tutti-clap-test-plugin`, the reference CLAP cdylib the
    /// loader tests drive.
    ///
    /// The reference plugin is a dev-dependency built by the same `cargo test`
    /// run, so the suite works on any machine.
    ///
    /// Resolution (newest candidate wins) and the panic-on-absence rule are
    /// `tutti_fixtures`'s; see that crate for why each matters.
    #[cfg(feature = "clap")]
    pub fn clap_probe_path() -> &'static str {
        static RESOLVED: OnceLock<String> = OnceLock::new();
        RESOLVED
            .get_or_init(|| {
                tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin")
                    .to_string_lossy()
                    .into_owned()
            })
            .as_str()
    }

    /// Acquire the plugin-load lock, recovering from poisoning.
    ///
    /// These tests load real plugins, and a single failing
    /// assertion panics while holding the lock — poisoning it. Without
    /// recovery, every *other* plugin test then fails with `PoisonError`,
    /// turning one real failure into dozens of misleading cascade failures.
    /// The guarded data is `()`, so there's no invariant a poisoned lock
    /// could violate; recovering is safe and keeps failures isolated.
    pub fn plugin_load_lock() -> MutexGuard<'static, ()> {
        PLUGIN_LOAD_LOCK.lock()
    }
}
