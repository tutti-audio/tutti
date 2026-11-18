use super::{EditorUnit, ModifierConvention};

pub(super) const HOST: ModifierConvention = ModifierConvention::Pc;

pub(super) const WINDOWING: EditorUnit = EditorUnit::Physical;

#[cfg(test)]
mod tests {
    use super::EditorUnit;

    /// Win32 (`HWND`) and X11 windows are measured in device pixels.
    #[test]
    fn win32_and_x11_windows_are_measured_in_physical_pixels() {
        assert_eq!(EditorUnit::WINDOWING, EditorUnit::Physical);
    }
}
