//! All VST plugin editor related functionality.

/// Rectangle used to specify dimensions of editor window.
#[doc(hidden)]
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct Rect {
    /// Y value in pixels of top side.
    pub top: i16,
    /// X value in pixels of left side.
    pub left: i16,
    /// Y value in pixels of bottom side.
    pub bottom: i16,
    /// X value in pixels of right side.
    pub right: i16,
}

/// A platform independent key code. Includes modifier keys.
#[derive(Copy, Clone, Debug)]
pub struct KeyCode {
    /// ASCII character for key pressed (if applicable).
    pub character: char,
    /// Key pressed. See `enums::Key`.
    pub key: Key,
    /// Modifier key bitflags. See `enums::flags::modifier_key`.
    pub modifier: u8,
}

use crate::primitive_enum::primitive_enum;

primitive_enum! {
    /// Platform independent key codes.
    #[derive(Debug, Copy, Clone)]
    pub enum Key: isize {
        /// No key.
        None = 0,
        /// The SDK's VKEY_BACK.
        Back,
        /// The SDK's VKEY_TAB.
        Tab,
        /// The SDK's VKEY_CLEAR.
        Clear,
        /// The SDK's VKEY_RETURN.
        Return,
        /// The SDK's VKEY_PAUSE.
        Pause,
        /// The SDK's VKEY_ESCAPE.
        Escape,
        /// The SDK's VKEY_SPACE.
        Space,
        /// The SDK's VKEY_NEXT.
        Next,
        /// The SDK's VKEY_END.
        End,
        /// The SDK's VKEY_HOME.
        Home,
        /// The SDK's VKEY_LEFT.
        Left,
        /// The SDK's VKEY_UP.
        Up,
        /// The SDK's VKEY_RIGHT.
        Right,
        /// The SDK's VKEY_DOWN.
        Down,
        /// The SDK's VKEY_PAGEUP.
        PageUp,
        /// The SDK's VKEY_PAGEDOWN.
        PageDown,
        /// The SDK's VKEY_SELECT.
        Select,
        /// The SDK's VKEY_PRINT.
        Print,
        /// The SDK's VKEY_ENTER.
        Enter,
        /// The SDK's VKEY_SNAPSHOT.
        Snapshot,
        /// The SDK's VKEY_INSERT.
        Insert,
        /// The SDK's VKEY_DELETE.
        Delete,
        /// The SDK's VKEY_HELP.
        Help,
        /// The SDK's VKEY_NUMPAD0.
        Numpad0,
        /// The SDK's VKEY_NUMPAD1.
        Numpad1,
        /// The SDK's VKEY_NUMPAD2.
        Numpad2,
        /// The SDK's VKEY_NUMPAD3.
        Numpad3,
        /// The SDK's VKEY_NUMPAD4.
        Numpad4,
        /// The SDK's VKEY_NUMPAD5.
        Numpad5,
        /// The SDK's VKEY_NUMPAD6.
        Numpad6,
        /// The SDK's VKEY_NUMPAD7.
        Numpad7,
        /// The SDK's VKEY_NUMPAD8.
        Numpad8,
        /// The SDK's VKEY_NUMPAD9.
        Numpad9,
        /// The SDK's VKEY_MULTIPLY.
        Multiply,
        /// The SDK's VKEY_ADD.
        Add,
        /// The SDK's VKEY_SEPARATOR.
        Separator,
        /// The SDK's VKEY_SUBTRACT.
        Subtract,
        /// The SDK's VKEY_DECIMAL.
        Decimal,
        /// The SDK's VKEY_DIVIDE.
        Divide,
        /// The SDK's VKEY_F1.
        F1,
        /// The SDK's VKEY_F2.
        F2,
        /// The SDK's VKEY_F3.
        F3,
        /// The SDK's VKEY_F4.
        F4,
        /// The SDK's VKEY_F5.
        F5,
        /// The SDK's VKEY_F6.
        F6,
        /// The SDK's VKEY_F7.
        F7,
        /// The SDK's VKEY_F8.
        F8,
        /// The SDK's VKEY_F9.
        F9,
        /// The SDK's VKEY_F10.
        F10,
        /// The SDK's VKEY_F11.
        F11,
        /// The SDK's VKEY_F12.
        F12,
        /// The SDK's VKEY_NUMLOCK.
        Numlock,
        /// The SDK's VKEY_SCROLL.
        Scroll,
        /// The SDK's VKEY_SHIFT.
        Shift,
        /// The SDK's VKEY_CONTROL.
        Control,
        /// The SDK's VKEY_ALT.
        Alt,
        /// The SDK's VKEY_EQUALS.
        Equals,
    }
}

#[cfg(test)]
mod tests {
    use super::Rect;

    /// `Rect` is read as the plugin's `ERect` (four `i16`: top, left, bottom,
    /// right), so its layout is `repr(C)`. No test can fail on the missing
    /// attribute: `repr(Rust)` happens to keep field order for four equal
    /// fields. This pins the layout the attribute guarantees.
    /// Spec: vst2.struct.ERect
    #[test]
    fn rect_has_the_erect_layout() {
        assert_eq!(std::mem::size_of::<Rect>(), 8);
        assert_eq!(std::mem::align_of::<Rect>(), 2);
        assert_eq!(std::mem::offset_of!(Rect, top), 0);
        assert_eq!(std::mem::offset_of!(Rect, left), 2);
        assert_eq!(std::mem::offset_of!(Rect, bottom), 4);
        assert_eq!(std::mem::offset_of!(Rect, right), 6);
    }
}
