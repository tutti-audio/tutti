//! Which class of a bundle a load picks: the choice is the class id the
//! plugin id holds ([`Uid`]), never a display name.

use tutti_fixtures::main_thread;
use tutti_plugin_types::Uid;
use tutti_vst3_host::{
    Library, LoadCause, LoadOptions, LoadStage, Vst3Error, Vst3Loaded, class_id,
};

/// The audio classes of the shared test plugin's bundle, as (id, name).
fn audio_classes(path: &std::path::Path) -> Vec<(Uid, String)> {
    let library = Library::load(path).expect("the bundle opens");
    (0..library.count_classes())
        .filter_map(|i| library.class_info(i).ok())
        .filter(|info| info.category.contains("Audio"))
        .map(|info| (class_id(&info), info.name))
        .collect()
}

/// **A load by class id loads that class, and its descriptor reports that id.**
/// The bundle exports several audio classes; the last is chosen so a load that
/// ignored the choice (the first class) is told apart.
#[test]
fn a_load_by_class_id_picks_that_class() {
    let path = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    let classes = audio_classes(&path);
    assert!(classes.len() > 1, "the bundle exports several classes");
    let (id, name) = classes.last().unwrap().clone();
    assert_ne!(Some(&id), classes.first().map(|c| &c.0));

    let loaded = Vst3Loaded::load(
        main_thread(),
        &tutti_fixtures::detached_work(),
        &path,
        LoadOptions::new().class_id(id),
    )
    .expect("the class loads");
    let info = loaded.plugin_info();
    assert_eq!(info.id, id);
    assert_eq!(info.name, name);
}

/// **An id the bundle does not export is refused, and the refusal lists the
/// bundle's classes by id and name.** A display name is no [`Uid`]: asking for
/// a class by one cannot be written.
#[test]
fn an_unknown_class_id_is_refused_naming_the_classes() {
    let path = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    let classes = audio_classes(&path);
    let wanted = Uid::new([0; 16]);
    let err = Vst3Loaded::load(
        main_thread(),
        &tutti_fixtures::detached_work(),
        &path,
        LoadOptions::new().class_id(wanted),
    )
    .expect_err("no class has this id");
    let Vst3Error::LoadFailed {
        stage: LoadStage::Factory,
        cause: LoadCause::ClassNotFound { wanted: w, offered },
        ..
    } = &err
    else {
        panic!("expected ClassNotFound, got {err:?}");
    };
    assert_eq!(*w, wanted.to_string());
    assert_eq!(offered.len(), classes.len());
    for (id, name) in &classes {
        assert!(
            offered
                .iter()
                .any(|o| o.contains(&id.to_string()) && o.contains(name)),
            "{offered:?} lists {id} ({name})"
        );
    }
}

/// **A VST3 load refuses an id of another format at the call**, naming both,
/// and loads nothing: a CLAP id picks no class of the bundle.
#[test]
fn an_id_of_another_format_is_refused_at_the_call() {
    use tutti_plugin_types::{InProc, PluginId, PluginRef};
    let path = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    let other = PluginId::Clap("tutti.shared-test-plugin".into());
    // Matched rather than `expect_err`, because `Vst3Loaded` is not `Debug`.
    let msg = match Vst3Loaded::load_in_process(
        main_thread(),
        &tutti_fixtures::detached_work(),
        &PluginRef::new(&path, other.clone()),
    ) {
        Ok(_) => panic!("a CLAP id must not load a VST3 class"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains(&other.to_string()) && msg.contains("vst3"),
        "the error names the id and the loader: {msg}"
    );
}
