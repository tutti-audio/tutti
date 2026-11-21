//! Load a plugin, play a MIDI file, export to WAV.
//!
//! The plugin is loaded as a value the app runs and added to an offline session; the file's
//! events become a clip on the plugin's event input; the project adopts the
//! file's tempo map, and the bounce renders the file's beats (so their length
//! is the map's, steps included), trims the latency the plan compensated, and
//! renders the plugin's declared tail. The plugin hears Offline from
//! `bounce_start` and Realtime from `finish_bounce`.
//!
//! With no arguments it plays a two-bar phrase it writes itself through the
//! shared reference plugin (`tutti-shared-plugin`, in its `Notes` mode: it
//! renders the key it holds), which needs the
//! `plugin-server` built first:
//!
//! ```text
//! cargo build -p tutti-plugin-server -p tutti-plugin-probe
//! cargo run -p tutti --features clap,export --example plugin_midi_export
//! cargo run -p tutti --features clap,export --example plugin_midi_export -- \
//!     Synth.clap song.mid song.wav
//! ```
//!
//! CI runs it as a test (`test = true`), which plays the phrase.
//!
//! It is the umbrella's, not `tutti-session`'s, so that it reads as an
//! application does: one `use tutti::{..}` from the root (the session's
//! requests are there, behind the features that gate them). `tutti-session` cannot
//! name `tutti`, which depends on it.

// The server lookup `tutti-session`'s plugin suite uses (its reference-probe
// lookup is for that suite).
#[allow(dead_code)]
#[path = "../../tutti-session/tests/support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};

use tutti::midi::{MidiChannel, MidiFile, NoteNumber, Tick, Timebase, Track};
use tutti::session::Master;
use tutti::{
    Beat, Bounce, Bpm, Format, LoadPlugin, MaxBlock, NodeKey, Pcm, PeerState, Range, Root,
    SampleRate, Samples, Session, SessionEvent, Written,
};

const RATE: SampleRate = SampleRate::SR_48K;

/// What `Session::update` found: faults and dropouts (a refused commit, a
/// plugin that died, a lost block). A host logs them; here they are printed.
fn report(events: Vec<SessionEvent>) {
    for event in events {
        eprintln!("session event: {event:?}");
    }
}

/// Probes `plugin` into a catalog, loads it on the main thread (its state
/// given to the load, restored before the plugin is activated), and adds its
/// node.
fn load(
    s: &mut Session,
    main: tutti::plugin::MainThreadMarker,
    plugin: &Path,
    state: Option<Vec<u8>>,
) -> Result<tutti::Key<tutti::AnyPlugin>, Box<dyn std::error::Error>> {
    use tutti::plugin::catalog::{CatalogConfig, PluginDb};
    let dir = tempfile::TempDir::new()?;
    let mut catalog =
        tutti::Catalog::new(PluginDb::new(), CatalogConfig::new(dir.path()), |_| Ok(()));
    let scanned = tutti_fixtures::block_on(s.register_plugin(&mut catalog, plugin)?)?;
    let scanned = catalog.apply(scanned)?;
    catalog.save().run()?;
    if let Some(failure) = scanned.failures().first() {
        return Err(format!("the plugin did not probe: {failure:?}").into());
    }
    let entry = catalog.entries().next().ok_or("the file lists no plugin")?;
    let id = entry
        .id
        .cloned()
        .ok_or("the scan could not read the plugin's id")?;
    let plugin = entry.plugin_ref();
    let state = state.map(|bytes| PeerState::of_plugin(plugin, bytes));
    let loaded = match s.load_plugin(&id, &catalog)? {
        LoadPlugin::InProcess(mut load) => {
            if let Some(state) = state {
                load.initial_state(state)?;
            }
            load.run(main)?
        }
        LoadPlugin::OutOfProcess(mut load) => {
            if let Some(state) = state {
                load.initial_state(state)?;
            }
            tutti_fixtures::block_on(load)?
        }
    };
    Ok(s.add_plugin(loaded)?)
}

