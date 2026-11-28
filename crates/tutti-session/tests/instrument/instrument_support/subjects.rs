//! The three instruments, as subjects.

use crate::loading::{LoadPlugin, Perform, Spec};
use tutti_music::Adsr;
use tutti_polysynth::{FilterType, OscillatorType, PolySynth, SynthConfig};
use tutti_sf2::SoundFont;
use tutti_soundfont::{SoundFontConfig, SoundFontUnit};
use tutti_units::{Amplitude, Seconds};

use super::cases::{Chunks, Gap, Subject};
use super::rig::{RATE, Rig, offline_session};

/// `PolySynth`: a saw, a 1 ms attack and a 50 ms release.
pub(crate) struct Poly;

impl Subject for Poly {
    const NAME: &'static str = "PolySynth";
    const GAPS: &'static [Gap] = &[];
    const PITCHED: bool = true;
    const SETTLE: usize = 4_800;
    const LEAD: (usize, usize) = (0, 4);
    const CUT: usize = 1;

    fn rig() -> Rig {
        let synth = PolySynth::from(
            SynthConfig::new()
                .polyphony(std::num::NonZeroUsize::new(8).unwrap())
                .oscillator(OscillatorType::Saw)
                .filter(FilterType::None)
                .envelope(Adsr {
                    attack: Seconds::new(0.001),
                    decay: Seconds::new(0.0),
                    sustain: Amplitude::new(1.0),
                    release: Seconds::new(0.05),
                }),
        );
        let mut session = offline_session();
        let handle = session
            .add_instrument(synth)
            .expect("a synth is an instrument");
        Rig::played(session, handle)
    }
}

/// The SoundFont unit over the repository's test font, the first preset.
pub(crate) struct Soundfont;

impl Subject for Soundfont {
    const NAME: &'static str = "SoundFontUnit";
    const GAPS: &'static [Gap] = &[];
    const PITCHED: bool = true;
    /// The release of the font's first preset, measured: silent within 6 s.
    const SETTLE: usize = 6 * 48_000;
    /// The lead of the font's attack ramp, and the unit's 8-frame event
    /// resolution either side.
    const LEAD: (usize, usize) = (0, 64);
    const CUT: usize = 8;

    fn rig() -> Rig {
        let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
        let font =
            std::sync::Arc::new(SoundFont::open(path.as_path()).expect("the test font parses"));
        let unit = SoundFontUnit::try_from(SoundFontConfig::new(font)).expect("builds");
        let mut session = offline_session();
        let handle = session
            .add_instrument(unit)
            .expect("a SoundFont unit is an instrument");
        Rig::played(session, handle)
    }
}

/// The shared test plugin (`tutti-shared-plugin`) in `Notes` mode: a note gate
/// that outputs `key + 1000 * channel` while a note is held. The mode is a word
/// in the state it is loaded with, as the plugin contract requires: no
/// environment variable, symbol or process global. In a real `plugin-server`.
pub(crate) struct Plugin;

impl Plugin {
    /// A session and the plugin it loaded, in the graph.
    pub(crate) fn load(
        path: std::path::PathBuf,
        state: Option<Vec<u8>>,
    ) -> (
        tutti_session::Session,
        tutti_session::Key<tutti_session::AnyPlugin>,
    ) {
        tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        let mut session = offline_session();
        let plugin = tutti_plugin_types::PluginRef::file(path);
        let request = match state {
            Some(bytes) => Spec::new(plugin.clone())
                .restoring(tutti_session::PeerState::of_plugin(plugin, bytes)),
            None => Spec::new(plugin),
        };
        let node = session
            .load_plugin_blocking(tutti_fixtures::main_thread(), request)
            .expect("the plugin loads");
        (session, node)
    }

    /// The state that puts the shared plugin in `Notes` mode: its mode
    /// parameter set, saved (`core.rs`, "State").
    pub(crate) fn notes_state() -> Vec<u8> {
        let core = tutti_shared_plugin::core::Core::new();
        core.set_param(tutti_shared_plugin::core::MODE, 3.0 / 4.0);
        core.save()
    }

    /// The shared plugin, state in `Notes` mode.
    pub(crate) fn notes() -> (
        tutti_session::Session,
        tutti_session::Key<tutti_session::AnyPlugin>,
    ) {
        let real = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
        Self::load(
            tutti_fixtures::publish_with_extension(&real, "clap"),
            Some(Self::notes_state()),
        )
    }

    /// The reference probe, which declares itself an effect.
    pub(crate) fn effect() -> (
        tutti_session::Session,
        tutti_session::Key<tutti_session::AnyPlugin>,
    ) {
        let real = tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
        Self::load(tutti_fixtures::publish_with_extension(&real, "clap"), None)
    }
}

impl Subject for Plugin {
    const NAME: &'static str = "SharedPluginNotes";
    const GAPS: &'static [Gap] = &[];
    const PITCHED: bool = false;
    const SETTLE: usize = 1_024;
    const LEAD: (usize, usize) = (0, 0);
    const CUT: usize = 1;

    /// The plugin's own record of what the host sent it, read from its state.
    fn transcript(rig: &Rig) -> Option<tutti_shared_plugin::transcript::Transcript> {
        let state = rig
            .session
            .save_state(&rig.plugin?, tutti_plugin_types::StateUse::Project)
            .expect("the plugin is hosted")
            .perform(tutti_fixtures::main_thread())
            .expect("the plugin saves its state");
        // Not `complete()`: its overflow flag is also set by more blocks than the
        // log keeps, and a case this long renders more. The events are few.
        Some(tutti_shared_plugin::transcript::Transcript::parse(
            &tutti_shared_plugin::transcript::find_blob(state.bytes()),
        ))
    }

    fn chunks(rig: &Rig) -> Chunks {
        let d = rig
            .session
            .plugin_view(rig.node)
            .expect("the plugin is hosted")
            .diagnostics()
            .chunks();
        Chunks {
            missed: d.missed.get(),
            late: d.late.get(),
            refused: d.refused.get(),
            skipped: d.skipped.get(),
        }
    }

    fn rig() -> Rig {
        let (mut session, node) = Self::notes();
        let handle = session
            .instrument(node)
            .expect("the plugin declares itself an instrument");
        let mut rig = Rig::wired(session, handle);
        rig.plugin = Some(node);
        rig
    }
}
