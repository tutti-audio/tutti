//! The backend decision, shared by `build.rs` and the crate's unit test.

/// The `midi_backend` cfg value for a target OS and the enabled backend features.
///
/// A backend is chosen when the target is its OS and its feature is on; `alsa_ump` is
/// whether alsa-lib is at least 1.2.10 (the UMP sequencer API). On Windows `winmm` wins
/// over `midi_services`: the feature replaces the default backend. Anything else is the
/// `"none"` stub.
pub(crate) fn choose(
    target_os: &str,
    alsa: bool,
    coremidi: bool,
    winmm: bool,
    midi_services: bool,
    alsa_ump: bool,
) -> &'static str {
    match target_os {
        "macos" if coremidi => "coremidi",
        "windows" if winmm => "winmm",
        "windows" if midi_services => "midi_services",
        "linux" if alsa && alsa_ump => "alsa",
        _ => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::choose;

    #[test]
    fn a_backend_is_chosen_only_on_its_os_with_its_feature_on_and_winmm_replaces_midi_services() {
        // (os, alsa, coremidi, winmm, midi_services, alsa_ump) -> backend
        let table = [
            ("linux", true, false, false, false, true, "alsa"),
            ("linux", true, true, true, true, true, "alsa"),
            ("linux", true, true, true, true, false, "none"),
            ("linux", false, true, true, true, true, "none"),
            ("macos", false, true, false, false, false, "coremidi"),
            ("macos", true, false, true, true, true, "none"),
            ("windows", false, false, true, false, false, "winmm"),
            ("windows", false, false, false, true, false, "midi_services"),
            ("windows", false, false, true, true, false, "winmm"),
            ("windows", true, true, false, false, true, "none"),
            ("freebsd", true, true, true, true, true, "none"),
        ];
        for (os, alsa, coremidi, winmm, services, ump, want) in table {
            assert_eq!(
                choose(os, alsa, coremidi, winmm, services, ump),
                want,
                "{os} {alsa} {coremidi} {winmm} {services} {ump}"
            );
        }
    }
}
