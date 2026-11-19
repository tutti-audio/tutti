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

primitive_enum! {
    /// Platform independent key codes.
    #[allow(missing_docs)]
    #[derive(Debug, Copy, Clone)]
    pub enum Key: isize {
        None = 0,
        Back,
        Tab,
        Clear,
        Return,
        Pause,
        Escape,
        Space,
        Next,
        End,
        Home,
        Left,
        Up,
        Right,
        Down,
        PageUp,
        PageDown,
        Select,
        Print,
        Enter,
        Snapshot,
        Insert,
        Delete,
        Help,
        Numpad0,
        Numpad1,
        Numpad2,
        Numpad3,
        Numpad4,
        Numpad5,
        Numpad6,
        Numpad7,
        Numpad8,
        Numpad9,
        Multiply,
        Add,
        Separator,
        Subtract,
        Decimal,
        Divide,
        F1,
        F2,
        F3,
        F4,
        F5,
        F6,
        F7,
        F8,
        F9,
        F10,
        F11,
        F12,
        Numlock,
        Scroll,
        Shift,
        Control,
        Alt,
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
