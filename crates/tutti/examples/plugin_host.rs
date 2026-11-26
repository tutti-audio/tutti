//! A plugin host, end to end: scan, pick, load, wire, play, set a parameter,
//! save and restore its state, load a preset, read its health, bounce.
//!
//! ```text
//! cargo build -p tutti-plugin-server -p tutti-plugin-probe
//! cargo run -p tutti --features clap,export --example plugin_host -- \
//!     "Plugin Name" out.wav /path/to/plugin/dir [more dirs...]
//! ```
//!
//! The scan puts every plugin it finds in a [`Catalog`]; the catalog says what
//! exists and what is blacklisted, and the session does the loading; the one whose name is
//! `argv[1]` becomes a [`PluginRef`] (its file *and* its id in it, so a bundle
//! that exports several loads the one that was picked); the session loads it,
//! hosts it, and bounces four beats of it to `out.wav`. An instrument plays a
//! note; the plugin's first parameter is set and dragged (`params()`); its state is saved with
//! `session.save_state(..)`, written beside the WAV and restored.
//!
//! Work is a value the app runs. A load, a state or a preset is `InProcess` (a
//! plugin that lives in this process: `run(main)` on the main thread) or
//! `OutOfProcess` (a plugin in a `plugin-server` process: a `Send` future the
//! server's reply fills). This app awaits the second kind with
//! `futures_lite::future::block_on`; a host with a frame loop hands it to its own
//! executor.
//!
//! The plugin is a [`Plugin`](tutti::Plugin) view (`session.plugin_view(node)`), and what happens to
//! it is a [`PluginEvent`] in `update()`'s events. The host makes the token on its
//! main thread and calls `session.update()`, which services the
//! editors it opened (`session.editor(node, &main)?.open(&window, scale)`, with a
//! window of a winit app; this host has none); a size an editor asks for is
//! `editor.requested_size()`.
//!
//! It is not run in CI (it needs a plugin directory and the two binaries
//! above built); `cargo build -p tutti --features clap,export --examples`
//! compiles it.

use std::future::IntoFuture as _;
use std::path::PathBuf;

use futures_lite::future::block_on;

use tutti::midi::{Address, Message, MidiChannel, NoteNumber, TimedMidiEvent, U7, Ump, Voice1};
use tutti::plugin::PresetSupport;
use tutti::plugin::catalog::{CatalogConfig, PluginDb};
use tutti::session::Peer as _;
use tutti::{
    Beat, Bounce, Catalog, Format, LoadPlugin, LoadPreset, MainOutput, MaxBlock, ParamId, Pcm,
    PluginEvent, Range, RestoreState, SampleRate, Samples, SaveState, Session, SessionEvent,
    StateUse,
};
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_units::Normalized;

const RATE: SampleRate = SampleRate::SR_48K;

