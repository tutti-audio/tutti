//! A hosted plugin's editor: [`Session::editor`] hands out an [`Editor`], the
//! one public type that holds the main-thread proof.
//!
//! A [`Session`] is `Send` and has no main thread of its own: a headless
//! session has none to be handed. What only the main thread may do is a method
//! that takes the proof ([`MainThreadMarker`], which the app makes once, on its
//! main thread, with [`MainThreadMarker::new`]); an editor is a lasting one, so
//! it keeps the proof and is `!Send` with it:
//!
//! - [`Session::editor`] makes it, for the plugin at a node.
//! - It opens in a window ([`Editor::open`]), sizes, rescales and closes. It
//!   closes when it is dropped, on the thread that made it.
//! - [`Session::update`] services every open editor (the format's idle tick:
//!   VST2's `effEditIdle`, VST3's run loop on Linux) when it is called on the
//!   main thread, so a host calls no `editor_idle` of its own. What the plugin
//!   asks for, a size, is read with [`Editor::requested_size`].
//!
//! An editor's subject is the plugin instance, not the key. A removal of the
//! node and a replacement or reload of the plugin end it: it reads closed
//! ([`Editor::is_open`]), its calls answer [`EditorCallError::Ended`], and the
//! plugin it held is closed by the next update on the main thread.
//!
//! ```no_run
//! # use tutti_session::Session;
//! # use tutti_graph::NodeKey;
//! # use tutti_plugin_types::{ContentScale, MainThreadMarker};
//! # fn open(session: &mut Session, node: NodeKey, window: &impl raw_window_handle::HasWindowHandle) {
//! let main = MainThreadMarker::new().expect("the app's main thread makes the token");
//! let mut editor = session.editor(node, &main).unwrap();
//! let _ = editor.open(window, ContentScale::IDENTITY);
//! let _events = session.update();
//! # }
//! ```
//!
//! The proof cannot leave its thread, so neither can the editor:
//!
//! ```compile_fail
//! use tutti_session::Editor;
//! fn assert_send<T: Send>() {}
//! assert_send::<Editor>();
//! ```
//!
//! and `Session` itself stays `Send`, for the host with no main thread:
//!
//! ```
//! fn assert_send<T: Send>() {}
//! assert_send::<tutti_session::Session>();
//! ```

use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};

use dpi::Size;
use raw_window_handle::HasWindowHandle;
use tutti_graph::NodeKey;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{
    ContentScale, EditorError, EditorMirrorError, Features, KeyEvent, KeyUse, MainThreadMarker,
    WindowHandle,
};

use crate::{Clock, NoSuchNode, Session};

/// What an [`Editor`] and the session's registry share: no plugin handle, so
/// the control thread that reads it never holds the last owner of a plugin.
#[derive(Debug)]
struct EditorFlags {
    node: NodeKey,
    /// Set when the node is removed or the plugin replaced: the instance this
    /// editor is for no longer serves its node.
    ended: AtomicBool,
    /// Set once the main thread closed the plugin's editor of an ended editor.
    closed: AtomicBool,
}

/// The plugin instance an editor is for: held by the [`Editor`] alone. The
/// registry's weak reference to it is upgraded only on the main thread, where
/// the editor lives, so the last owner of the handle drops there.
#[derive(Debug)]
struct EditorOwner {
    handle: PluginHandle,
}

#[derive(Debug)]
struct Entry {
    flags: Weak<EditorFlags>,
    owner: Weak<EditorOwner>,
}

/// The editors that are alive: weak, so an editor the app dropped is not kept.
#[derive(Debug, Default)]
pub(crate) struct Editors {
    live: Vec<Entry>,
}

