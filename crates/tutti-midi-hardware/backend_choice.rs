//! The backend decision, shared by `build.rs` and the crate's unit test.

/// The `midi_backend` cfg value for a target OS and architecture and the enabled backend
/// features.
///
/// A backend is chosen when the target is its OS and its feature is on; the web backend
/// is the target `wasm32-unknown-unknown` (an architecture and the OS `unknown`), which has no
/// feature; `alsa_ump` is
/// whether alsa-lib is at least 1.2.10 (the UMP sequencer API). On Windows `winmm` wins
/// over `midi_services`: the feature replaces the default backend. Anything else is the
/// `"none"` stub.
pub(crate) fn choose(
    target_os: &str,
    target_arch: &str,
    alsa: bool,
    coremidi: bool,
    winmm: bool,
    midi_services: bool,
    alsa_ump: bool,
) -> &'static str {
    match target_os {
        "unknown" if target_arch == "wasm32" => "web",
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
    fn a_backend_is_chosen_only_on_its_target_with_its_feature_on_and_winmm_replaces_midi_services()
    {
        // (os, arch, alsa, coremidi, winmm, midi_services, alsa_ump) -> backend
        let table = [
            ("linux", "x86_64", true, false, false, false, true, "alsa"),
            ("linux", "x86_64", true, true, true, true, true, "alsa"),
            ("linux", "x86_64", true, true, true, true, false, "none"),
            ("linux", "x86_64", false, true, true, true, true, "none"),
            (
                "macos", "x86_64", false, true, false, false, false, "coremidi",
            ),
            ("macos", "x86_64", true, false, true, true, true, "none"),
            (
                "windows", "x86_64", false, false, true, false, false, "winmm",
            ),
            (
                "windows",
                "x86_64",
                false,
                false,
                false,
                true,
                false,
                "midi_services",
            ),
            (
                "windows", "x86_64", false, false, true, true, false, "winmm",
            ),
            ("windows", "x86_64", true, true, false, false, true, "none"),
            ("freebsd", "x86_64", true, true, true, true, true, "none"),
            (
                "unknown", "wasm32", false, false, false, false, false, "web",
            ),
            ("unknown", "wasm32", true, true, true, true, true, "web"),
            ("unknown", "x86_64", true, true, true, true, true, "none"),
            ("linux", "wasm32", false, false, false, false, false, "none"),
        ];
        for (os, arch, alsa, coremidi, winmm, services, ump, want) in table {
            assert_eq!(
                choose(os, arch, alsa, coremidi, winmm, services, ump),
                want,
                "{os} {arch} {alsa} {coremidi} {winmm} {services} {ump}"
            );
        }
    }
}
