//! The editor tests that need a window number that means the same in another
//! process: an X11 window id. One `cfg` on this module's declaration holds them
//! and their helpers.

use dpi::{PhysicalSize, Size};
use tutti_clap_test_plugin::{
    GUI_CMD_REQUEST_RESIZE, GUI_HEIGHT, GUI_REQUESTED_RESIZE_H, GUI_REQUESTED_RESIZE_W, GUI_WIDTH,
    GuiMode,
};
use tutti_plugin_ipc::{ControlFrame, ControlReply, ControlRequest, News};
use tutti_plugin_types::ContentScale;

use super::*;

/// Selects the gui shape for the load that follows; the default again on drop.
struct GuiShape;

impl GuiShape {
    fn embeddable() -> Self {
        let probe = crate::test_utils::clap();
        probe.set_gui_mode(GuiMode::Embeddable);
        probe.gui_reset();
        Self
    }
}

impl Drop for GuiShape {
    fn drop(&mut self) {
        let probe = crate::test_utils::clap();
        probe.gui_command(tutti_clap_test_plugin::GUI_CMD_NONE);
        probe.set_gui_mode(GuiMode::Absent);
    }
}

/// The editor opens on the served instance, a resize the plugin asks for
/// reaches the host with no message to carry it, the host's size is taken, and
/// a close destroys the plugin's editor. Needs a window number that means the
/// same here as in the host (an X11 window id).
#[test]
fn the_editor_opens_resizes_and_closes_on_the_served_instance() {
    let _lock = crate::test_utils::PLUGIN_LOAD_LOCK.lock();
    let _shape = GuiShape::embeddable();
    let probe = crate::test_utils::clap();
    probe.gui_command(GUI_CMD_REQUEST_RESIZE);
    let (mut session, _slab) = session();
    let wanted = Size::Physical(PhysicalSize::new(801, 601));
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Send(ControlRequest::OpenEditor {
            parent: Some(0x40_0001),
            scale: ContentScale::IDENTITY,
        }),
        // The plugin asked for a size from `show`; only an idle tick can say so.
        Until::step(|heard| {
            heard
                .iter()
                .any(|m| matches!(m, ControlFrame::News(News::EditorWantsSize { .. })))
        }),
        Step::Send(ControlRequest::SetEditorSize { size: wanted }),
        Step::Send(ControlRequest::CloseEditor),
        Until::step(move |_| probe.gui_capture().destroyed),
    ]);

    run(&mut session, &mut script);

    let opened = script
        .heard
        .iter()
        .find_map(|m| match m {
            ControlFrame::Reply {
                reply: ControlReply::EditorOpened { result },
                ..
            } => Some(result.clone()),
            _ => None,
        })
        .expect("the open is answered");
    assert_eq!(
        opened,
        Ok(Some(Size::Physical(PhysicalSize::new(
            GUI_WIDTH, GUI_HEIGHT
        )))),
        "the plugin's own size"
    );
    assert!(
        script.heard.iter().any(|m| matches!(
            m,
            ControlFrame::News(News::EditorWantsSize { size })
                if *size == Size::Physical(PhysicalSize::new(GUI_REQUESTED_RESIZE_W, GUI_REQUESTED_RESIZE_H))
        )),
        "the plugin's resize request reached the host: {:?}",
        script.heard
    );
    let sized = script
        .heard
        .iter()
        .find_map(|m| match m {
            ControlFrame::Reply {
                reply: ControlReply::EditorSized { result },
                ..
            } => Some(result.clone()),
            _ => None,
        })
        .expect("the resize is answered");
    let took = sized
        .expect("the plugin takes a size")
        .to_physical::<u32>(1.0);
    let cap = probe.gui_capture();
    assert_eq!(
        (cap.last_set_size_w, cap.last_set_size_h),
        (took.width, took.height),
        "the size the server answered is the one the plugin was set to"
    );
    assert!(
        cap.set_parent_window_non_null,
        "embedded in the host's window"
    );
    assert!(cap.destroyed, "a close reaches the plugin");
    assert_eq!(cap.create_balance, 0, "every create is destroyed");
}

/// **A plugin that a load replaces while its editor is open takes the editor with it, and
/// the host is told**: the server closed it on its own account.
#[test]
fn replacing_a_plugin_closes_its_editor_and_says_so() {
    let _lock = crate::test_utils::PLUGIN_LOAD_LOCK.lock();
    let _shape = GuiShape::embeddable();
    let probe = crate::test_utils::clap();
    let (mut session, _slab) = session();
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Send(ControlRequest::OpenEditor {
            parent: Some(0x40_0001),
            scale: ContentScale::IDENTITY,
        }),
        Step::Send(load_message()),
        Until::step(|heard| {
            heard
                .iter()
                .any(|m| matches!(m, ControlFrame::News(News::EditorClosed)))
        }),
    ]);

    run(&mut session, &mut script);

    assert!(
        script
            .heard
            .iter()
            .any(|m| matches!(m, ControlFrame::News(News::EditorClosed))),
        "{:?}",
        script.heard
    );
    assert_eq!(
        probe.gui_capture().create_balance,
        0,
        "the editor was destroyed"
    );
}