impl Editors {
    /// An editor for the plugin `handle` at `node`, closed, kept in the
    /// registry.
    pub(crate) fn make(
        &mut self,
        node: NodeKey,
        handle: PluginHandle,
        main: MainThreadMarker,
    ) -> Editor {
        let flags = Arc::new(EditorFlags {
            node,
            ended: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        let owner = Arc::new(EditorOwner { handle });
        self.live.retain(|e| e.flags.strong_count() > 0);
        self.live.push(Entry {
            flags: Arc::downgrade(&flags),
            owner: Arc::downgrade(&owner),
        });
        Editor {
            flags,
            owner,
            main,
            opened: false,
        }
    }

    /// The editors of the plugin at `node` end: it was removed or replaced.
    /// Control thread: reads flags only.
    pub(crate) fn end(&mut self, node: NodeKey) {
        for flags in self.live.iter().filter_map(|e| e.flags.upgrade()) {
            if flags.node == node {
                flags.ended.store(true, Ordering::Release);
            }
        }
    }

    /// How many ended editors still wait to be closed on the main thread.
    pub(crate) fn waiting(&self) -> usize {
        self.live
            .iter()
            .filter_map(|e| e.flags.upgrade())
            .filter(|f| f.ended.load(Ordering::Acquire) && !f.closed.load(Ordering::Acquire))
            .count()
    }

    /// Closes the plugin side of every ended editor, on the main thread, and
    /// forgets them.
    pub(crate) fn close_ended(&mut self, main: MainThreadMarker) {
        for entry in &self.live {
            let (Some(flags), Some(owner)) = (entry.flags.upgrade(), entry.owner.upgrade()) else {
                continue;
            };
            if flags.ended.load(Ordering::Acquire) {
                if owner.handle.is_editor_open() {
                    owner.handle.close_editor(main);
                }
                flags.closed.store(true, Ordering::Release);
            }
        }
        self.live.retain(|e| {
            e.flags
                .upgrade()
                .is_some_and(|f| !f.ended.load(Ordering::Acquire))
        });
    }

    /// The idle tick of every open editor.
    pub(crate) fn service(&mut self, main: MainThreadMarker) {
        self.live.retain(|e| e.flags.strong_count() > 0);
        for entry in &self.live {
            let (Some(flags), Some(owner)) = (entry.flags.upgrade(), entry.owner.upgrade()) else {
                continue;
            };
            if !flags.ended.load(Ordering::Acquire) && owner.handle.is_editor_open() {
                owner.handle.editor_idle(main);
            }
        }
    }
}

impl<K: crate::AppKey, C: Clock> Session<K, C> {
    /// The editor of the plugin hosted at `node`, for the caller that holds
    /// the proof ([`MainThreadMarker::new`]). It is closed until
    /// [`Editor::open`]; see the module docs.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when no plugin is hosted at `node`.
    pub fn editor(
        &mut self,
        node: impl Into<NodeKey>,
        main: &MainThreadMarker,
    ) -> Result<Editor, NoSuchNode> {
        let node = node.into();
        let hosted = self
            .graph
            .plugins
            .get(&node)
            .ok_or_else(|| NoSuchNode::new(node))?;
        let handle = hosted.handle.clone();
        Ok(self.graph.graveyard.editors.make(node, handle, *main))
    }
}

/// Why an [`Editor`] call did not run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EditorCallError {
    /// The plugin this editor was made for is gone: its node was removed, or
    /// the plugin was replaced or reloaded. Ask the session for a new editor.
    #[error("the editor's plugin is gone: its node was removed or the plugin replaced")]
    Ended,
    /// The plugin's editor is open already (through this editor or another
    /// for the same plugin): close it first.
    #[error("the plugin's editor is open already")]
    AlreadyOpen,
    /// The plugin refused.
    #[error(transparent)]
    Plugin(#[from] EditorError),
}

/// Whether the editor embedded in the parent the host gave, or the plugin
/// opened a window of its own ([`Editor::open`]), and whether the editor was
/// brought in step with the plugin's state.
///
/// An editor that could not be mirrored (`mirrored` is an error) still opens:
/// it shows the plugin at its defaults, and the plugin keeps its own state.
/// The failure is counted too
/// (`mirror_writes_refused` of the plugin's diagnostics).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Opened {
    /// Embedded in the parent window, at this size: the host sizes the parent
    /// to it. The size names its unit: a [`Size::Physical`] where the plugin's
    /// format counts in device pixels, a [`Size::Logical`] where it counts in
    /// points (macOS, and AU), so a windowing frontend that sizes its windows
    /// in `dpi` units, as winit does, passes it on as it is.
    Embedded {
        /// The size the plugin's editor asked for.
        size: Size,
        /// Whether the editor holds the plugin's state.
        mirrored: Result<(), EditorMirrorError>,
    },
    /// The plugin made a floating window of its own (a CLAP plugin that cannot
    /// embed): there is no size, and nothing to lay out.
    Floating {
        /// Whether the editor holds the plugin's state.
        mirrored: Result<(), EditorMirrorError>,
    },
}

/// A hosted plugin's editor, made by [`Session::editor`].
///
/// `!Send`: it holds the main-thread proof, and its drop closes the editor on
/// the thread that made it. It holds its own handle to the plugin, so it is
/// the plugin instance it was made for that it serves, not whatever a later
/// replacement puts at the node.
#[derive(Debug)]
pub struct Editor {
    flags: Arc<EditorFlags>,
    owner: Arc<EditorOwner>,
    main: MainThreadMarker,
    /// Whether this editor opened the plugin's editor: only that one closes it
    /// on drop.
    opened: bool,
}

impl Editor {
    /// The node of the plugin this editor is for.
    pub fn node(&self) -> NodeKey {
        self.flags.node
    }

    fn live(&self) -> Result<&PluginHandle, EditorCallError> {
        if self.flags.ended.load(Ordering::Acquire) {
            return Err(EditorCallError::Ended);
        }
        Ok(&self.owner.handle)
    }

