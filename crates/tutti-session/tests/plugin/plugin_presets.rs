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
    use crate::loading::Perform as _;
    use crate::loading::{LoadPlugin, Spec};
    use crate::support::clap_probe;
    use tutti_buffers::MaxBlock;
    use tutti_plugin_types::PluginRef;
    use tutti_plugin_types::{PresetError, PresetSupport};
    use tutti_session::{
        AnyPlugin, Key, LoadPreset, Peer as _, PluginApplyError, PluginInstance, Session,
    };
    use tutti_units::Samples;

    fn loaded() -> (Session, PluginInstance, Key<AnyPlugin>) {
        // The plugin's file is probed before its variables are set.
        crate::loading::prime(&PluginRef::file(clap_probe()));
        tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        // The reference plugin answers `clap.preset-load` only when asked.
        tutti_fixtures::set_env("TUTTI_CLAP_PROBE_PRESET_LOAD", "1");
        let mut s = Session::builder()
            .max_block(MaxBlock::of(Samples::new(512)))
            .build()
            .expect("an offline session builds");
        let (plugin, node) = s
            .load_plugin_handle(
                tutti_fixtures::main_thread(),
                Spec::new(PluginRef::file(clap_probe())),
            )
            .expect("the reference CLAP plugin loads");
        (s, plugin, node)
    }

    fn saved<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
        s: &Session<K, C>,
        node: Key<AnyPlugin>,
    ) -> Vec<u8> {
        s.save_state(&node, tutti_plugin_types::StateUse::Project)
            .unwrap()
            .perform(tutti_fixtures::main_thread())
            .expect("the plugin saves")
            .bytes()
            .to_vec()
    }

    /// **CLAP loads a preset from a file and lists none.** Its support is
    /// `LoadByPath` with an empty list that needs no read; a file it accepts
    /// changes its state, and one it declines ends `Refused` and leaves the
    /// state alone, so the load reached the plugin and the plugin's answer
    /// reached the run.
    #[test]
    fn clap_loads_a_preset_file_and_lists_nothing() {
        let (s, plugin, node) = loaded();
        let info = plugin.info();
        let presets = info.presets();
        assert_eq!(presets.support(), PresetSupport::LoadByPath);
        assert!(
            presets.is_read() && presets.is_empty(),
            "CLAP enumerates nothing"
        );
        assert_eq!(
            s.current_preset(&node, tutti_fixtures::main_thread()),
            None,
            "and has no current-preset query"
        );

        let before = saved(&s, node);
        let main = tutti_fixtures::main_thread();
        let refused = s
            .load_preset_file(&node, "/nowhere/not-a-preset.txt")
            .unwrap()
            .perform(main);
        assert!(
            matches!(refused, Err(PresetError::Refused)),
            "the plugin declined a file of the wrong kind: {refused:?}"
        );
        assert_eq!(saved(&s, node), before, "a refused load changes nothing");

        let taken = s
            .load_preset_file(&node, "/nowhere/bright.tutti-preset")
            .unwrap()
            .perform(main);
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
        node: Key<AnyPlugin>,
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
        let (mut s, _plugin, node) = loaded();
        tutti_fixtures::wait_until("the read to land", tutti_fixtures::HANG_GUARD, || {
            let _ = s.update();
            s.plugin_view(node).is_ok_and(|p| p.params().is_ready())
        });
        let before = cutoff(&s, node).expect("Cutoff is read");

        let main = tutti_fixtures::main_thread();
        let loaded = s
            .load_preset_file(&node, "/nowhere/bright.tutti-preset")
            .unwrap()
            .perform(main)
            .expect("the plugin takes the preset");
        s.apply(loaded).expect("the node hosts the plugin");

        tutti_fixtures::wait_until(
            "the mirror to leave the value before the preset",
            tutti_fixtures::HANG_GUARD,
            || {
                let _ = s.update_on(main);
                cutoff(&s, node) != Some(before)
            },
        );
    }

    /// **A preset awaited off the main thread moves the mirror once the main
    /// thread applies it.** The load is a `Send` future a spawned thread awaits
    /// (the bridge's reader fills it); its output comes back to the main thread,
    /// where `apply` has the session read the plugin's parameters again.
    #[test]
    fn a_preset_file_awaited_off_main_moves_the_mirror_when_applied() {
        let (mut s, _plugin, node) = loaded();
        let main = tutti_fixtures::main_thread();
        let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(10);
        while !s.plugin_view(node).is_ok_and(|p| p.params().is_ready()) {
            assert!(tutti_platform::Instant::now() < deadline, "no read landed");
            let _ = s.update_on(main);
            std::thread::yield_now();
        }
        let before = cutoff(&s, node).expect("Cutoff is read");

        let LoadPreset::OutOfProcess(load) = s
            .load_preset_file(&node, "/nowhere/bright.tutti-preset")
            .unwrap()
        else {
            panic!("a served plugin's preset is awaited");
        };
        let loaded = std::thread::spawn(move || tutti_fixtures::block_on(load))
            .join()
            .expect("the awaiting thread ends")
            .expect("the plugin takes the preset");
        s.apply(loaded).expect("the node hosts the plugin");

        let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(5);
        while cutoff(&s, node) == Some(before) {
            assert!(
                tutti_platform::Instant::now() < deadline,
                "the mirror still reads {before} after the preset moved Cutoff"
            );
            let _ = s.update_on(main);
            std::thread::yield_now();
        }
    }

    /// **A loaded preset applied after crash recovery replaced the node's
    /// plugin is refused `Stale`**: the server that took the preset is gone.
    #[test]
    fn a_preset_applied_after_a_recovery_replaced_the_plugin_is_stale() {
        let (mut s, _plugin, node) = loaded();
        let main = tutti_fixtures::main_thread();
        let loaded = s
            .load_preset_file(&node, "/nowhere/bright.tutti-preset")
            .unwrap()
            .perform(main)
            .expect("the plugin takes the preset");
        let before = s.plugin_view(node).unwrap().diagnostics().server_pid();
        s.plugin_view(node).unwrap().diagnostics().terminate(
            tutti_plugin_types::TerminationReason::Hung {
                blocks: 16,
                held: std::time::Duration::from_secs(5),
            },
        );
        let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let _ = s.update_on(main);
            let now = s.plugin_view(node).unwrap().diagnostics().server_pid();
            if now.is_some() && now != before {
                break;
            }
            assert!(
                tutti_platform::Instant::now() < deadline,
                "the plugin never recovered"
            );
            std::thread::yield_now();
        }
        assert_eq!(s.apply(loaded).unwrap_err(), PluginApplyError::Stale);
    }
}

