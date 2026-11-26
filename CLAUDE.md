# CLAUDE.md

Rules and commands for working in tutti. What an item does is in its rustdoc; read the
crate docs of what you touch. The reasoning behind a rule is in
[`docs/engineering-notes.md`](docs/engineering-notes.md); the library style is
[`docs/style-guide.md`](docs/style-guide.md).

## Commands

`cargo xtask <recipe>` wraps everything (`cargo xtask help`; `--dry-run` prints the
commands). `cargo xtask ci` runs what CI runs. `cargo xtask gates` (rustfmt, xtask's
tests and the source gates, seconds and no build) is what CI fails fastest on: run it
before a push, or once per clone `git config core.hooksPath .githooks` to have
`.githooks/pre-push` run it.

`cargo xtask deslop` runs deslop-please's architectural lints (module cycles, tangles,
field leaks, shallow modules, wide directories, long files) against `xtask/deslop.allow`,
the findings on main. A PR adds no finding. Fix one by asking what its readers need and giving
them that, never with a getter or a mechanical move. A PR that removes a finding, or changes
a cycle's members, rewrites the list with `cargo xtask write-deslop-allow`, and its diff of
the list adds no line it does not explain. It builds the workspace, so it is not in `gates`.

```bash
cargo build
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt
cargo nextest run --workspace              # not `cargo test`: nextest isolates each test
cargo nextest run -p tutti-engine -E 'test(name)'
cargo test --doc --workspace               # nextest does not run doctests
cargo xtask test-all                       # every test tier CI runs
cargo xtask doc                            # rustdoc as CI runs it
cargo xtask check-loom                     # clippy under --cfg loom
cargo xtask rtsan                          # RealtimeSanitizer over the FFI/plugin paths; local only, needs curl
cargo xtask check-windows                  # Windows lints from Linux
cargo xtask test-wasm                      # the web suite under node (needs `wasm-bindgen-cli` at Cargo.lock's version)
cargo xtask check-wasm                     # clippy for wasm32-unknown-unknown
cargo xtask deny                           # needs cargo-deny 0.20.2 (`cargo install --locked`)
cargo xtask deslop                         # needs deslop-please 0.1.4; a finding not in xtask/deslop.allow fails
```

The static gates (`cargo xtask check-*`) are Rust in `xtask/src/gates/`: a pure function
from the files, read through `Tree`, to a `Report`, tested over in-memory trees. xtask is
std only. A rule the compiler already enforces is not a gate.

- **Toolchain:** `rust-toolchain.toml` pins it and `Cargo.lock` is committed (`--locked`).
  A bump is its own PR that fixes the new lints.
- **Setup:** `git submodule update --init --recursive` (the VST3 SDK under
  `vendor/vst3-sdk/`; the gate asserts SHAs). Linux needs ALSA headers (`libasound2-dev
  pkg-config`).
