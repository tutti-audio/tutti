//! A hosted plugin's presets through its info and the session's jobs:
//! what the format can do with them, a list that loads by id, a file that
//! loads by path, and the typed answers of a load.
//!
//! CLAP over the reference plugin in a real `plugin-server` (with `TUTTI_CLAP_PROBE_PRESET_LOAD=1` it takes a preset
//! file with the suffix `.tutti-preset` and sets its Cutoff, observed through
//! its saved state); VST2 over the reference plugin in process, whose program
//! count the environment sets.

#[cfg(feature = "clap")]
mod clap {
    use crate::pooled::{LandOn as _, LoadPlugin, Spec};
    use crate::support::clap_probe;
    use tutti_buffers::MaxBlock;
    use tutti_graph::NodeKey;
    use tutti_plugin::catalog::PluginRef;
    use tutti_plugin_types::{PresetError, PresetSupport};
    use tutti_session::{Peer as _, PresetJobFailed, Session};
    use tutti_units::Samples;

    fn loaded() -> (Session, u64, NodeKey) {
        // The plugin's file is probed before its variables are set.
        crate::pooled::prime(&PluginRef::file(clap_probe()));
        // SAFETY: nextest runs each test in its own process, and this is set
        // before the first server is spawned, from the test's own thread.
        unsafe {
            std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
            // The reference plugin answers `clap.preset-load` only when asked.
            std::env::set_var("TUTTI_CLAP_PROBE_PRESET_LOAD", "1");
        }
        let mut s = Session::builder()
            .max_block(MaxBlock::of(Samples::new(512)))
            .build()
            .expect("an offline session builds");
        let (entry, node) = s
            .load_plugin_keyed(
                tutti_fixtures::main_thread(),
                Spec::new(PluginRef::file(clap_probe())),
            )
            .expect("the reference CLAP plugin loads");
        (s, entry, node.erase())
    }