#[cfg(feature = "vst3")]
mod vst3 {
    use crate::loading::Perform as _;
    use crate::loading::{Block as _, Hosted as _};
    use tutti_buffers::ChannelLayout;
    use tutti_plugin_types::PluginRef;
    use tutti_plugin_types::PresetId;
    use tutti_session::PresetStartError;
    use tutti_session::{Peer as _, Session};
    use tutti_units::SampleRate;

    /// **A served plugin's list is read by the load, which runs on the main
    /// thread.** The shared plugin's VST3 declares one program list, `Factory`,
    /// of three programs. The handle's info lists them, in the plugin's order
    /// and bank, as soon as the load returns, and so does the info of a clone.
    #[test]
    fn a_served_list_is_read_at_the_load() {
        tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        let module = tutti_fixtures::vst3_module();
        let bundle = tutti_plugin_types::bundle::bundle_dir_of(
            &module,
            tutti_plugin_types::bundle::ModuleKind::Vst3,
        )
        .expect("a .vst3 bundle around the module");
        let plugin = PluginRef::file(bundle.to_path_buf());
        let (catalog, id, _dir) = crate::loading::catalog_of(&plugin);
        let s = Session::builder()
            .outputs_floor(ChannelLayout::STEREO)
            .build_detached(SampleRate::DEFAULT)
            .expect("a detached session builds");
        let opened = s
            .load_out_of_process(&id, &catalog)
            .block()
            .expect("the shared plugin loads");
        let loaded = opened.instance();

        assert!(loaded.info().presets().support().can_list());
        for info in [loaded.info(), loaded.clone().info()] {
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

    /// **A node inserted with `Graph::add_plugin`, no load behind it, loads
    /// a preset its own plugin listed.** The list is read from the node's
    /// plugin; a preset of another plugin instance is refused.
    #[test]
    fn a_node_inserted_by_the_host_loads_a_preset_its_plugin_listed() {
        tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        let module = tutti_fixtures::vst3_module();
        let bundle = tutti_plugin_types::bundle::bundle_dir_of(
            &module,
            tutti_plugin_types::bundle::ModuleKind::Vst3,
        )
        .expect("a .vst3 bundle around the module");
        let client = |rate| {
            tutti_plugin::PluginClient::new(
                tutti_plugin_ipc::IpcConfig::new(),
                tutti_fixtures::executor(),
                &tutti_fixtures::detached_work(),
                &tutti_plugin_db::Loadable::uncatalogued(PluginRef::file(bundle.to_path_buf())),
                rate,
            )
            .expect("the shared plugin loads")
        };
        let main = tutti_fixtures::main_thread();
        let mut s = Session::builder()
            .outputs_floor(ChannelLayout::STEREO)
            .build_detached(SampleRate::DEFAULT)
            .expect("a detached session builds");
        let node = s.graph().add_plugin(Box::new(client(SampleRate::DEFAULT)));
        let other = s.graph().add_plugin(Box::new(client(SampleRate::DEFAULT)));

        let listed = s.plugin_view(node).unwrap().presets(main);
        let preset = listed.iter().nth(1).expect("the plugin lists programs");
        let loaded = s
            .load_preset(&node, &preset)
            .expect("a preset of the node's own plugin")
            .perform(main)
            .expect("the plugin takes it");
        s.apply(loaded).expect("the node hosts the plugin");
        assert!(matches!(
            s.load_preset(&other, &preset),
            Err(PresetStartError::OtherPlugin)
        ));
    }

    /// **A crash recovery loads the plugin again, so a preset listed before
    /// the crash is refused `OtherPlugin` after it**: the new plugin is another
    /// instance, and the listing belongs to the one that died.
    #[test]
    fn a_preset_listed_before_a_crash_recovery_is_another_plugins() {
        tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        let module = tutti_fixtures::vst3_module();
        let bundle = tutti_plugin_types::bundle::bundle_dir_of(
            &module,
            tutti_plugin_types::bundle::ModuleKind::Vst3,
        )
        .expect("a .vst3 bundle around the module");
        let (catalog, id, _dir) =
            crate::loading::catalog_of(&PluginRef::file(bundle.to_path_buf()));
        let main = tutti_fixtures::main_thread();
        let mut s = Session::builder()
            .outputs_floor(ChannelLayout::STEREO)
            .plugin_recovery(tutti_session::RecoveryPolicy::default())
            .build_detached(SampleRate::DEFAULT)
            .expect("a detached session builds");
        let plugin = s.load_out_of_process(&id, &catalog).block().unwrap();
        let node = s.apply(plugin).unwrap();
        let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(60);
        let preset = s
            .plugin_view(node)
            .unwrap()
            .presets(main)
            .iter()
            .nth(1)
            .expect("the plugin lists programs");
        assert!(s.load_preset(&node, &preset).is_ok(), "listed by this one");

        let before = s.plugin_view(node).unwrap().diagnostics().server_pid();
        s.plugin_view(node).unwrap().diagnostics().terminate(
            tutti_plugin_types::TerminationReason::Hung {
                blocks: 16,
                held: std::time::Duration::from_secs(5),
            },
        );
        loop {
            let _ = s.update_on(main);
            let now = s.plugin_view(node).unwrap().diagnostics().server_pid();
            if now.is_some() && now != before {
                break;
            }
            assert!(
                tutti_platform::Instant::now() < deadline,
                "the plugin never recovered"
            );
            std::thread::yield_now();
        }
        assert!(matches!(
            s.load_preset(&node, &preset),
            Err(PresetStartError::OtherPlugin)
        ));
    }
}

#[cfg(feature = "vst2")]
mod vst2 {
    use crate::loading::Perform as _;
    use crate::loading::{Hosted as _, LoadFrom as _, LoadPlugin, Spec};
    use tutti_buffers::ChannelLayout;
    use tutti_fixtures::ScopedEnv;
    use tutti_plugin_db::Hosting;
    use tutti_plugin_types::PluginRef;
    use tutti_plugin_types::PresetId;
    use tutti_plugin_types::{PresetError, PresetSupport};
    use tutti_session::DetachedSession;
    use tutti_session::{
        AnyPlugin, Key, Peer as _, PluginApplyError, PluginInstance, PresetStartError, Session,
    };
    use tutti_units::SampleRate;

    fn probe_with(programs: &str) -> (DetachedSession, PluginInstance, Key<AnyPlugin>, ScopedEnv) {
        let mut env = ScopedEnv::new();
        env.set("TUTTI_VST2_PROBE_PROGRAMS", programs);
        let mut s = Session::builder()
            .outputs_floor(ChannelLayout::STEREO)
            .build_detached(SampleRate::DEFAULT)
            .expect("a detached session builds");
        let (plugin, node) = s
            .load_plugin_handle(
                tutti_fixtures::main_thread(),
                Spec::new(PluginRef::file(tutti_fixtures::vst2_probe().to_path_buf())),
            )
            .expect("the in-process VST2 loads");
        (s, plugin, node, env)
    }

    /// **A listed preset loads by the id it was listed with.** A list of the
    /// probe's three programs, read when the plugin loaded; the third loaded
    /// from its handle and confirmed by the plugin's own current-program query;
    /// an id the plugin never listed is not a handle, and a file load lands
    /// `Unsupported`.
    #[test]
    fn a_listed_preset_loads_by_its_id() {
        let (mut s, plugin, node, _env) = probe_with("3");
        let main = tutti_fixtures::main_thread();
        let info = plugin.info();
        let presets = info.presets();
        assert_eq!(presets.support(), PresetSupport::Full);
        assert!(presets.is_read(), "an in-process list is read at the load");
        assert_eq!(presets.len(), 3, "the probe declared three programs");

        let third = presets.iter().nth(2).expect("three entries");
        let id = third.id().clone();
        let loaded = s
            .load_preset(&node, &third)
            .unwrap()
            .perform(main)
            .expect("the plugin takes it");
        s.apply(loaded).expect("the node hosts the plugin");
        assert_eq!(
            s.current_preset(&node, main),
            Some(id.clone()),
            "the plugin took it"
        );

        assert!(presets.get(&id).is_some());
        assert!(presets.get(&PresetId::Number(77)).is_none());
        let file = s
            .load_preset_file(&node, "/x.tutti-preset")
            .unwrap()
            .perform(main);
        assert!(
            matches!(file, Err(PresetError::Unsupported)),
            "a list-and-load format takes no file: {file:?}"
        );
    }

    /// **A plugin that declares no programs has no preset support**, not an
    /// empty browser: the format was asked and declined, and a file is
    /// `Unsupported` too.
    #[test]
    fn a_plugin_with_no_programs_has_no_presets() {
        let (s, plugin, node, _env) = probe_with("0");
        let info = plugin.info();
        assert_eq!(info.presets().support(), PresetSupport::None);
        assert!(info.presets().is_empty());
        let file = s
            .load_preset_file(&node, "/x.tutti-preset")
            .unwrap()
            .perform(tutti_fixtures::main_thread());
        assert!(matches!(file, Err(PresetError::Unsupported)));
    }

    /// **A preset listed by one plugin is not loaded into another**: the load
    /// is refused at the call and nothing is sent.
    #[test]
    fn a_preset_of_another_plugin_is_refused_at_the_call() {
        let (mut s, plugin, _node, _env) = probe_with("3");
        let foreign = plugin
            .info()
            .presets()
            .iter()
            .next()
            .expect("a listed preset");
        // Listed before the second plugin loads.
        let (_, other) = s
            .load_plugin_handle(
                tutti_fixtures::main_thread(),
                Spec::new(PluginRef::file(tutti_fixtures::vst2_probe().to_path_buf())),
            )
            .expect("a second VST2 loads");
        let refused = s.load_preset(&other, &foreign);
        assert!(
            matches!(refused, Err(PresetStartError::OtherPlugin)),
            "{:?}",
            refused.map(|_| ())
        );
    }

    /// A session with the probe's three programs loaded from a catalog the
    /// test keeps: its catalog and id stay in hand for a second load.
    fn declared() -> (
        DetachedSession,
        tutti_plugin_db::Catalog,
        tutti_plugin_types::PluginId,
        PluginInstance,
        Key<AnyPlugin>,
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
        let (catalog, id, dir) = crate::loading::catalog_of(&plugin);
        let main = tutti_fixtures::main_thread();
        let (plugin, node) = s
            .load_from(main, &catalog, &id, Hosting::Auto, None)
            .expect("the VST2 loads");
        (s, catalog, id, plugin, node, env, dir)
    }

    /// **A preset of a second plugin is refused until the node has it.** A
    /// second plugin loads in process and waits, in no node. A preset listed
    /// from its info is not the node's plugin's: it is refused at the call and
    /// nothing is sent, while one the node's own plugin listed still loads. Once
    /// the node swaps to the second plugin the two change places.
    #[test]
    fn a_preset_of_a_second_plugin_is_refused_until_the_node_swaps_to_it() {
        let (mut s, catalog, id, first, node, _env, _dir) = declared();
        let main = tutti_fixtures::main_thread();
        let old = first.info().presets().iter().next().unwrap();
        let second = s
            .load_in_process(&id, &catalog)
            .run(main)
            .expect("a second VST2 loads");
        let incoming = second.instance();
        let new = incoming.info().presets().iter().nth(2).unwrap();
        assert_ne!(new, old, "another plugin instance listed it");

        let before = s.current_preset(&node, main);
        let refused = s.load_preset(&node, &new);
        assert!(
            matches!(refused, Err(PresetStartError::OtherPlugin)),
            "{:?}",
            refused.map(|_| ())
        );
        let _ = s.update_on(main);
        assert_eq!(s.current_preset(&node, main), before, "nothing was sent");

        let loaded = s
            .load_preset(&node, &old)
            .unwrap()
            .perform(main)
            .expect("the node's own plugin lists it");
        s.apply(loaded).expect("the node hosts the plugin");

        s.swap_plugin(node, second, main)
            .expect("the second plugin swaps in");
        assert!(
            matches!(
                s.load_preset(&node, &old),
                Err(PresetStartError::OtherPlugin)
            ),
            "the first plugin's list is another instance's now"
        );
        let loaded = s
            .load_preset(&node, &new)
            .unwrap()
            .perform(main)
            .expect("the node's new plugin lists it");
        s.apply(loaded).expect("the node hosts the plugin");
    }

    /// **A load for a node removed before it runs ends `Lost`**: the plugin
    /// is gone, so nothing is asked.
    #[test]
    fn a_load_for_a_removed_node_is_lost() {
        let (mut s, _catalog, _id, plugin, node, _env, _dir) = declared();
        let third = plugin.info().presets().iter().nth(2).unwrap();
        let load = s.load_preset(&node, &third).unwrap();
        s.remove(node).expect("the node is removed");
        let run = load.perform(tutti_fixtures::main_thread());
        assert!(matches!(run, Err(PresetError::Lost)), "{run:?}");
    }

    /// **A preset loaded and then applied to a node removed since is refused
    /// `Gone`**: the plugin took the load, and the session has no node to read
    /// the parameters of.
    #[test]
    fn a_loaded_preset_applied_after_its_node_was_removed_is_gone() {
        let (mut s, _catalog, _id, plugin, node, _env, _dir) = declared();
        let main = tutti_fixtures::main_thread();
        let third = plugin.info().presets().iter().nth(2).unwrap();
        let loaded = s.load_preset(&node, &third).unwrap().perform(main).unwrap();
        s.remove(node).expect("the node is removed");
        assert_eq!(s.apply(loaded).unwrap_err(), PluginApplyError::Gone);
    }
}