- **Windows:** `cargo xtask check-windows` lints for MSVC from Linux; CI's `clippy
  (windows)` lints natively and also runs `tutti-winmm-ffi`'s and `tutti-midi-hardware`'s
  tests there, `--features synth-tests` included, and `tutti-plugin-ipc-ffi`'s and
  `tutti-plugin-ipc`'s `transport::` tests (the contract's `contract (windows)` job is not
  required; no other job runs tests on Windows).
  The synth suites open the GS Wavetable Synth, which needs an audio endpoint: the job
  installs a virtual audio driver first (`.github/scripts/install-virtual-audio.ps1`).
  A vst3 SDK enum constant
  is `c_uint` on unix and `c_int` on Windows: cast it with `helpers::sdk_enum_i32`. Read
  the engineering notes before touching `tutti-plugin-ipc`'s transport or plugin loading.
- **Review:** every PR is reviewed by someone other than its author against
  CONTRIBUTING.md's checklist before it is queued; the review is recorded in its
  description. A reviewed PR with CI green and no open blocking finding is queued by its
  author.
- **Stacked PRs:** a PR may sit on an open PR's branch when it needs that unmerged work
  (three deep at most; each piece builds, passes and stays under the size limit against
  its parent). Auto-merge is armed only once its base is main. Before queueing, merge
  `origin/main` in, check `git diff origin/main...HEAD` shows only its own change, rerun
  the gates and write baseline counts from the gate, never by picking a side. A parent
  fix is made in the parent and merged up. Never delete a parent's branch by hand.

## Testing

- **Build `plugin-server` and `plugin-probe` first** (`cargo build -p tutti-plugin-server
  -p tutti-plugin-probe`). A suite that spawns one fails with that command when it is
  missing; it never skips.
- **A test finds a repo file through `tutti_fixtures`** (`asset`, `repo_root`, `cdylib`,
  `plugin_server`, `plugin_probe`), never a relative `..` path. Absence is a failure.
- **A feature-gated suite runs only where its feature is on.** CI's dark features job runs
  its listed crates with `--all-features`; a crate whose suite a feature gates joins that
  `-p` list and `cargo xtask test-features`, or it never runs.
  `tutti-midi-hardware --features sequencer-tests` needs the kernel's `snd-seq`; CI only
  lints it, so run it locally before changing `backend/alsa`.
- **A test pins an analytic figure, a spec value or an invariant**, never a digest of
  tutti's own output. A comparison with an external implementation is a one-off
  validation, never a test.
- **One integration binary per area**: `tests/<area>/main.rs`, one `mod` per file. An area
  binary installs `tutti_fixtures::RtAllocCheck` once: any allocation or free inside a
  `tutti_rt::RtScope` aborts the test with its name. A render prepared `Realtime` (the
  executor, `Direct`, `drive_input`) runs inside one, so every realtime render test is an
  allocation test; a render prepared `Offline` may block and allocate. A file is its own
  binary only for a different allocator (counting, no-free), `harness = false`, or
  `cfg(loom)`.
- **A test must be able to fail.** It asserts a spec property, an analytic figure or an
  invariant, and its name says which. Delete a test that cannot fail: one that is always
  true, asserts only that nothing panicked, or compares a value with the function that
  computed it.
- **A test never asserts wall-clock time.** A timing rule (deadline, stall, timeout,
  debounce) is a type whose methods take the `now`, and its test passes constructed
  instants. A real-time limit in a test only guards a hang: generous, asserting
  completion, never speed. A sleep never stands in for waiting on the event. Speed is a
  bench.
- **Never weaken a test to make it pass.** Fix the bug, or report the failure and its
  cause.
- **Never compare a render against another render.** Pin an analytic figure or an
  invariant. No migration tests (old against new) and no test-only seams.
- The spec is the oracle: nothing is pinned against another implementation.

## Code rules

### The graph

- Every node is a `tutti_graph::Node` (`shape`, `prepare`, `process`, `reset`). A node with
  params also implements `ParamNode`, declares them with `tutti_graph::declare_params!`
  next to its `Param<U>` fields, is inserted as `Params(node)` (never `ForkByClone`), and
  reads a driven param through the declaration (`declared_param`), never by port index. A
  plain node is `ForkByClone` or `Unforkable`. The crate docs' "Writing a node" is the
  guide.
- A node is `Node<F>` for any `F`. It reads params as `&[F]` lanes or `F::from_unit` cells,
  and its state is `F`. An f32 graph and an f64 graph never mix except through
  `Bridged<G, N>`.
- `Shape::tail` is a ceiling. A node whose ring-out follows its params narrows it per block
  with `Io::report_tail`, covering the ring-out and erring long. A finite ceiling past
  `MAX_NODE_TAIL` counts as unbounded; nothing clamps it.
- Musical delay is not latency: report only processing latency.
- Nodes communicate only through ports. Two nodes sharing mutable state outside the graph
  are unordered on the parallel executor: share through an edge, or make it one node.
- A cheap node declares its cost (`Shape::cost`); one that declares none is `Heavy`.
- Op code reaches the arena, event slots, units, rings and outputs only through
  `AudioSlots`/`EventSlots` or a claim, which keeps `tutti-graph` `#![forbid(unsafe_code)]`.