/// An in-process call, asked again until the plugin's node has let go of the
/// writes made before it (a drain lasts microseconds).
fn ready<O: tutti::PluginOp>(
    mut op: tutti::InProcessOp<O>,
    main: tutti::plugin::MainThreadMarker,
) -> Result<O::Output, O::Error> {
    loop {
        if let std::task::Poll::Ready(result) = op.poll(main, tutti_platform::Instant::now()) {
            return result;
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let (Some(name), Some(out)) = (args.next(), args.next()) else {
        return Err("usage: plugin_host NAME OUT.wav DIR [DIR...]".into());
    };
    let name = name.to_string_lossy().into_owned();
    let out = PathBuf::from(out);
    let dirs: Vec<PathBuf> = args.map(PathBuf::from).collect();

    // 0. The offline session the plugin is loaded into. The scan runs on its
    //    executor, so it is built first.
    let mut session = Session::builder()
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()?;

    // 1. Scan. The catalog's table is in memory (its persistence stores nothing,
    //    so nothing is kept between runs); the scan probes every file it finds in a
    //    `plugin-probe` subprocess, so a plugin that crashes the probe is
    //    blacklisted, not loaded into this process.
    let mut catalog = Catalog::new(
        PluginDb::new(),
        CatalogConfig::new(std::env::temp_dir()).scan_dirs(dirs),
        |_| Ok(()),
    );
    //    The scan is a future: this example has nothing else to do and awaits it
    //    here; a host with a frame loop polls it off the frame thread, reads
    //    `progress()` as it goes, and cancels by dropping it.
    let scanned = block_on(session.scan_plugins(&mut catalog)?.into_future())?;
    let scanned = catalog.apply(scanned)?;
    eprintln!("scanned {} plugin file(s)", scanned.scanned());
    //    A scan leaves its rows in memory; the save is a value this app runs inline.
    catalog.save().run()?;

    // 2. Pick by name. An entry knows its file, its format and its descriptor;
    //    the ref made from it is what the load and a saved state both name.
    let entry = catalog
        .entries()
        .find(|e| e.descriptor.name == name)
        .ok_or_else(|| format!("no plugin named {name:?} in the scan"))?;
    let instrument = entry.is_instrument();
    // `load_plugin` serves CLAP, VST3 and AU from a `plugin-server` process and
    //    runs VST2 here; `load_plugin_as` with `ResolvedHosting::InProcess` asks
    //    for this process where the format allows it.
    let plugin = entry.plugin_ref();
    eprintln!("{name}: {:?} in {}", plugin.id(), plugin.path().display());

    // 3. Load it on the main thread and add it to the session. The load
    //    names the catalog's answer for the plugin's id, so a plugin the catalog
    //    blacklisted does not resolve and fails, before any server starts. The
    //    thread that runs the load is the main thread: make the token once. The
    //    load gives the node the session takes; its handle observes the plugin.
    let main = tutti::plugin::MainThreadMarker::new().ok_or("run on the process's main thread")?;
    let id = entry
        .id
        .cloned()
        .ok_or("the scan could not read the plugin's id")?;
    let opened = match session.load_plugin(&id, &catalog)? {
        LoadPlugin::InProcess(load) => load.run(main)?,
        LoadPlugin::OutOfProcess(load) => block_on(load.into_future())?,
    };
    let loaded = opened.instance();
    let node = session.apply(opened)?;

    // 4. Wire it: the plugin's outputs feed the main output's channels. `node` is a
    //    `Key<AnyPlugin>`: ports come from it as from any node's key
    //    (`node.output(0)`, `node.event_input(0)`), and
    //    `node.param(id)` is the reference a route targets.
    session.graph().connect(node, MainOutput)?;

    // 5. Play. An instrument takes a note on its event input: middle C for the
    //    first two beats, as a clip on the project's timeline.
    if instrument {
        let note = |beat: f64, on: bool| {
            let (note, velocity) = (NoteNumber::of(60), U7::of(100));
            let voice = if on {
                Voice1::NoteOn { note, velocity }
            } else {
                Voice1::NoteOff { note, velocity }
            };
            let message = Message::Midi1(Address::first(MidiChannel::FIRST), voice);
            TimedMidiEvent::new(Beat::new(beat), Ump::from(message)).expect("a finite beat")
        };
        session
            .midi()
            .play_clip(node, Beat::new(0.0), [note(0.0, true), note(2.0, false)])?;
    }

    // 6. Set a parameter: the first one it lists, to the middle of its range.
    //    `params()` is the session's own view of what the plugin declares (read
    //    at load, read again when the plugin announces a rescan), so listing
    //    asks the plugin nothing; `get` resolves an id once and refuses one the
    //    plugin does not declare. A value set reads back at once, and `text` is
    //    the plugin's own wording of it. A drag is a gesture: `edit()` begins
    //    it, each `set` is a change, and dropping the guard ends it, so the
    //    session reports the host's begin, changes and end in `update()`'s
    //    events as it reports the plugin's own.
    let hosted = session.plugin_view(node)?;
    let params = hosted.params();
    let first: Option<ParamId> = params.iter().next().map(|p| p.id());
    if let Some(id) = first {
        let param = params.get(id)?;
        session.set(param.target().to(Normalized::clamped(0.5)))?;
        eprintln!(
            "{} = {}",
            param.info().name(),
            param
                .text()
                .unwrap_or_else(|| param.normalized().get().to_string())
        );
        let drag = param.edit()?;
        for step in 0..=4 {
            drag.set(Normalized::clamped(f64::from(step) / 8.0))?;
        }
        drop(drag);
    }

    // 7. Save its state: now, from the plugin (`last_good` is the session's
    //    crash-recovery copy, up to a snapshot interval old). `save_state`
    //    makes the work, by where the plugin runs. The state names the plugin it
    //    came from, so a project that keeps it restores it into that plugin and
    //    no other (`restore_state`, run, then `apply`).
    let state = match session.save_state(&node, StateUse::Project)? {
        SaveState::InProcess(save) => ready(save, main)?,
        SaveState::OutOfProcess(save) => block_on(save.into_future())?,
    };
    let state_path = out.with_extension("state");
    std::fs::write(&state_path, state.bytes())?;
    eprintln!(
        "saved {} bytes of {} to {}",
        state.bytes().len(),
        state.peer(),
        state_path.display()
    );
    let restored = match session.restore_state(&node, state)? {
        RestoreState::InProcess(restore) => ready(restore, main)?,
        RestoreState::OutOfProcess(restore) => block_on(restore.into_future())?,
    };
    session.apply(restored)?;

    // 8. Health and presets. `health()` is one reading of the plugin's life
    //    (running, stalled, dead and about to be written off, or being
    //    reloaded). Presets are part of the plugin's info, read when it loaded:
    //    `support()` says whether the plugin lists them (load a handle with
    //    `load_preset`), loads them from files (`LoadByPath`, CLAP:
    //    `load_preset_file`) or has none. A load is work the app runs (on the
    //    main thread, or awaited for a plugin in a server), then applies.
    {
        let info = loaded.info();
        let presets = info.presets();
        match presets.support() {
            PresetSupport::None => eprintln!("no presets"),
            support if support.can_list() => {
                eprintln!("{} preset(s)", presets.len());
                if let Some(first) = presets.iter().next() {
                    let loaded = match session.load_preset(&node, &first)? {
                        LoadPreset::InProcess(load) => ready(load, main)?,
                        LoadPreset::OutOfProcess(load) => block_on(load.into_future())?,
                    };
                    session.apply(loaded)?;
                    eprintln!(
                        "loaded {:?}, current {:?}",
                        first.name(),
                        session.current_preset(&node, main)
                    );
                }
            }
            _ => eprintln!("loads preset files, lists none"),
        }
    }

    // 9. Events. What the plugin reports (its own edit of a parameter, a new
    //    tail) and what happens to it (died, reloading) is one stream: the
    //    session's `SessionEvent::Plugin`, named by node. A host reads it from
    //    `update()`; there is no callback to install and none to clear.
    //    A session with recovery reloads a plugin that died. An app that wants
    //    its own reload composes it: `load_plugin_as` with the plugin's `info()`
    //    id and hosting, started from its state, then `swap_plugin` at the node
    //    (see `swap_plugin`'s "Reloading").
    //    `update` on the main thread also drops the plugins the session
    //    retired and services every open editor.
    for event in session.update() {
        if let SessionEvent::Plugin(PluginEvent { node, change, .. }) = event {
            eprintln!("{node:?}: {change:?}");
        }
    }

    // 10. Bounce four beats to a 24-bit WAV. The plugin hears Offline while it
    //    renders, and Realtime from the update after the apply.
    //    `replace`: the output named on the command line is overwritten.
    let rendering = session.bounce(
        Bounce::new()
            .replace()
            .range(Range::Beats(Beat::new(0.0), Beat::new(4.0)))
            .main_output(
                out.as_path(),
                Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24))),
            ),
    )?;
    let rendered = block_on(rendering.into_future());
    let written = session.apply(rendered)?.into_main_output();
    println!("wrote {} ({} bytes)", written.path.display(), written.bytes);
    Ok(())
}
