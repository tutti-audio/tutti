//! The editor: `Session::editor` hands out an `Editor`, which opens, rescales
//! and closes the plugin's editor, is serviced by `Session::update` on the
//! main thread, and ends with its plugin; and the graveyard a removal parks a
//! plugin in until the main thread's update empties it.
//!
//! The plugin is `tutti_plugin_types::fake`'s: a control with an editor the
//! test scripts (`FakePlugin::with_editor`, `request_editor_size`), hosted at a
//! node over `host`, so no `plugin-server` and no display is
//! needed.

use super::{Inert, host, host_served};
use std::sync::Arc;

use dpi::PhysicalSize;

use crate::{DetachedSession, EditorCallError, Opened, Session};
use tutti_buffers::ChannelLayout;
use tutti_fixtures::StandInWindow;
use tutti_graph::{ForkByClone, NodeKey};
use tutti_nodes::testing::Const;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::fake::{Call, FakePlugin};
use tutti_plugin_types::{ContentScale, EditorError};
use tutti_units::SampleRate;

/// A detached session with a fake plugin (with an editor) at a node.
fn session_with_editor() -> (DetachedSession, NodeKey, FakePlugin) {
    hosting(Inert::default())
}

/// [`session_with_editor`] over `backend`.
fn hosting(backend: Inert) -> (DetachedSession, NodeKey, FakePlugin) {
    hosting_as(backend, false)
}

/// [`hosting`], the plugin served from a plugin server or in process.
fn hosting_as(backend: Inert, served: bool) -> (DetachedSession, NodeKey, FakePlugin) {
    let (fake, loaded) = FakePlugin::new();
    fake.with_editor(true);
    let control: Arc<dyn tutti_plugin_host::MainThreadControl> = Arc::new(loaded);
    let handle = PluginHandle::from_backend(
        Arc::new(backend),
        Some(control),
        tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
        Default::default(),
        Default::default(),
    );
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let node = s.graph().add(ForkByClone(Const::mono(0.0)));
    let plugin = tutti_plugin_types::PluginRef::file("fake.clap");
    if served {
        host_served(&mut s, node, handle, plugin);
    } else {
        host(&mut s, node, handle, plugin);
    }
    s.commit();
    (s, node.erase(), fake)
}

fn idles(fake: &FakePlugin) -> usize {
    fake.calls()
        .iter()
        .filter(|c| **c == Call::EditorIdle)
        .count()
}

fn closes(fake: &FakePlugin) -> usize {
    fake.calls()
        .iter()
        .filter(|c| **c == Call::CloseEditor)
        .count()
}

/// **An editor reads the size the plugin asked for, once**: the request is
/// the editor's to read, and a second read has nothing more to say.
#[test]
fn an_editor_reads_a_requested_size_once() {
    let (mut s, node, fake) = session_with_editor();
    let main = tutti_fixtures::main_thread();
    let mut editor = s.editor(node, &main).unwrap();
    let opened = editor.open(&StandInWindow, ContentScale::IDENTITY).unwrap();
    let Opened::Embedded { size, mirrored } = opened else {
        panic!("an embedded open: {opened:?}");
    };
    assert_eq!(size, PhysicalSize::new(640, 480).into());
    assert!(mirrored.is_ok(), "{mirrored:?}");

    let want = PhysicalSize::new(1000, 700);
    fake.request_editor_size(want);
    assert_eq!(editor.requested_size(), Some(want.into()));
    assert_eq!(editor.requested_size(), None, "a request is read once");
}

/// **A new editor is closed, opening it opens it, and dropping it closes
/// the plugin's editor on the thread that made it**: the editor's state is the
/// plugin handle's flag, and the fake's calls show the close.
#[test]
fn an_editor_reads_open_after_open_and_a_drop_closes_it() {
    let (mut s, node, fake) = session_with_editor();
    let main = tutti_fixtures::main_thread();
    let mut editor = s.editor(node, &main).unwrap();
    assert!(editor.exists());
    assert!(!editor.is_open(), "a new editor is closed");
    editor.open(&StandInWindow, ContentScale::IDENTITY).unwrap();
    assert!(editor.is_open());
    assert_eq!(closes(&fake), 0);

    drop(editor);
    assert_eq!(closes(&fake), 1, "the drop closed it");
    assert!(
        !s.plugin_view(node).unwrap().handle().is_editor_open(),
        "the plugin reads closed"
    );
}