/// Plays `plugin` (restored from `state`, if any)
/// `midi` into the master `master` names (a file or memory, which `M` says
/// and the result's type follows).
fn play<M: Master>(
    main: tutti::plugin::MainThreadMarker,
    plugin: &Path,
    state: Option<Vec<u8>>,
    midi: &Path,
    master: impl FnOnce(Bounce) -> Bounce<NodeKey, M>,
) -> Result<M::Result<f32>, Box<dyn std::error::Error>> {
    let mut s = Session::builder()
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()?;

    let synth = load(&mut s, main, plugin, state)?;
    s.graph().connect(synth, Root).unwrap();

    // The load is a value the app runs: `run` reads the file (an SMF or a Clip
    // File) and converts its tracks to clips. A file in SMPTE time or of format 2 has no
    // one beat timeline to adopt: this example plays one metrical timeline only.
    let loaded = s.midi().load_midi(midi).run()?;
    let tempo_map = loaded.tempo()?.clone();
    let Timebase::Metrical(metrical) = loaded.file().timebase() else {
        return Err("a file with no one beat timeline has nothing to play on".into());
    };
    let end = metrical.beat_at(metrical.end());
    // A file's tempo track is the file's data, not the project's: the project
    // adopts it on import, here, by an explicit call (a shared map, no copy).
    s.set(s.transport_handle().tempo().to(tempo_map))?;
    s.midi().play_track(synth, loaded.track(0)?)?;
    report(s.update());

    // Everything else is derived: the clock is the project's tempo map from
    // beat 0, the latency is the plan's, the tail is the plugin's declared
    // one, and the file is as wide as the root.
    let job = s.bounce_start(master(
        Bounce::project().range(Range::Beats(Beat::new(0.0), end)),
    ))?; // the plugin hears Offline from here
    let bounced = s.finish_bounce(job.run())?; // and Realtime from here
    Ok(bounced.into_master())
}

/// [`play`] into a 24-bit WAV at `out`.
fn play_to_file(
    main: tutti::plugin::MainThreadMarker,
    plugin: &Path,
    state: Option<Vec<u8>>,
    midi: &Path,
    out: &Path,
) -> Result<Written, Box<dyn std::error::Error>> {
    play(main, plugin, state, midi, |bounce| {
        bounce.master(out, Format::Wav(Pcm::Int24))
    })
}

/// Two bars: a note on each beat, 120 BPM for the first four beats and 60
/// for the last four, as a file with a tempo map.
fn phrase(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use tutti::midi::{Layout, LeadTrack, Meta, SmfDivision, TicksPerQuarterNote, U7};
    const TPQ: u16 = 480;
    const DIVISION: TicksPerQuarterNote = TicksPerQuarterNote::of(TPQ);
    const SMF_DIVISION: SmfDivision = SmfDivision::per_quarter(TPQ);
    let beat = |n| Tick::quarters(n, DIVISION).expect("eight quarters fit");
    let mut track = Track::new()
        .meta(Tick(0), Meta::tempo(Bpm::of(120.0)))
        .meta(beat(4), Meta::tempo(Bpm::of(60.0))); // 60 BPM from beat 4
    for n in 0..8u8 {
        let key = NoteNumber::of(60 + n);
        track = track.note(
            beat(n.into()),
            beat(u64::from(n) + 1),
            MidiChannel::FIRST,
            key,
            U7::of(100),
        );
    }
    MidiFile::smf(SMF_DIVISION, Layout::Format0(LeadTrack::new(track)))?.write(path)?;
    Ok(())
}

/// The reference plugin's cdylib, published as a `.clap`, and its state in
/// `Notes` mode (mode 3 of 5, the parameter's value `k / 4`).
fn notes_plugin() -> (PathBuf, Vec<u8>) {
    let real = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    let core = tutti_shared_plugin::core::Core::new();
    core.set_param(tutti_shared_plugin::core::MODE, 3.0 / 4.0);
    (
        tutti_fixtures::publish_with_extension(&real, "clap"),
        core.save(),
    )
}