- A render is bit-identical at any participant count; the plan verifier proves schedule
  independence, and one test compares 1 against N participants bit for bit.
- Never make memory safety depend on an exact float result (nodes run inside
  `with_no_denormals`).
- A node never latches a NaN or an infinity from its input: a block that wrote one is
  passed on and the recursive state resets after it (`KernelNode` does this for a kernel).
- A node that changes width declares its buses; the graph never guesses a dry path. A node
  reads a non-main bus through the handle its shape issued.
- Summing is the graph's: several sources into one port sum. Never add a node to sum.
- `GraphSpec` holds structure only. A node's mode and mix, an edge's gain base, range and
  fade, and a param's range are values the editor holds (`Editor::set_*`); a field added
  to the spec is structure, or it goes to the editor's table.
- A transport command meant for playback takes an `At`. A node reads the transport at a
  frame through `Env::transport_at`; a block is never split for it.

### The session

- `Graph::add` is the one way a node enters, and keys are typed (`Key<N>`). Wiring is
  `Graph::connect(from, to)` over ports taken from keys; params are methods of the key.
- **One failure rule:** a call that names a node or port that does not exist fails at the
  call with a named error, never later and never as a default. A port that goes missing
  after a successful call reads silence, except a tap whose every source was removed:
  it ends (`Pull::Ended`) once drained. Removal returns a `#[must_use] Result`.
- The session graph declicks wiring (`connect` fades in unless `.unfaded()`); a test that
  renders a session wire for another reason wires `.unfaded()`.
- An inlet whose rate is not the graph's is refused, never resampled.
- An app reads the playhead through `Session::playhead()`.
- An app writes a value one way: `Session::set` (or `Values::set`) over an assignment built
  from a target, and reads it with `get`; a new `pub fn set_*` on the session is refused
  (`cargo xtask check-values`, whose baseline only shrinks).
- A node's values are targets on its key: `Graph::add` returns the key alone, and an app
  never holds a node's cells. The sampler's handles are `Graph::handle` until their
  commands are targets.
- A main-thread method takes `MainThreadMarker`. No public type in `tutti-session` holds one, no `pub fn` is named `*_blocking` and no borrowing `pub fn run` (a work value's `run(self ..)` is the app's call), bar the rows of `xtask/session-api-allow.txt` (`cargo xtask check-session-api`).

### The audio thread

- Never allocate, free, lock or wait on the audio thread. A full queue is counted, never
  waited on.
- A collection the audio thread fills is a `tutti_rt::CappedVec`, its cap set on the
  control side when it is built (or an inline `tutti_rt::RtVec<T, N>`); a push past it is
  refused and counted. Never a `Vec` or
  `VecDeque` whose room a `with_capacity` or `reserve` elsewhere promises. An owned item a
  full collection refuses comes back to the caller (`push_within_capacity`), never drops
  there. Test nodes follow the same rule.
- The audio thread never calls `tracing`, `log` or a print macro: a subscriber may
  allocate, lock or write a file. What it reports is a count an app can act on, and what
  it logs goes only through `Io::log`.
- Non-scalar state reaches the audio thread through `tutti_rt::{Publisher, Reader, Loan}`;
  scalars use `Param<U>`. Nothing the audio thread reads is an `ArcSwap`.
- The audio thread never holds an owning handle to published state and never frees it.
  `publish` and `reclaim` are control-thread only.
- Register a `Reader` on the control side where the reader is built, and store it in the
  node. Lend once per block (once per segment where a commit can install mid-block),
  never per sample, and never keep a `Loan` across blocks.
- Every cell's publisher calls `reclaim()` on a control-side tick it already has; a new
  cell joins its owner's sweep in the same change.

### Work off the audio thread

- The app runs the work it asks for. A load, an analysis, a scan or a bounce is an inert
  value the app runs (`run(self)`, or `run(self, main)` where the OS needs the main
  thread). A wait for a device's or the plugin server's answer is a std `Future` that
  tutti's input or IPC path fills; a wait that ends only on a time is a value with
  `poll(.., now)` and `deadline()`. tutti offers no blocking call beside these.
