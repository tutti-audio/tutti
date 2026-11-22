//! The backend decision, shared by `build.rs` and the crate's unit test.

/// The `midi_backend` cfg value for a target OS and the enabled backend features.
///
/// A backend is chosen when the target is its OS and its feature is on; `alsa_ump` is
/// whether alsa-lib is at least 1.2.10 (the UMP sequencer API). Anything else is the
/// `"none"` stub.
pub(crate) fn choose(
    target_os: &str,
    alsa: bool,
    coremidi: bool,
    winmm: bool,
    alsa_ump: bool,
) -> &'static str {
    match target_os {
        "macos" if coremidi => "coremidi",
        "windows" if winmm => "winmm",
        "linux" if alsa && alsa_ump => "alsa",
        _ => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::choose;

    #[test]
    fn a_backend_is_chosen_only_on_its_os_with_its_feature_on() {
        // (os, alsa, coremidi, winmm, alsa_ump) -> backend
        let table = [
            ("linux", true, false, false, true, "alsa"),
            ("linux", true, true, true, true, "alsa"),
            ("linux", true, true, true, false, "none"),
            ("linux", false, true, true, true, "none"),
            ("macos", false, true, false, false, "coremidi"),
            ("macos", true, false, true, true, "none"),
            ("windows", false, false, true, false, "winmm"),
            ("windows", true, true, false, true, "none"),
            ("freebsd", true, true, true, true, "none"),
        ];
        for (os, alsa, coremidi, winmm, ump, want) in table {
            assert_eq!(
                choose(os, alsa, coremidi, winmm, ump),
                want,
                "{os} {alsa} {coremidi} {winmm} {ump}"
            );
        }
    }
}