/// **An editor that never opened leaves another's open editor alone** when it
/// is dropped.
#[test]
fn dropping_an_editor_that_never_opened_closes_nothing() {
    let (mut s, node, fake) = session_with_editor();
    let main = tutti_fixtures::main_thread();
    let mut opened = s.editor(node, &main).unwrap();
    opened.open(&StandInWindow, ContentScale::IDENTITY).unwrap();
    drop(s.editor(node, &main).unwrap());
    assert_eq!(closes(&fake), 0);
    assert!(opened.is_open());
}

/// **A key that hosts no plugin has no editor**, by name.
#[test]
fn an_editor_for_a_node_with_no_plugin_is_refused_by_name() {
    let (mut s, node, _fake) = session_with_editor();
    let main = tutti_fixtures::main_thread();
    s.remove(node).unwrap();
    let error = s.editor(node, &main).unwrap_err();
    assert_eq!(error.node(), node);
}

/// **`update` services an editor only while it is open, and only on the main
/// thread:** the plugin's idle runs once per update on it with the editor
/// open; an update off it, a closed editor, and a dropped one tick nothing.
#[test]
fn update_services_the_open_editor_on_the_main_thread_and_no_other() {
    let (mut s, node, fake) = session_with_editor();
    let main = tutti_fixtures::main_thread();
    let mut editor = s.editor(node, &main).unwrap();
    let _ = s.update_on(main);
    assert_eq!(idles(&fake), 0, "nothing is serviced before it opens");

    editor.open(&StandInWindow, ContentScale::IDENTITY).unwrap();
    let _ = s.update_on(main);
    let _ = s.update_on(main);
    assert_eq!(idles(&fake), 2, "one idle per update");
    let _ = s.update();
    assert_eq!(
        idles(&fake),
        2,
        "an update off the main thread services none"
    );

    editor.close();
    let _ = s.update_on(main);
    assert_eq!(idles(&fake), 2, "a closed editor is not serviced");

    editor.open(&StandInWindow, ContentScale::IDENTITY).unwrap();
    drop(editor);
    let _ = s.update_on(main);
    assert_eq!(idles(&fake), 2, "a dropped editor is not serviced");
}

/// **`rescale` reaches the plugin's live scale call, without closing the
/// editor**, and a format with none answers the typed refusal: the fake
/// takes the scale only when told to (as CLAP and VST3 do), and refuses
/// like VST2 and AU otherwise.
#[test]
fn rescale_reaches_the_plugins_scale_call_or_is_refused_by_name() {
    let (mut s, node, fake) = session_with_editor();
    let main = tutti_fixtures::main_thread();
    let mut editor = s.editor(node, &main).unwrap();
    editor.open(&StandInWindow, ContentScale::IDENTITY).unwrap();
    let two = ContentScale::new(2.0).unwrap();

    assert!(
        matches!(
            editor.rescale(two),
            Err(EditorCallError::Plugin(EditorError::ScaleUnsupported))
        ),
        "a format with no live scale call refuses by name"
    );
    fake.with_editor_scale(true);
    editor.rescale(two).unwrap();
    let calls = fake.calls();
    assert!(calls.contains(&Call::SetEditorScale(2.0_f64.to_bits())));
    assert_eq!(
        calls.iter().filter(|c| **c == Call::OpenEditor).count(),
        1,
        "the editor was not reopened"
    );
    assert!(
        !calls.contains(&Call::CloseEditor),
        "the editor was not closed"
    );
}

/// **A removed plugin waits in the graveyard until an update on the main
/// thread empties it:** an update off it leaves it. The count is
/// `Session::retired_plugins`.
#[test]
fn a_removed_plugin_is_dropped_by_the_main_threads_update_not_by_another() {
    let (mut s, node, _fake) = session_with_editor();
    assert_eq!(s.retired_plugins(), 0);
    s.remove(node).unwrap();
    assert_eq!(s.retired_plugins(), 1, "removal parks the record");
    let _ = s.update();
    assert_eq!(
        s.retired_plugins(),
        1,
        "an update off the main thread frees no plugin"
    );
    let _ = s.update_on(tutti_fixtures::main_thread());
    assert_eq!(s.retired_plugins(), 0, "the main thread's update drops it");
}