- tutti runs only the work that keeps pace with the audio thread or the hardware (the
  streamer, a take's pump, OS edges, sensing, the bells' tick), on the session's own
  executor, which the app never sees. A blocking OS wait or a priority loop is
  `Executor::spawn_dedicated`; a future keeps a `Timer`, never an `Executor`.
- Nothing else starts an OS thread: `clippy.toml`'s `disallowed-methods` refuses
  `std::thread` starts and `tutti_platform::spawn_named`. The starts that stay (the
  platform's own, the graph's render workers, the engine's deviceless clock, the CoreMIDI
  hub) carry an `#[expect]` with their reason; test code allows the lint at its root.

### Wanted and applied

A value one side wants and something else carries to where it takes effect (a plugin
parameter, a setting, a staged session value, a request to a device) follows one pattern,
stated where the value is declared:

- **Wanted** is one record on the writer's side, read without asking the target. A read
  answers wanted, else applied.
- **The carrier follows the value's kind.** A level goes in a latest-per-key carrier
  (`tutti_rt::SeqSlotsWriter` and its reader, or a `DirtySet` over cells). A command or a
  timed stream goes in an ordered queue that refuses at the call by name or counts what it
  drops. A carrier never waits.
- **Applied**, where something reads it, is a `tutti_rt::Generation` taken at the call and
  compared with `has_reached`, exact while the gap is under 2^31. The holder of the reader
  publishes it after the target's own call returns; it is never read off the carrier. A
  reply that carries a value names the ask it answers. A value whose applied state nobody
  reads keeps only an owed mark until it lands.
- **A loss is counted where it happens, and named.** A level the carrier lost is owed and
  sent again. A level the target refused is given up with a named notice. An answer that
  says a write was applied never covers one that was lost. A reset ends an epoch stored
  beside the value; nothing from an earlier epoch is sent again.
- **A read or a save states what is not yet applied** beside its result, and a load replays
  it. It never waits for a carrier: a synchronous entry, a fork or an undriven graph has
  none.

### Unsafe and runtime structure

- A library with no `unsafe` carries `#![forbid(unsafe_code)]` (`cargo xtask check-ffi`).
- Plugin FFI `unsafe` lives only in `tutti-clap-ffi`, `tutti-vst3-ffi`, `tutti-au-ffi` and
  `tutti-vst2-ffi`; the process edge's in `tutti-plugin-ipc-ffi`; MIDI's OS calls in
  `tutti-alsa-ffi`, `tutti-coremidi-ffi` and `tutti-winmm-ffi`; the OS calls (clock, thread
  priority, main-thread detection, denormal control) in `tutti-platform`. Everything above
  them forbids `unsafe`. Test-only `unsafe` is `tutti-fixtures`'.
- Time, threads, thread priority, the main thread and denormal control come from
  `tutti-platform`; it names no tutti crate and holds only what needs a platform `cfg`.
- Name `Instant` and `SystemTime::now` through `tutti_platform`; std's panic on wasm
  (`cargo xtask check-platform`).
- New lock-free or `unsafe` runtime structure goes in `tutti-rt`, with a loom model that
  runs the shipped code. The publish cell is the `arc-lend` crate from its own repository
  (github.com/tutti-audio/arc-lend); a change to it is made there.
- Store-load pairs use `SeqCst` fences, not `SeqCst` accesses.
- A platform difference sits behind one seam: the `cfg` chooses a platform module, never
  each item.

### Shared types

Every crate uses these; a hand-rolled copy is replaced by the shared one when its file is
touched.

- A reading the audio thread writes and any thread reads is a `tutti_rt::Gauge<U>` (latest
  value) or `Counter<U>` (a cumulative count that saturates); a count with many writers is
  a `Tally<U>`. A count of lost events is a `Counter<Losses>` (`tutti_units::Losses`).
- A refusable push returns `tutti_rt::PushError { Full, Disconnected }`.
- A thread's wake-up is `tutti_rt::parker()`: any number of `Unparker`s unpark one
  `Parker`. A future's wake-up is a `tutti_rt::Bell` from the session's `Bells`: the audio
  thread rings it, and the future awaits its `Listener::rung()`. An app never names either:
  a session's tap carries one, and the app awaits `TapReader::notified()`.