    /// Whether the plugin has an editor to open.
    pub fn exists(&self) -> bool {
        self.owner.handle.has_editor()
    }

    /// Whether the plugin's editor is open (the plugin handle's one reading,
    /// whichever editor of the plugin opened it) and this editor's plugin
    /// still serves the node.
    pub fn is_open(&self) -> bool {
        self.live().is_ok() && self.owner.handle.is_editor_open()
    }

    /// Opens the editor in `window` (a winit window, or any
    /// `raw-window-handle` 0.6 window), whose backing-store scale is `scale`:
    /// a window handle carries none. A plugin that can only float
    /// ([`Features::EDITOR_FLOATING`]) opens its own window instead and `window`
    /// is not used. From then on [`Session::update`] services it.
    ///
    /// Close it before the window is destroyed.
    ///
    /// # Errors
    ///
    /// [`EditorCallError::Ended`] for a plugin that is gone,
    /// [`EditorCallError::AlreadyOpen`] when its editor is open. Otherwise the
    /// plugin has no editor, refuses it, or `window` has no handle a plugin
    /// can embed into.
    pub fn open(
        &mut self,
        window: &impl HasWindowHandle,
        scale: ContentScale,
    ) -> Result<Opened, EditorCallError> {
        let main = self.main;
        let handle = self.live()?;
        if handle.is_editor_open() {
            return Err(EditorCallError::AlreadyOpen);
        }
        let opened = if handle.loaded().features.contains(Features::EDITOR_FLOATING) {
            Opened::Floating {
                mirrored: handle.open_floating_editor(main)?.mirrored,
            }
        } else {
            let parent = WindowHandle::from_window(window)?;
            let opened = handle.open_editor(main, parent, scale)?;
            Opened::Embedded {
                size: opened.window,
                mirrored: opened.mirrored,
            }
        };
        self.opened = true;
        Ok(opened)
    }

    /// The size the open editor asked for since the last call, if it did
    /// (answer it with [`resize`](Self::resize), or size the window to it):
    /// each request is read once. `None` when it asked for none, or the
    /// editor is closed or ended.
    pub fn requested_size(&mut self) -> Option<Size> {
        let handle = self.live().ok()?;
        if !handle.is_editor_open() {
            return None;
        }
        handle.poll_editor_resize_request(self.main)
    }

    /// Asks the plugin to take `requested` (what
    /// [`requested_size`](Self::requested_size) answered, or the host's own
    /// resize of its window) and returns the size it took, which may be
    /// snapped or clamped. `requested` may be in either unit: the format
    /// converts it at the scale the editor was last given. The size taken is
    /// in the format's own unit, as [`Opened::Embedded`]'s `size` is.
    ///
    /// # Errors
    ///
    /// [`EditorCallError::Ended`], or no editor is open, or the plugin cannot
    /// resize.
    pub fn resize(&mut self, requested: Size) -> Result<Size, EditorCallError> {
        Ok(self.live()?.set_editor_size(self.main, requested)?)
    }

    /// Tells the open editor its window's backing-store scale changed (it moved
    /// to another monitor), without closing it: CLAP's `gui.set_scale`, VST3's
    /// `setContentScaleFactor`. An out-of-process plugin's editor is the editor-only
    /// instance in this process, so this reaches it as it is.
    ///
    /// # Errors
    ///
    /// [`EditorError::ScaleUnsupported`] where the format takes the scale only at
    /// open (VST2) or the OS scales the window (AU, and any logical-pixel window
    /// such as Cocoa's): there is nothing to do, and the host keeps its window
    /// as it is. [`EditorCallError::Ended`], or no editor is open, or the
    /// plugin refused.
    pub fn rescale(&mut self, scale: ContentScale) -> Result<(), EditorCallError> {
        Ok(self.live()?.set_editor_scale(self.main, scale)?)
    }

    /// Forwards a key the host's window received to the open editor.
    ///
    /// # Errors
    ///
    /// [`EditorError::KeysUnsupported`] where the format takes no keys in its editor.
    /// [`EditorCallError::Ended`], or no editor is open, or the plugin refused.
    pub fn editor_key(&mut self, event: KeyEvent) -> Result<KeyUse, EditorCallError> {
        Ok(self.live()?.editor_key(self.main, event)?)
    }

    /// Closes the editor this one opened; the plugin stays loaded. Nothing
    /// happens when this editor opened none, or the plugin's editor is already
    /// closed (by the main thread's update after the editor ended).
    pub fn close(&mut self) {
        self.release();
    }

    /// Closes the plugin's editor once, and only where this editor opened it
    /// and nothing closed it since.
    fn release(&mut self) {
        if std::mem::take(&mut self.opened)
            && !self.flags.closed.load(Ordering::Acquire)
            && self.owner.handle.is_editor_open()
        {
            self.owner.handle.close_editor(self.main);
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        self.release();
    }
}