/// **A served plugin's record drops where it is retired, with no main-thread
/// update**: nothing of it waits in the graveyard.
#[test]
fn a_removed_served_plugin_drops_at_once() {
    let backend = Inert::default();
    let dropped_on = Arc::clone(&backend.dropped_on);
    let (mut s, node, _fake) = hosting_as(backend, true);
    s.remove(node).unwrap();
    assert_eq!(s.retired_plugins(), 0, "nothing is parked");
    assert_eq!(
        *dropped_on.lock().unwrap(),
        Some(std::thread::current().id()),
        "the removing thread dropped it"
    );
}

/// **An in-process plugin's record drops on the main thread, whichever thread
/// removed it:** the removal on another thread leaves the backend alive, and
/// the main thread's update is where its last owner lets go.
#[test]
fn a_removed_in_process_plugin_drops_on_the_main_thread() {
    let backend = Inert::default();
    let dropped_on = Arc::clone(&backend.dropped_on);
    let (mut s, node, _fake) = hosting(backend);
    let remover = std::thread::spawn(move || {
        s.remove(node).unwrap();
        s
    })
    .join()
    .unwrap();
    let mut s = remover;
    assert_eq!(s.retired_plugins(), 1, "the record is parked");
    assert_eq!(*dropped_on.lock().unwrap(), None, "nothing dropped yet");
    let _ = s.update_on(tutti_fixtures::main_thread());
    assert_eq!(
        *dropped_on.lock().unwrap(),
        Some(std::thread::current().id()),
        "the main thread dropped it"
    );
}

/// **A removal ends the editor:** it reads closed and its calls answer
/// `Ended` at once, and the plugin's editor is closed by the next update on
/// the main thread, not before.
#[test]
fn a_removal_ends_the_editor() {
    let (mut s, node, fake) = session_with_editor();
    let main = tutti_fixtures::main_thread();
    let mut editor = s.editor(node, &main).unwrap();
    editor.open(&StandInWindow, ContentScale::IDENTITY).unwrap();
    s.remove(node).unwrap();

    assert!(
        !editor.is_open(),
        "the removed plugin's editor reads closed"
    );
    assert!(matches!(
        editor.open(&StandInWindow, ContentScale::IDENTITY),
        Err(EditorCallError::Ended)
    ));
    assert!(matches!(
        editor.resize(PhysicalSize::new(2, 2).into()),
        Err(EditorCallError::Ended)
    ));
    assert_eq!(editor.requested_size(), None);
    assert_eq!(closes(&fake), 0, "not before the main thread's update");

    let _ = s.update();
    assert_eq!(closes(&fake), 0, "not by an update off the main thread");
    let _ = s.update_on(main);
    assert_eq!(closes(&fake), 1, "the main thread's update closed it");
    assert!(!editor.is_open());
    drop(editor);
    assert_eq!(closes(&fake), 1, "the drop does not close it again");
}

/// **Two editors of one plugin share its one open editor**: the second `open`
/// is refused by name while the first is open, the plugin's open is not called
/// twice, and the first's drop closes nothing the second opened after the first
/// closed.
#[test]
fn a_second_editor_cannot_open_over_the_first_and_a_closed_one_closes_nothing() {
    let (mut s, node, fake) = session_with_editor();
    let main = tutti_fixtures::main_thread();
    let opens = |fake: &FakePlugin| {
        fake.calls()
            .iter()
            .filter(|c| **c == Call::OpenEditor)
            .count()
    };
    let mut a = s.editor(node, &main).unwrap();
    let mut b = s.editor(node, &main).unwrap();
    a.open(&StandInWindow, ContentScale::IDENTITY).unwrap();
    assert!(
        b.is_open(),
        "the plugin's editor is open, whoever opened it"
    );
    assert!(matches!(
        b.open(&StandInWindow, ContentScale::IDENTITY),
        Err(EditorCallError::AlreadyOpen)
    ));
    assert_eq!(opens(&fake), 1, "the plugin was not asked to open twice");

    a.close();
    b.open(&StandInWindow, ContentScale::IDENTITY).unwrap();
    assert_eq!(opens(&fake), 2);
    let before = closes(&fake);
    drop(a);
    assert_eq!(
        closes(&fake),
        before,
        "a closed editor's drop closes nothing"
    );
    assert!(b.is_open());
}
