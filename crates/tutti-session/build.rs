//! Defines `cfg(codec)`: `tutti-codec` was built with at least one `decode-*`
//! feature (it reports that through its `links` metadata), so a file can be
//! decoded and an `audio_file` entry can be loaded.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(codec)");
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("DEP_TUTTI_CODEC_DECODE").is_some() {
        println!("cargo::rustc-cfg=codec");
    }
}