- A value the audio thread must never drop holds a `tutti_rt::ControlDrop`.
- A lock is `tutti_rt::{Mutex, RwLock, Condvar}`: it recovers a poisoned lock and, in a debug
  build, panics on a wait inside an `RtScope`. `clippy.toml`'s `disallowed-types` refuses
  std's; a crate below `tutti-rt` or a test plugin allows it at its root with the reason.
- A collection the audio thread fills is a `tutti_rt::RtVec` or `CappedVec`: capped at
  construction, refusing past it, never growing. A value of more than one field read as
  one word is an `AtomicCell`; an atomic float is `atomic_float::AtomicF32`/`AtomicF64`.
- A value the audio thread reads whole is a `tutti_rt::AtomicCell<T>` over an `AtomicWord`
  (`atomic_word!`); a node declares it in `declare_params!`'s `settings` block.
- An id type is written with `tutti_rt::id!` and minted from a `tutti_rt::Mint`.
- A parameter's id is a `tutti_units::ParamId`; a channel's index is a
  `tutti_buffers::ChannelIndex`; a meter's attack and release are a `tutti_units::Ballistics`.
- Time and layout have one type each: `tutti_music::LoopRange` for a loop,
  `TimeSignature` and `MeterMap` for meter, `tutti_tempo::TempoMap` for tempo,
  `tutti_buffers::{ChannelLayout, ChannelTopology, ChannelSet}` for channels, and
  `tutti_curve::Trajectory<U>` for an automation curve.

### Unit types

- Use a `tutti_units` unit, never a bare float, for any quantity one covers: fields,
  signatures, `Param<U>`, returns.
- A unit's operators are its whole algebra. Read the omission ledgers in `tutti-units`'
  `units/` tests before adding one, and ship a replacement with any omission.
- A number becomes a unit with `new` and leaves with `get`; there is no `From` between a
  unit and its raw. Convert between units with the named converters.
- Measurements are not controls: `Confidence`, `Correlation` and `StereoBalance` are readings.
- Types stop at C ABI, IPC and wire boundaries; say why in a comment there.
- Add a unit only for a distinct range or a distinct algebra.
- A quantity has one width: continuous units are `f64`, positions and counts are integers.
  Data is generic over the graph's `F`. A quantity becomes data only through `F::from_unit(u)`,
  `F::lit(x)` for a literal, a kernel's `Design::of` (a figure it computes, a coefficient or
  pole, crosses with `F::from_design`), or the graph's lane render;
  `cargo xtask check-width` holds it.
### Range-checked values

- A range-checked type has the `MidiChannel` shape: `new(raw) -> Option` (never clamps),
  a const `of` that panics for literals, `get` and `From<Self> for Raw`, `TryFrom<Raw>`
  agreeing with `new`, serde through `try_from`, and named `clamped`/`masked` only where a
  hot path or the wire needs them. No public field, no infallible `From<Raw>`. Test it with
  `tutti_fixtures::check_range_contract`.
- Write a new range-checked type with `tutti_units::ranged!`, never by hand or with a
  crate-local macro; a hand-written one moves to it when its file is touched.
- The `ranged!` raw for a continuous range is `f64`; an integer range stays its integer raw
  (MIDI words, counts).
- Make a wrong value unwritable before adding a runtime check for it.

### Edges and buffers

- `Source`/`Sink` are edges over a `Sample` format: a checked `Interleaved`/`InterleavedMut`
  view, a runtime `ChannelLayout`, every count in frames (`Samples`). Cross to a slice
  length only with `ChannelLayout::interleaved_len` / `from_interleaved_len`.
- Audio in a public API is a `tutti_buffers` view (`Mono`, `Interleaved`, `Planes`),
  generic over `Sample`/`Float` with the f32 default; a fixed format is a row in
  `xtask/buffers-fixed.txt` with its reason (`cargo xtask check-buffers`).
