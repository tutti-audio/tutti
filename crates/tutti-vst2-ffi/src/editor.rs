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
        /// `VKEY_BACK`.
        Back,
        /// `VKEY_TAB`.
        Tab,
        /// `VKEY_CLEAR`.
        Clear,
        /// `VKEY_RETURN`.
        Return,
        /// `VKEY_PAUSE`.
        Pause,
        /// `VKEY_ESCAPE`.
        Escape,
        /// `VKEY_SPACE`.
        Space,
        /// `VKEY_NEXT`.
        Next,
        /// `VKEY_END`.
        End,
        /// `VKEY_HOME`.
        Home,
        /// `VKEY_LEFT`.
        Left,
        /// `VKEY_UP`.
        Up,
        /// `VKEY_RIGHT`.
        Right,
        /// `VKEY_DOWN`.
        Down,
        /// `VKEY_PAGEUP`.
        PageUp,
        /// `VKEY_PAGEDOWN`.
        PageDown,
        /// `VKEY_SELECT`.
        Select,
        /// `VKEY_PRINT`.
        Print,
        /// `VKEY_ENTER`.
        Enter,
        /// `VKEY_SNAPSHOT`.
        Snapshot,
        /// `VKEY_INSERT`.
        Insert,
        /// `VKEY_DELETE`.
        Delete,
        /// `VKEY_HELP`.
        Help,
        /// `VKEY_NUMPAD0`.
        Numpad0,
        /// `VKEY_NUMPAD1`.
        Numpad1,
        /// `VKEY_NUMPAD2`.
        Numpad2,
        /// `VKEY_NUMPAD3`.
        Numpad3,
        /// `VKEY_NUMPAD4`.
        Numpad4,
        /// `VKEY_NUMPAD5`.
        Numpad5,
        /// `VKEY_NUMPAD6`.
        Numpad6,
        /// `VKEY_NUMPAD7`.
        Numpad7,
        /// `VKEY_NUMPAD8`.
        Numpad8,
        /// `VKEY_NUMPAD9`.
        Numpad9,
        /// `VKEY_MULTIPLY`.
        Multiply,
        /// `VKEY_ADD`.
        Add,
        /// `VKEY_SEPARATOR`.
        Separator,
        /// `VKEY_SUBTRACT`.
        Subtract,
        /// `VKEY_DECIMAL`.
        Decimal,
        /// `VKEY_DIVIDE`.
        Divide,
        /// `VKEY_F1`.
        F1,
        /// `VKEY_F2`.
        F2,
        /// `VKEY_F3`.
        F3,
        /// `VKEY_F4`.
        F4,
        /// `VKEY_F5`.
        F5,
        /// `VKEY_F6`.
        F6,
        /// `VKEY_F7`.
        F7,
        /// `VKEY_F8`.
        F8,
        /// `VKEY_F9`.
        F9,
        /// `VKEY_F10`.
        F10,
        /// `VKEY_F11`.
        F11,
        /// `VKEY_F12`.
        F12,
        /// `VKEY_NUMLOCK`.
        Numlock,
        /// `VKEY_SCROLL`.
        Scroll,
        /// `VKEY_SHIFT`.
        Shift,
        /// `VKEY_CONTROL`.
        Control,
        /// `VKEY_ALT`.
        Alt,
        /// `VKEY_EQUALS`.
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
