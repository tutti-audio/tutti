//! A hosted plugin's presets through its info and the session's jobs:
//! what the format can do with them, a list that loads by id, a file that
//! loads by path, and the typed answers of a load.
//!
//! CLAP over the reference plugin in a real `plugin-server` (with `TUTTI_CLAP_PROBE_PRESET_LOAD=1` it takes a preset
//! file with the suffix `.tutti-preset` and sets its Cutoff, observed through
//! its saved state); VST2 over the reference plugin in process, whose program
//! count the environment sets.

use crate::pooled::{LandOn as _, LoadPlugin, Spec};
use tutti_plugin::catalog::PluginRef;
use tutti_plugin_types::{PresetError, PresetSupport};
use tutti_session::{Peer as _, PresetJobError, PresetJobFailed, Session};

#[cfg(feature = "clap")]
mod clap {
    use super::*;
    use crate::support::{clap_probe, plugin_server};
    use tutti_graph::NodeKey;
    use tutti_types::{MaxBlock, Samples};

    fn loaded() -> (Session, u64, NodeKey) {
        // The plugin's file is probed before its variables are set.
        crate::pooled::prime(&PluginRef::file(clap_probe()).expect("a plugin file"));
        // SAFETY: nextest runs each test in its own process, and this is set
        // before the first server is spawned, from the test's own thread.
        unsafe {
            std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server());
            // The reference plugin answers `clap.preset-load` only when asked.
            std::env::set_var("TUTTI_CLAP_PROBE_PRESET_LOAD", "1");
        }
        let mut s = Session::builder()
            .max_block(MaxBlock::of(Samples(512)))
            .build()
            .expect("an offline session builds");
        let (entry, node) = s
            .load_plugin_keyed(
                tutti_fixtures::main_thread(),
                Spec::new(PluginRef::file(clap_probe()).expect("a plugin file")),
            )
            .expect("the reference CLAP plugin loads");
        (s, entry, node.erase())
    }

    fn saved<C: tutti_session::Clock>(s: &Session<C>, node: NodeKey) -> Vec<u8> {
        s.plugin_view(node)
            .unwrap()
            .state()
            .save()
            .expect("the plugin saves")
            .bytes()
            .to_vec()
    }

    /// **CLAP loads a preset from a file and lists none.** Its support is
    /// `LoadByPath` with an empty list that needs no read; a file it accepts
    /// changes its state, and one it declines lands `Refused` and leaves the
    /// state alone, so the load reached the plugin and the plugin's answer
    /// reached the job.
    #[test]
    fn clap_loads_a_preset_file_and_lists_nothing() {
        let (mut s, entry, node) = loaded();
        let info = s.plugin(&entry).unwrap().info();
        let presets = info.presets();
        assert_eq!(presets.support(), PresetSupport::LoadByPath);
        assert!(
            presets.is_read() && presets.is_empty(),
            "CLAP enumerates nothing"
        );
        assert_eq!(
            s.current_preset(&entry, tutti_fixtures::main_thread()),
            None,
            "and has no current-preset query"
        );

        let before = saved(&s, node);
        let refused = s
            .load_preset_file(&entry, "/nowhere/not-a-preset.txt")
            .unwrap()
            .land_on(&mut s);
        assert!(
            matches!(refused, Err(PresetJobFailed::Preset(PresetError::Refused))),
            "the plugin declined a file of the wrong kind: {refused:?}"
        );
        assert_eq!(saved(&s, node), before, "a refused load changes nothing");

        let taken = s
            .load_preset_file(&entry, "/nowhere/bright.tutti-preset")
            .unwrap()
            .land_on(&mut s);
        assert!(taken.is_ok(), "{taken:?}");
        assert_ne!(
            saved(&s, node),
            before,
            "an accepted preset file moved the plugin's own state"
        );
    }

    /// The mirror's value of the plugin's first parameter (Cutoff).
    fn cutoff<C: tutti_session::Clock>(s: &Session<C>, node: NodeKey) -> Option<f64> {
        let plugin = s.plugin_view(node).unwrap();
        let params = plugin.params();
        let first = params.iter().next()?.id();
        params.get(first).ok().map(|p| p.normalized().get())
    }

    /// **A preset the session loads moves the parameter mirror.** The plugin
    /// reports no value change for a preset file (it only sets Cutoff), so the
    /// mirror read at load keeps the old value unless the session reads it
    /// again: after the load, updates bring the mirror to the plugin's new
    /// Cutoff.
    #[test]
    fn a_loaded_preset_file_moves_the_mirror() {
        let (mut s, entry, node) = loaded();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !s.plugin_view(node).is_ok_and(|p| p.params().is_ready()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the read never landed"
            );
            let _ = s.update();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let before = cutoff(&s, node).expect("Cutoff is read");

        let main = tutti_fixtures::main_thread();
        s.load_preset_file(&entry, "/nowhere/bright.tutti-preset")
            .unwrap()
            .land_on(&mut s)
            .expect("the plugin takes the preset");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while cutoff(&s, node) == Some(before) {
            assert!(
                std::time::Instant::now() < deadline,
                "the mirror still reads {before} after the preset moved Cutoff"
            );
            let _ = s.update_on(main);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

#[cfg(feature = "vst2")]
mod vst2 {
    use super::*;
    use tutti_fixtures::ScopedEnv;
    use tutti_plugin_types::PresetId;
    use tutti_session::DetachedSession;
    use tutti_types::{ChannelLayout, SampleRate};

    fn probe_with(programs: &str) -> (DetachedSession, u64, ScopedEnv) {
        let mut env = ScopedEnv::new();
        env.set("TUTTI_VST2_PROBE_PROGRAMS", programs);
        let mut s = Session::builder()
            .outputs_floor(ChannelLayout::STEREO)
            .build_detached(SampleRate::DEFAULT)
            .expect("a detached session builds");
        let (entry, _) = s
            .load_plugin_keyed(
                tutti_fixtures::main_thread(),
                Spec::new(
                    PluginRef::file(tutti_fixtures::vst2_probe().to_path_buf())
                        .expect("a plugin file"),
                ),
            )
            .expect("the in-process VST2 loads");
        (s, entry, env)
    }

    /// **A listed preset loads by the id it was listed with.** A list of the
    /// probe's three programs, read when the plugin loaded; the third loaded
    /// from its handle and confirmed by the plugin's own current-program query;
    /// an id the plugin never listed is not a handle, and a file load lands
    /// `Unsupported`.
    #[test]
    fn a_listed_preset_loads_by_its_id() {
        let (mut s, entry, _env) = probe_with("3");
        let main = tutti_fixtures::main_thread();
        let info = s.plugin(&entry).unwrap().info();
        let presets = info.presets();
        assert_eq!(presets.support(), PresetSupport::Full);
        assert!(presets.is_read(), "an in-process list is read at the load");
        assert_eq!(presets.len(), 3, "the probe declared three programs");

        let third = presets.iter().nth(2).expect("three entries");
        let id = third.id().clone();
        s.load_preset(&entry, &third)
            .unwrap()
            .land_on(&mut s)
            .expect("the plugin takes it");
        assert_eq!(
            s.current_preset(&entry, main),
            Some(id.clone()),
            "the plugin took it"
        );

        assert!(presets.get(&id).is_some());
        assert!(presets.get(&PresetId::Number(77)).is_none());
        let file = s
            .load_preset_file(&entry, "/x.tutti-preset")
            .unwrap()
            .land_on(&mut s);
        assert!(
            matches!(file, Err(PresetJobFailed::Preset(PresetError::Unsupported))),
            "a list-and-load format takes no file: {file:?}"
        );
    }

    /// **A plugin that declares no programs has no preset support**, not an
    /// empty browser: the format was asked and declined, and a file is
    /// `Unsupported` too.
    #[test]
    fn a_plugin_with_no_programs_has_no_presets() {
        let (mut s, entry, _env) = probe_with("0");
        let info = s.plugin(&entry).unwrap().info();
        assert_eq!(info.presets().support(), PresetSupport::None);
        assert!(info.presets().is_empty());
        let file = s
            .load_preset_file(&entry, "/x.tutti-preset")
            .unwrap()
            .land_on(&mut s);
        assert!(matches!(
            file,
            Err(PresetJobFailed::Preset(PresetError::Unsupported))
        ));
    }

    /// **A preset listed by one plugin is not loaded into another**: the job
    /// is refused at the call and nothing is sent.
    #[test]
    fn a_preset_of_another_plugin_is_refused_at_the_call() {
        let (mut s, entry, _env) = probe_with("3");
        let foreign = s
            .plugin(&entry)
            .unwrap()
            .info()
            .presets()
            .iter()
            .next()
            .expect("a listed preset");
        // Listed before the second plugin loads.
        let (other, _) = s
            .load_plugin_keyed(
                tutti_fixtures::main_thread(),
                Spec::new(
                    PluginRef::file(tutti_fixtures::vst2_probe().to_path_buf())
                        .expect("a plugin file"),
                ),
            )
            .expect("a second VST2 loads");
        let refused = s.load_preset(&other, &foreign);
        assert!(
            matches!(refused, Err(PresetJobError::OtherPlugin)),
            "{:?}",
            refused.map(|_| ())
        );
    }
}