    fn saved<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
        s: &Session<K, C>,
        node: NodeKey,
    ) -> Vec<u8> {
        s.plugin_view(node)
            .unwrap()
            .state()
            .save(tutti_plugin_types::StateUse::Project)
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
    fn cutoff<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
        s: &Session<K, C>,
        node: NodeKey,
    ) -> Option<f64> {
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
        let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(10);
        while !s.plugin_view(node).is_ok_and(|p| p.params().is_ready()) {
            assert!(
                tutti_platform::Instant::now() < deadline,
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

        let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(5);
        while cutoff(&s, node) == Some(before) {
            assert!(
                tutti_platform::Instant::now() < deadline,
                "the mirror still reads {before} after the preset moved Cutoff"
            );
            let _ = s.update_on(main);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

#[cfg(feature = "vst3")]
mod vst3 {
    use tutti_buffers::ChannelLayout;
    use tutti_plugin::catalog::PluginRef;
    use tutti_plugin_types::PresetId;
    use tutti_session::{PHASES, PhaseCx, PoolEvent, SessionEvent};
    use tutti_session::{Peer as _, Session};
    use tutti_units::SampleRate;

    /// **A served plugin's list is read by the first update on the main
    /// thread.** The shared plugin's VST3 declares one program list, `Factory`,
    /// of three programs. The load lands in updates that do not carry the main
    /// thread's proof (a served load needs none): the info is there, its list
    /// not yet read and empty. The first update on the main thread reads it,
    /// and the info the load handed out (an `Arc` a caller kept) shows it, as
    /// does a new one, in the plugin's order and bank.
    #[test]
    fn a_served_list_is_read_by_the_first_main_thread_update() {
        // SAFETY: nextest runs each test in its own process, and this is set
        // from the test's own thread before the first server is spawned.
        unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
        let module = tutti_fixtures::vst3_module();
        let bundle = tutti_plugin_types::bundle::bundle_dir_of(
            &module,
            tutti_plugin_types::bundle::ModuleKind::Vst3,
        )
        .expect("a .vst3 bundle around the module");
        let plugin = PluginRef::file(bundle.to_path_buf());
        let (catalog, id, _dir) = crate::pooled::catalog_of(&plugin);
        let mut s = Session::builder()
            .executor(tutti_platform::Executor::polled())
            .outputs_floor(ChannelLayout::STEREO)
            .build_detached(SampleRate::DEFAULT)
            .expect("a detached session builds");
        s.pool(s.plugin_entry(0u64, &id, &catalog)).unwrap();

        // Phases with no proof of the main thread: nothing runs main work.
        let mut now = tutti_platform::Instant::now();
        let deadline = now + std::time::Duration::from_secs(60);
        let kept = 'landed: loop {
            now += std::time::Duration::from_millis(20);
            let mut out = Vec::new();
            for &phase in PHASES {
                s.run_phase(phase, PhaseCx::at(now), &mut out);
            }
            for event in out {
                if let SessionEvent::Pool { event, .. } = event
                    && matches!(event, PoolEvent::Loaded)
                {
                    break 'landed s.plugin(&0u64).unwrap().info();
                }
            }
            assert!(
                tutti_platform::Instant::now() < deadline,
                "the load never landed"
            );
            std::thread::yield_now();
        };
        assert!(kept.presets().support().can_list());
        assert!(!kept.presets().is_read());
        assert_eq!(kept.presets().iter().count(), 0);

        let _ = s.update_on(tutti_fixtures::main_thread());
        let fresh = s.plugin(&0u64).unwrap().info();
        for info in [&kept, &fresh] {
            assert!(info.presets().is_read());
            let listed: Vec<_> = info
                .presets()
                .iter()
                .map(|p| {
                    (
                        p.id().clone(),
                        p.name().to_owned(),
                        p.bank().map(str::to_owned),
                    )
                })
                .collect();
            let expected: Vec<_> = ["Init", "Pad", "Lead"]
                .iter()
                .zip(0u32..)
                .map(|(name, index)| {
                    (
                        PresetId::Program { list_id: 1, index },
                        (*name).to_owned(),
                        Some("Factory".to_owned()),
                    )
                })
                .collect();
            assert_eq!(listed, expected);
        }
    }
}

#[cfg(feature = "vst2")]
mod vst2 {
    use crate::pooled::{LandOn as _, LoadEntry as _, LoadPlugin, Spec};
    use tutti_buffers::ChannelLayout;
    use tutti_fixtures::ScopedEnv;
    use tutti_plugin::catalog::{Hosting, PluginRef};
    use tutti_plugin_types::PresetId;
    use tutti_plugin_types::{PresetError, PresetSupport};
    use tutti_session::DetachedSession;
    use tutti_session::{
        Peer as _, PoolEvent, PresetJobError, PresetJobFailed, Session, SessionEvent,
    };
    use tutti_units::SampleRate;

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
                Spec::new(PluginRef::file(tutti_fixtures::vst2_probe().to_path_buf())),
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
                Spec::new(PluginRef::file(tutti_fixtures::vst2_probe().to_path_buf())),
            )
            .expect("a second VST2 loads");
        let refused = s.load_preset(&other, &foreign);
        assert!(
            matches!(refused, Err(PresetJobError::OtherPlugin)),
            "{:?}",
            refused.map(|_| ())
        );
    }

    /// A session with the probe's three programs loaded through an entry the
    /// test declares itself: its catalog, id and entry key stay in hand for a
    /// reload.
    fn declared() -> (
        DetachedSession,
        tutti_plugin::catalog::Catalog,
        tutti_plugin::catalog::PluginId,
        u64,
        tutti_graph::NodeKey,
        ScopedEnv,
        tempfile::TempDir,
    ) {
        let mut env = ScopedEnv::new();
        env.set("TUTTI_VST2_PROBE_PROGRAMS", "3");
        let mut s = Session::builder()
            .outputs_floor(ChannelLayout::STEREO)
            .build_detached(SampleRate::DEFAULT)
            .expect("a detached session builds");
        let plugin = PluginRef::file(tutti_fixtures::vst2_probe().to_path_buf());
        let (catalog, id, dir) = crate::pooled::catalog_of(&plugin);
        let main = tutti_fixtures::main_thread();
        let key = s.declare_entry(main, &catalog, &id, Hosting::Auto, None);
        let node = s.await_entry(main, key).expect("the VST2 loads").erase();
        (s, catalog, id, key, node, env, dir)
    }

    /// **A preset of a reloaded plugin is refused until the node has it.** The
    /// entry reloads (its hosting changes) and the new plugin lands; this
    /// thread is not the process's main thread, so the in-process swap waits.
    /// A preset listed from the new info is not the node's plugin's: it is
    /// refused at the call and nothing is sent, while one the node's own plugin
    /// listed still loads.
    #[test]
    fn a_preset_of_a_landed_reload_is_refused_until_the_swap() {
        let (mut s, catalog, id, key, _node, _env, _dir) = declared();
        let main = tutti_fixtures::main_thread();
        let old = s
            .plugin(&key)
            .unwrap()
            .info()
            .presets()
            .iter()
            .next()
            .unwrap();
        s.pool(
            s.plugin_entry(key, &id, &catalog)
                .hosting(Hosting::InProcess),
        )
        .unwrap();
        let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(60);
        'reloaded: loop {
            for event in s.update_on(main) {
                if let SessionEvent::Pool { key: k, event } = event
                    && k.get::<u64>() == Some(&key)
                    && matches!(event, PoolEvent::Reloaded)
                {
                    break 'reloaded;
                }
            }
            assert!(
                tutti_platform::Instant::now() < deadline,
                "the reload never landed"
            );
            std::thread::yield_now();
        }
        let new = s
            .plugin(&key)
            .unwrap()
            .info()
            .presets()
            .iter()
            .nth(2)
            .unwrap();
        assert_ne!(new, old, "another plugin instance listed it");

        let before = s.current_preset(&key, main);
        let refused = s.load_preset(&key, &new);
        assert!(
            matches!(refused, Err(PresetJobError::OtherPlugin)),
            "{:?}",
            refused.map(|_| ())
        );
        let _ = s.update_on(main);
        assert_eq!(s.current_preset(&key, main), before, "nothing was sent");

        s.load_preset(&key, &old)
            .unwrap()
            .land_on(&mut s)
            .expect("the node's own plugin lists it");
    }

    /// **A load for a node removed before the update lands `Lost`**: the plugin
    /// is gone, so nothing is asked.
    #[test]
    fn a_load_for_a_removed_node_lands_lost() {
        let (mut s, _catalog, _id, key, node, _env, _dir) = declared();
        let third = s
            .plugin(&key)
            .unwrap()
            .info()
            .presets()
            .iter()
            .nth(2)
            .unwrap();
        let job = s.load_preset(&key, &third).unwrap();
        s.graph().remove(node).expect("the node is removed");
        let landed = job.land_on(&mut s);
        assert!(
            matches!(landed, Err(PresetJobFailed::Preset(PresetError::Lost))),
            "{landed:?}"
        );
    }

    /// **Dropping a load's job before the update cancels it**: the plugin is
    /// never asked, so its own current program is where it was.
    #[test]
    fn a_dropped_load_is_not_run() {
        let (mut s, _catalog, _id, key, _node, _env, _dir) = declared();
        let main = tutti_fixtures::main_thread();
        let third = s
            .plugin(&key)
            .unwrap()
            .info()
            .presets()
            .iter()
            .nth(2)
            .unwrap();
        let before = s.current_preset(&key, main);
        drop(s.load_preset(&key, &third).unwrap());
        let _ = s.update_on(main);
        assert_eq!(s.current_preset(&key, main), before);
        assert_ne!(Some(third.id().clone()), before);
        // The same load, kept, does move it.
        s.load_preset(&key, &third)
            .unwrap()
            .land_on(&mut s)
            .unwrap();
        assert_eq!(s.current_preset(&key, main), Some(third.id().clone()));
    }
}
