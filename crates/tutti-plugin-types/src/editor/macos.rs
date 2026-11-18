use super::{EditorUnit, ModifierConvention};

pub(super) const HOST: ModifierConvention = ModifierConvention::Mac;

pub(super) const WINDOWING: EditorUnit = EditorUnit::Logical;

#[cfg(test)]
mod tests {
    use super::EditorUnit;

    /// AppKit measures a window in points, which the window system scales to
    /// the display (`NSView` geometry is logical).
    #[test]
    fn appkit_windows_are_measured_in_logical_points() {
        assert_eq!(EditorUnit::WINDOWING, EditorUnit::Logical);
    }
}