- Interleave, deinterleave, convert and fold go through `tutti_buffers` (`interleave`,
  `deinterleave`, `convert`, `Fold`), never a hand loop over frame and channel indices.
- Width is runtime: never a const-generic channel count. A channel fold goes through
  `tutti_buffers::Fold`, built on the control side; a narrowing by index is refused, never
  truncated.
- Every `Source::pull` answers `Starved` (not yet) or `Ended` (never again) correctly, and
  `Failed` only after the frames before it.
- A width change between data is the user's: `Bridged` in a graph, std `From`/`as` on app
  buffers, the decode width chosen at load. tutti never converts float data on its own; a
  plugin that can't take `F` is an error carrying the plugin.
- A copy out of the graph is a tap (`TapReader`, `EventTapReader`), a copy in is an inlet.
  Both count what they lose and never block.

### Plugins

- A main-thread entry takes a `tutti_plugin_types::MainThreadMarker`; holding one is the proof.
- A format host is a `Loaded` typestate whose `activate` splits a plugin into a realtime
  `Processor` and a main-thread `Controller`.
- The plugin-server is one `Server<F: OutOfProcess>`; VST2 is not hosted out of process.
- A plugin load, state save or restore, and preset load is a value split by where the plugin
  runs: `InProcess` runs on main (`run(main)`), `OutOfProcess` is a `Send` future the IPC
  reply fills. A reload is the app's composition (`Session::swap_plugin`'s "Reloading").
- `tutti_plugin_types::bundle` is the only code that knows a bundle's layout. Build a VST3
  UID with `inline_uid`.
- **The plugin contract** (`testing/tutti-plugin-contract`) runs the same cases against
  every format with the one test plugin, `testing/tutti-shared-plugin`, and its C++ VST3
  twin. Expected figures live in the contract's `spec`, never asked of the plugin. The
  plugin records what the host sent before it orders or repairs anything, and is
  configured only through its state. A case a format cannot pass is a `Gap` that must fail
  with its text; `gaps = N` is a ratchet. Never `#[ignore]`.
- Every host callback a plugin can call (an `extern "C" fn`, a COM method, the VST2
  audioMaster) runs its body under the panic guard, and a CLAP `[main-thread]` callback
  under `main_only`: a panic out of one aborts the host (`cargo xtask check-guards`).

### Style

- Import from a crate's root, never through its modules. There are no preludes, and no
  crate re-exports another crate whole.
- An umbrella holds no logic: a piece with one owner goes to that crate, a piece composing
  several goes to `tutti-session`.
- A pure algorithm over `tutti_buffers` views goes in `tutti-dsp` (a measurement in
  `tutti-measure`); a node file holds only the `Node` adapter.
- A new trait states its boundary in one sentence without "and".
- A method that answers "not for this kind" (`None`, `false`, ignored), or an `unreachable!`
  that says two tags agree, means two kinds share a type: see `docs/style-guide.md` T19
  before adding a tag, and before splitting.
- A configured thing is `Type::new(required..)` plus chained options (`self`, a bare
  name, `-> Self`), never a struct of public fields filled with `..Default::default()`;
  a separate builder with `build()` only where finishing does work or can fail.
  See `docs/style-guide.md` T5; `cargo xtask check-building` holds it.