/// The phrase through the reference plugin, in `dir`.
fn reference(
    main: tutti::plugin::MainThreadMarker,
    dir: &Path,
) -> Result<Written, Box<dyn std::error::Error>> {
    let midi = dir.join("phrase.mid");
    phrase(&midi)?;
    let (plugin, state) = notes_plugin();
    play_to_file(main, &plugin, Some(state), &midi, &dir.join("phrase.wav"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The thread that opens an in-process plugin is the main thread: the token is made once.
    let main = tutti::plugin::MainThreadMarker::new().ok_or("run on the process's main thread")?;
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let written = match args.as_slice() {
        [plugin, midi, out] => play_to_file(main, plugin, None, midi, out)?,
        [] => {
            // SAFETY: set before the first server or probe is spawned, while
            // `main` is the only thread.
            unsafe {
                std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
                std::env::set_var("TUTTI_PLUGIN_PROBE", tutti_fixtures::plugin_probe());
            }
            reference(main, &std::env::temp_dir())?
        }
        _ => return Err("usage: plugin_midi_export [PLUGIN MIDI OUT]".into()),
    };
    println!("wrote {} ({} bytes)", written.path.display(), written.bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    /// **The example plays**: the phrase through the reference plugin
    /// writes a WAV holding the file's length plus the plugin's tail.
    /// Fails, not skips, without `plugin-server`.
    ///
    /// The length is pinned in frames: four beats at 120 BPM and four at 60
    /// are 2 s + 4 s; past them, the probe's declared tail (8191 frames,
    /// `tutti_shared_plugin::core::TAIL`); the latency the plan compensated is trimmed
    /// from the front, not added. The file is 24-bit stereo,
    /// 6 bytes a frame, after a header of at least 44 bytes.
    #[test]
    fn the_example_plays() {
        let dir = std::env::temp_dir().join(format!("tutti-5-2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut env = tutti_fixtures::ScopedEnv::new();
        env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        env.set("TUTTI_PLUGIN_PROBE", tutti_fixtures::plugin_probe());
        let written =
            super::reference(tutti_fixtures::main_thread(), &dir).expect("the example runs");
        let frames = 6 * 48_000 + u64::from(tutti_shared_plugin::core::TAIL);
        let header = written.bytes.checked_sub(frames * 6);
        assert!(
            header.is_some_and(|h| (44..256).contains(&h)),
            "{} bytes for {frames} frames of 24-bit stereo",
            written.bytes
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The file's notes land on the frames of the file's tempo map.** The
    /// shared plugin in `Notes` mode renders the key it holds, so the left
    /// channel of the bounce steps to a key on the frame its note arrived on.
    /// The phrase is a note a beat, 120 BPM to beat 4 and 60 from it: written
    /// out from the bpm, a beat is `60 / bpm` s, so beats 0 to 4 land every
    /// 24 000 frames at 48 kHz and beats 5 to 7 every 48 000 after frame
    /// 96 000. Past the last note's end, the file's 288 000 frames, the plugin
    /// holds nothing (zero).
    ///
    /// Break it by taking the file's first tempo for the whole bounce (the
    /// project's map left at its default or constant): beats 5 to 7 land at
    /// 120 000, 144 000 and 168 000 and the assertion fails.
    #[test]
    fn the_notes_land_on_the_frames_of_the_files_tempo_map() {
        let dir = std::env::temp_dir().join(format!("tutti-5-2-notes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut env = tutti_fixtures::ScopedEnv::new();
        env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        env.set("TUTTI_PLUGIN_PROBE", tutti_fixtures::plugin_probe());
        let midi = dir.join("phrase.mid");
        super::phrase(&midi).unwrap();
        let (plugin, state) = super::notes_plugin();
        // Into buffers, not the file: the plugin's output is a key number,
        // far past full scale, which a WAV clips.
        let rendered = super::play(
            tutti_fixtures::main_thread(),
            &plugin,
            Some(state),
            &midi,
            tutti::Bounce::master_buffers,
        )
        .expect("the example runs");
        let left: Vec<i32> = rendered.planes()[0].iter().map(|&s| s as i32).collect();

        let onsets: Vec<(usize, i32)> = (0..left.len())
            .filter(|&f| left[f] != 0 && (f == 0 || left[f] != left[f - 1]))
            .map(|f| (f, left[f]))
            .collect();
        let frames = [0, 24_000, 48_000, 72_000, 96_000, 144_000, 192_000, 240_000];
        let want: Vec<(usize, i32)> = frames.iter().zip(60..).map(|(&f, key)| (f, key)).collect();
        assert_eq!(onsets, want);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
