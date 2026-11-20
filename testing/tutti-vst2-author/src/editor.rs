//! The editor trait a test plugin implements.

use std::os::raw::c_void;

pub use tutti_vst2_ffi::{Key, KeyCode, Rect};

/// Implemented by plugin editors.
#[allow(unused_variables)]
pub trait Editor {
    /// Get the size of the editor window.
    fn size(&self) -> (i32, i32);

    /// Get the coordinates of the editor window.
    fn position(&self) -> (i32, i32);

    /// Editor idle call. Called by host.
    fn idle(&mut self) {}

    /// Called when the editor window is closed.
    fn close(&mut self) {}

    /// Called when the editor window is opened.
    ///
    /// `parent` is a window pointer that the new window should attach itself to.
    /// **It is dependent upon the platform you are targeting.**
    ///
    /// A few examples:
    ///
    ///  - On Windows, it should be interpreted as a `HWND`
    ///  - On Mac OS X (64 bit), it should be interpreted as a `NSView*`
    ///  - On X11 platforms, it should be interpreted as a `u32` (the ID number of the parent window)
    ///
    /// Return `true` if the window opened successfully, `false` otherwise.
    fn open(&mut self, parent: *mut c_void) -> bool;

    /// Return whether the window is currently open.
    fn is_open(&mut self) -> bool;

    /// Receive key up event. Return `true` if the key was used.
    fn key_up(&mut self, keycode: KeyCode) -> bool {
        false
    }

    /// Receive key down event. Return `true` if the key was used.
    fn key_down(&mut self, keycode: KeyCode) -> bool {
        false
    }
}
