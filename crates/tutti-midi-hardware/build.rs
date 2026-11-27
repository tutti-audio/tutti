//! Turn the `tutti-alsa-ffi` probe result into this crate's cfgs.
//!
//! - `alsa_ump_create`: alsa-lib >= 1.2.13, which adds publishing our own UMP
//!   endpoint.
//! - `midi_backend`: the one platform decision, made by `backend_choice::choose`
//!   from the target OS and the backend features (`coremidi`, `midi-services`,
//!   `winmm`, `alsa`). `"coremidi"` on macOS, `"winmm"` or `"midi_services"` on
//!   Windows (`winmm` replaces `midi_services` when both are on), `"alsa"` on Linux
//!   with alsa-lib 1.2.10 or later (the UMP sequencer API that backend needs), each
//!   only with its feature on; `"none"` everywhere else. The `backend` module declares the
//!   platform module this names; no other file branches on the platform.
//!
//! An older or absent alsa-lib degrades to the empty stub rather than failing
//! the build; the ffi crate's build script prints the reason as a warning.

#[path = "backend_choice.rs"]
mod backend_choice;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=backend_choice.rs");
    // Declared unconditionally so `unexpected_cfgs` stays quiet where unset.
    println!("cargo:rustc-check-cfg=cfg(alsa_ump_create)");
    println!(
        r#"cargo:rustc-check-cfg=cfg(midi_backend, values("coremidi", "winmm", "midi_services", "alsa", "none"))"#
    );

    // Cargo exports the ffi crate's `cargo:KEY=` lines as `DEP_ASOUND_KEY`,
    // only on Linux, where it is a dependency.
    if std::env::var_os("DEP_ASOUND_ALSA_UMP_CREATE").is_some() {
        println!("cargo:rustc-cfg=alsa_ump_create");
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let on = |feature: &str| std::env::var_os(format!("CARGO_FEATURE_{feature}")).is_some();
    let backend = backend_choice::choose(
        &target_os,
        on("ALSA"),
        on("COREMIDI"),
        on("WINMM"),
        on("MIDI_SERVICES"),
        std::env::var_os("DEP_ASOUND_ALSA_UMP").is_some(),
    );
    println!("cargo:rustc-cfg=midi_backend=\"{backend}\"");
}