- An error type derives `thiserror::Error` with its text in `#[error(..)]`; a hand
  `impl Error for` is a row of the gate's allow-list with its reason (`cargo xtask
  check-errors`), and a `pub` type named `...Error` implements `Error`.
- Logging is `tracing`: a library never installs a subscriber, and nothing prints
  (`println!`/`eprintln!`) outside a `main`; `cargo xtask check-logging` holds it.
- Naming: an algorithm uses the theory name, a spec keeps its own, a *frame* is one value
  per channel, a *sample* is one value. A user-facing node API keeps the control word.
- A comment states what the code does and the invariant it holds, in the present tense;
  rustdoc documents the item. No history, no "why it was added", no removed names, no
  design-doc citations. A comment or doc that disagrees with the code is a bug.
- Prose outside `docs/` never cites a line (`file.rs:N`): it states the logic or the
  invariant itself; a pointer in `docs/` names a file that has that line (`cargo xtask
  check-pointers`).
- A public fieldless enum that is `Eq` also derives `Hash` (`cargo xtask
  check-value-traits`).
- Before working around a constraint, name it in one sentence and verify it (read the
  manifest, grep, run the probe). An indirection whose only job is to avoid a change is
  the finding: make the change.
- **One entry per fact.** A fact something follows (a render mode, a rate, a tempo or loop
  reading, a plan, a value) reaches it through one entry: one trait method, hook, setter,
  phase or handle. Needing the fact at a new place, time or thread widens that entry (its
  argument, its view, its callers), never adds a second beside it: `bind_tempo` plus a
  `bind_loop` is one `bind_timeline(&TimelineView)`, and a prepare-time hook that tells a
  plugin the render mode joins the call that already tells it. Before adding a trait
  method, hook, phase, setter or handle field, name the fact it carries and grep for what
  delivers it already. Two entries each correct alone still disagree in order and default.

## Workspace

One workspace at the root; the `members` list is the source of truth. `crates/` is what
ships (directory name = package name, no nesting), `testing/` is never shipped, `xtask/`
holds the development commands (std only, no dependencies, `publish = false`), and
`vendor/` holds the VST3 SDK, which is never edited. No crate depends on Bevy; the Bevy
adapter is [tutti-audio/bevy-tutti](https://github.com/tutti-audio/bevy-tutti).

**Layering.** Normal and build dependencies point strictly down this list
(dev-dependencies may go anywhere); `cargo xtask check-layout` holds it:

0. `tutti-platform`
1. `tutti-units`, `tutti-plugin-ipc-ffi`
2. `tutti-music`, `tutti-buffers`
3. `tutti-ump`, `tutti-tempo`
4. `tutti-alsa-ffi`, `tutti-coremidi-ffi`, `tutti-winmm-ffi`, `tutti-midi-protocol`,
   `tutti-midi-ci`, `tutti-curve`
5. `tutti-rt`, `tutti-midi-device`
6. `tutti-midi-network`, `tutti-graph`, `tutti-dsp`, `tutti-modulation`, `tutti-voicing`,
   `tutti-sf2`
7. `tutti-timeline`, `tutti-plugin-types`, `tutti-measure`, `tutti-codec`
8. `tutti-engine`, `tutti-nodes`, `tutti-midi-runtime`, `tutti-midi-file`, `tutti-clap-ffi`,
   `tutti-vst3-ffi`, `tutti-au-ffi`, `tutti-vst2-ffi`, `tutti-plugin-db`, `tutti-plugin-ipc`
9. `tutti-cpal`, `tutti-au-host`, `tutti-export`, `tutti-sampler`, `tutti-polysynth`,
   `tutti-soundfont`, `tutti-midi-hardware`, `tutti-clap-host`, `tutti-vst3-host`,
   `tutti-vst2-host`, `tutti-plugin-host`
10. `tutti-plugin`, `tutti-plugin-probe`
11. `tutti-plugin-server`, `tutti-session`
12. `tutti`

- Nothing under `crates/` has a normal or build dependency on `testing/`.
- Only `tutti` depends on `tutti-session`, and nothing depends on `tutti` but a
  `testing/` crate that reads it as an app does, through a dev-dependency
  (`tutti-book`, the book's programs).
- A crate keeps its modules private and is named from its root; `check-paths` enforces it.
- `check-layout` also holds each crate's limits on its own dependencies: which workspace
  crates it may name, and which modules alone may name a wrapped library (`symphonia` in
  `tutti-codec`'s decode module, `winnow` in the listed parser modules, `web_time` and
  `thread_priority` in `tutti-platform`).
- A codec format is a cargo feature (`encode-*`, `decode-*`), none on by default; a decode
  feature pulls no encoder.
- A MIDI OS backend is a cargo feature of `tutti-midi-hardware` (`alsa`, `coremidi`,
  `winmm`), each a no-op off its OS and forwarded by `tutti-session` and `tutti`; the
  defaults keep every platform's backend, and `default-features = false` is the stub.
