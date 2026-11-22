//! `check-layout`: the repo layout and the crate layering, read from the
//! workspace's manifests (no build, no `cargo metadata`, no `cargo tree`).
//!
//! Layout:
//! - every workspace member under `crates/` is directly `crates/<package-name>`,
//!   a nested directory is refused, and so is a directory under `crates/` or
//!   `testing/` that is not a member;
//! - every member under `testing/` is directly `testing/<dir>`;
//! - no member lives anywhere else but `xtask`, whose manifest has no dependency
//!   at all (std only) and which nothing depends on;
//! - nothing under `crates/` has a normal or build dependency on `testing/`;
//! - no member's manifest names a dependency called `bevy` or `bevy_*`/`bevy-*`, in
//!   any dependency table (normal, build, dev, target-specific).
//!
//! Layering (CLAUDE.md, "Workspace"): a normal or build dependency between
//! workspace crates points strictly down [`LAYERS`]; a crate missing from the
//! table is an error, because placing it is the point. Cargo already refuses a
//! cycle; it does not refuse a dependency that points sideways or up. On top of
//! that [`ONLY`] fixes the whole dependency set of the crates whose purity is
//! the point (a layer alone would let a layer-1 crate depend on any layer-0
//! one), and a few named bans follow in [`check`].
//!
//! The codec libraries are `tutti-codec`'s alone, and `winnow` is named in
//! [`WINNOW_HOMES`]'s parser modules alone; in `tutti-codec`'s manifest a
//! `decode-*` feature enables no encoder library and an `encode-*` feature no
//! `symphonia`.
//!
//! [`NAME_BANS`] holds a crate's source to a list of banned words (`tutti-units`
//! allocates nothing and shares no state: no `alloc`, `Vec`, `String`, `Box`,
//! `Arc`, `atomic` or `Atomic*` outside `#[cfg(test)]` items, `tests` directories
//! and `tests.rs` files; `check-no-std` refuses `alloc` but not
//! `core::sync::atomic`).
//!
//! A rule the layers already state is not stated twice: `tutti-units`,
//! `tutti-plugin-ipc-ffi` and every other layer-0 crate depend on no workspace
//! crate because nothing sits below layer 0.

use super::lex::{blank, ident_at, is_test_file, line_of, skipped, test_ranges};
use super::toml::{Kind, Toml, strings};
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// Directories under `crates/` or `testing/` that hold data, not a package: the
/// spec inventories `check-conformance` reads.
const DATA_DIRS: &[&str] = &["testing/conformance"];

/// Lowest first. A normal or build dependency must land in a strictly lower layer.
pub(super) const LAYERS: &[&[&str]] = &[
    &["tutti-platform"],
    &["tutti-units", "tutti-plugin-ipc-ffi"],
    &["tutti-music", "tutti-buffers"],
    &["tutti-ump"],
    &[
        "tutti-alsa-ffi",
        "tutti-coremidi-ffi",
        "tutti-winmm-ffi",
        "tutti-midi-protocol",
        "tutti-midi-ci",
        "tutti-curve",
    ],
    &["tutti-rt", "tutti-midi-device"],
    &[
        "tutti-midi-network",
        "tutti-graph",
        "tutti-dsp",
        "tutti-modulation",
        "tutti-codec",
        "tutti-tempo",
        "tutti-voicing",
        "tutti-sf2",
    ],
    &["tutti-timeline", "tutti-plugin-types", "tutti-measure"],
    &[
        "tutti-engine",
        "tutti-nodes",
        "tutti-midi-runtime",
        "tutti-midi-file",
        "tutti-clap-ffi",
        "tutti-vst3-ffi",
        "tutti-au-ffi",
        "tutti-vst2-ffi",
        "tutti-plugin-db",
        "tutti-plugin-ipc",
    ],
    &[
        "tutti-cpal",
        "tutti-au-host",
        "tutti-export",
        "tutti-sampler",
        "tutti-polysynth",
        "tutti-soundfont",
        "tutti-midi-hardware",
        "tutti-clap-host",
        "tutti-vst3-host",
        "tutti-vst2-host",
        "tutti-plugin-host",
    ],
    &["tutti-plugin", "tutti-plugin-probe"],
    &["tutti-plugin-server", "tutti-session"],
    &["tutti"],
];

/// The crates whose normal and build dependencies on workspace crates are exactly these.
const ONLY: &[(&str, &[&str])] = &[
    // The platform's edges name no workspace crate: every other crate may depend on it.
    ("tutti-platform", &[]),
    (
        "tutti-midi-protocol",
        &["tutti-ump", "tutti-music", "tutti-units"],
    ),
    (
        "tutti-midi-ci",
        &["tutti-ump", "tutti-units", "tutti-platform"],
    ),
    (
        "tutti-midi-device",
        &[
            "tutti-midi-ci",
            "tutti-ump",
            "tutti-units",
            "tutti-platform",
        ],
    ),
    // The OS edges: the UMP types for what the spec defines and the platform's clock, nothing else.
    ("tutti-alsa-ffi", &["tutti-ump", "tutti-platform"]),
    ("tutti-coremidi-ffi", &["tutti-ump", "tutti-platform"]),
    ("tutti-winmm-ffi", &["tutti-ump", "tutti-platform"]),
    ("tutti-ump", &["tutti-units", "tutti-music"]),
    // The unit vocabulary a `Gauge<U>` and a `Param<U>` hold (`Reading` for a `Unit`), the
    // channel layout a position ring is written in and the `Sample` formats the
    // audio-shaped structures are generic over.
    (
        "tutti-rt",
        &["tutti-units", "tutti-buffers", "tutti-platform"],
    ),
    // `Mutex`, the lock that refuses a wait inside a render.
    (
        "tutti-midi-network",
        &["tutti-ump", "tutti-units", "tutti-platform", "tutti-rt"],
    ),
    // `Exclusive`, which makes the probe's `Send` reader `Sync` without a lock.
    (
        "tutti-codec",
        &["tutti-units", "tutti-buffers", "tutti-platform", "tutti-rt"],
    ),
    ("tutti-tempo", &["tutti-units", "tutti-music"]),
    (
        "tutti-voicing",
        &[
            "tutti-ump",
            "tutti-units",
            "tutti-music",
            "tutti-midi-protocol",
        ],
    ),
    (
        "tutti-timeline",
        &[
            "tutti-music",
            "tutti-units",
            "tutti-buffers",
            "tutti-rt",
            "tutti-graph",
            "tutti-tempo",
        ],
    ),
    // The facade over the hosting: the generic host, the wire, the catalog and
    // the vocabulary, the graph a plugin is a node of, the executor a served
    // plugin's exit watch runs on, and the format hosts an in-process load or an
    // editor needs.
    (
        "tutti-plugin",
        &[
            "tutti-buffers",
            "tutti-graph",
            "tutti-platform",
            "tutti-plugin-db",
            "tutti-plugin-host",
            "tutti-plugin-ipc",
            "tutti-plugin-types",
            "tutti-rt",
            "tutti-timeline",
            "tutti-units",
            "tutti-au-host",
            "tutti-clap-host",
            "tutti-vst2-host",
            "tutti-vst3-host",
        ],
    ),
    // The probe client's child guard and its host-pid variable.
    (
        "tutti-plugin-db",
        &[
            "tutti-plugin-types",
            "tutti-rt",
            "tutti-plugin-ipc-ffi",
            "tutti-platform",
        ],
    ),
    (
        "tutti-plugin-ipc",
        &[
            "tutti-music",
            "tutti-units",
            "tutti-buffers",
            "tutti-rt",
            "tutti-ump",
            "tutti-plugin-types",
            "tutti-plugin-ipc-ffi",
            "tutti-platform",
        ],
    ),
    (
        "tutti-plugin-host",
        &[
            "tutti-plugin-types",
            "tutti-buffers",
            "tutti-graph",
            "tutti-timeline",
            "tutti-tempo",
            "tutti-music",
            "tutti-units",
            "tutti-rt",
            "tutti-ump",
            "tutti-plugin-ipc-ffi",
            "tutti-platform",
        ],
    ),
    // The server is `Ipc` hosting over the served formats: the wire, the vocabulary and the CLAP, VST3 and AU hosts.
    // Not the facade, not `-host`, not VST2 (not served: editor and audio are
    // one object): the banned ones below hold for a dev-dependency too.
    (
        "tutti-plugin-server",
        &[
            "tutti-buffers",
            "tutti-plugin-ipc",
            "tutti-plugin-ipc-ffi",
            "tutti-plugin-types",
            "tutti-units",
            "tutti-rt",
            "tutti-ump",
            "tutti-graph",
            "tutti-clap-host",
            "tutti-vst3-host",
            "tutti-au-host",
            "tutti-platform",
        ],
    ),
    // The probe reads one file as a format host would: the hosts, the descriptor
    // types, the block view it renders through and the experiment's units; and ends with the host that spawned it
    // (the process edge's watch on a host).
    // No hosting (`tutti-plugin-ipc`, `tutti-plugin-host`, `tutti-plugin`).
    (
        "tutti-plugin-probe",
        &[
            "tutti-buffers",
            "tutti-plugin-types",
            "tutti-plugin-ipc-ffi",
            "tutti-graph",
            "tutti-ump",
            "tutti-units",
            "tutti-clap-host",
            "tutti-vst3-host",
            "tutti-vst2-host",
            "tutti-au-host",
            "tutti-platform",
        ],
    ),
    (
        "tutti-dsp",
        &["tutti-units", "tutti-music", "tutti-buffers", "tutti-rt"],
    ),
    ("tutti-curve", &["tutti-units"]),
    ("tutti-buffers", &["tutti-units"]),
    (
        "tutti-modulation",
        &[
            "tutti-units",
            "tutti-music",
            "tutti-curve",
            "tutti-ump",
            "tutti-rt",
        ],
    ),
    (
        "tutti-measure",
        &["tutti-music", "tutti-units", "tutti-buffers", "tutti-dsp"],
    ),
    (
        "tutti-graph",
        &[
            "tutti-music",
            "tutti-units",
            "tutti-buffers",
            "tutti-rt",
            "tutti-ump",
            "tutti-curve",
            "tutti-platform",
        ],
    ),
];

const CODEC_LIBS: &[&str] = &["symphonia", "flacenc", "aifc"];
/// The platform libraries `tutti-platform` alone names, in either spelling.
const PLATFORM_LIBS: &[&str] = &["web-time", "web_time", "thread-priority", "thread_priority"];
const ENCODERS: &[&str] = &["flacenc", "aifc", "md-5"];

/// A crate that names `winnow`, and the parser modules that may.
pub(super) const WINNOW_HOMES: &[(&str, &[&str])] = &[
    (
        "tutti-midi-file",
        &[
            "crates/tutti-midi-file/src/smf/read.rs",
            "crates/tutti-midi-file/src/clip_file/read.rs",
        ],
    ),
    (
        "tutti-midi-protocol",
        &[
            "crates/tutti-midi-protocol/src/universal/read.rs",
            "crates/tutti-midi-protocol/src/universal/machine_control/read.rs",
        ],
    ),
    (
        "tutti-midi-ci",
        &[
            "crates/tutti-midi-ci/src/read.rs",
            "crates/tutti-midi-ci/src/pe/json.rs",
        ],
    ),
];

/// A crate whose code outside tests names none of these words, and why. A word
/// ending in `*` bans every identifier it starts. Comments and literals are
/// not code; a type that merely shares a name is a finding too.
const NAME_BANS: &[(&str, &[&str], &str)] = &[(
    "tutti-units",
    &["alloc", "Vec", "String", "Box", "Arc", "atomic", "Atomic*"],
    "tutti-units has no allocation and no atomics; the name belongs in a crate above it",
)];

pub(super) struct Pkg {
    pub(super) name: String,
    pub(super) dir: String,
    toml: Toml,
    pub(super) deps: Vec<(String, Kind)>,
}

fn layer_of(name: &str) -> Option<usize> {
    LAYERS.iter().position(|l| l.contains(&name))
}

/// The workspace members' manifests, from the root's `members` list.
pub(super) fn packages(tree: &dyn Tree, bad: &mut Vec<String>) -> Vec<Pkg> {
    let root = Toml::parse(&tree.read("Cargo.toml").unwrap_or_default());
    let members = root
        .value("workspace", "members")
        .map(strings)
        .unwrap_or_default();
    if members.is_empty() {
        bad.push("Cargo.toml: no workspace members found; the scan is broken".into());
    }
    let mut out = Vec::new();
    for dir in members {
        let Some(text) = tree.read(&format!("{dir}/Cargo.toml")) else {
            bad.push(format!("{dir}: a workspace member with no Cargo.toml"));
            continue;
        };
        let toml = Toml::parse(&text);
        let Some(name) = toml
            .value("package", "name")
            .and_then(|v| strings(v).into_iter().next())
        else {
            bad.push(format!("{dir}/Cargo.toml: no package name"));
            continue;
        };
        let deps = toml.dependencies();
        out.push(Pkg {
            name,
            dir,
            toml,
            deps,
        });
    }
    out
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let pkgs = packages(tree, &mut bad);
    let by_name: BTreeMap<&str, &Pkg> = pkgs.iter().map(|p| (p.name.as_str(), p)).collect();

    // Layout.
    let mut member_dirs: BTreeSet<&str> = BTreeSet::new();
    for p in &pkgs {
        member_dirs.insert(&p.dir);
        let parts: Vec<&str> = p.dir.split('/').collect();
        match parts[0] {
            "crates" if parts.len() != 2 => bad.push(format!(
                "{}: {} is a nested directory under crates/; members sit directly in crates/",
                p.name, p.dir
            )),
            "crates" if parts[1] != p.name => {
                bad.push(format!("{}: {} must be crates/{}", p.name, p.dir, p.name));
            }
            "testing" if parts.len() != 2 => bad.push(format!(
                "{}: {} is a nested directory under testing/; members sit directly in testing/",
                p.name, p.dir
            )),
            "crates" | "testing" => {}
            _ if p.dir == "xtask" && p.name == "xtask" => {}
            _ => bad.push(format!(
                "{}: {} is outside crates/ and testing/ (vendor/ holds third-party code, never a member; xtask is the one exception)",
                p.name, p.dir
            )),
        }
    }
    for top in ["crates", "testing"] {
        let mut entries: BTreeSet<String> = BTreeSet::new();
        for f in tree.files(top, &skipped) {
            if let Some(e) = f[top.len() + 1..].split_once('/') {
                entries.insert(format!("{top}/{}", e.0));
            }
        }
        for dir in entries {
            if member_dirs.contains(dir.as_str()) || DATA_DIRS.contains(&dir.as_str()) {
                continue;
            }
            bad.push(format!("{dir} is not a workspace member"));
        }
    }

    let internal = |p: &Pkg| -> Vec<(String, Kind)> {
        p.deps
            .iter()
            .filter(|(d, _)| by_name.contains_key(d.as_str()) && *d != p.name)
            .cloned()
            .collect()
    };
    let dir_of = |n: &str| by_name[n].dir.as_str();

    // xtask stands alone.
    match by_name.get("xtask") {
        Some(x) => {
            if !x.deps.is_empty() {
                bad.push("xtask has dependencies; it is std only".into());
            }
            for (dep, kind) in internal(x) {
                bad.push(format!(
                    "xtask: {} dependency on {dep}; xtask depends on no workspace crate",
                    kind.label()
                ));
            }
            for p in &pkgs {
                if p.name != "xtask" && internal(p).iter().any(|(d, _)| d == "xtask") {
                    bad.push(format!(
                        "{} depends on xtask; nothing depends on it",
                        p.name
                    ));
                }
            }
        }
        None => bad.push("xtask is not a workspace member".into()),
    }

    // crates/ never builds on testing/.
    for p in pkgs.iter().filter(|p| p.dir.starts_with("crates/")) {
        for (dep, kind) in internal(p) {
            if kind != Kind::Dev && dir_of(&dep).starts_with("testing/") {
                bad.push(format!(
                    "{}: {} dependency on {dep}; crates/ depends on testing/",
                    p.name,
                    kind.label()
                ));
            }
        }
    }

    // Layering.
    for p in pkgs.iter().filter(|p| p.dir.starts_with("crates/")) {
        let Some(mine) = layer_of(&p.name) else {
            bad.push(format!(
                "{} is not in LAYERS; place it (xtask/src/gates/layout.rs)",
                p.name
            ));
            continue;
        };
        for (dep, kind) in internal(p) {
            let Some(theirs) = layer_of(&dep) else {
                continue;
            };
            if kind != Kind::Dev && theirs >= mine {
                bad.push(format!(
                    "{} -> {dep} ({}) points up or sideways (layer {mine} -> {theirs}); dependencies point down",
                    p.name,
                    kind.label()
                ));
            }
        }
    }

    // The codec libraries stay behind tutti-codec.
    for p in pkgs.iter().filter(|p| p.name != "tutti-codec") {
        for (dep, kind) in &p.deps {
            if CODEC_LIBS.contains(&dep.as_str()) && *kind != Kind::Dev {
                bad.push(format!(
                    "{dep} is a {} dependency of {}; the codec libraries are tutti-codec's alone",
                    kind.label(),
                    p.name
                ));
            }
        }
    }

    // The platform libraries stay behind tutti-platform, for every kind of dependency.
    for p in pkgs.iter().filter(|p| p.name != "tutti-platform") {
        for (dep, kind) in &p.deps {
            if PLATFORM_LIBS.contains(&dep.as_str()) {
                bad.push(format!(
                    "{dep} is a {} dependency of {}; the platform libraries are tutti-platform's alone",
                    kind.label(),
                    p.name
                ));
            }
        }
    }

    // `winnow` is a parser module's alone.
    let homes: Vec<&str> = WINNOW_HOMES.iter().map(|(c, _)| *c).collect();
    for p in &pkgs {
        for (dep, kind) in &p.deps {
            if dep == "winnow" && *kind != Kind::Dev && !homes.contains(&p.name.as_str()) {
                bad.push(format!(
                    "winnow is a {} dependency of {}; only {} may name it, in their listed parser modules",
                    kind.label(),
                    p.name,
                    homes.join(", ")
                ));
            }
        }
    }
    for (krate, modules) in WINNOW_HOMES {
        let dir = format!("crates/{krate}/src");
        for path in tree.files(&dir, &skipped) {
            if !path.ends_with(".rs") || modules.contains(&path.as_str()) {
                continue;
            }
            let text = tree.read(&path).unwrap_or_default();
            if blank(&text).contains("winnow") {
                bad.push(format!(
                    "{path} names winnow outside {}",
                    modules.join(", ")
                ));
            }
        }
    }

    // A crate's code names none of its banned words.
    for (krate, words, why) in NAME_BANS.iter().filter(|(k, ..)| by_name.contains_key(k)) {
        let mut read = 0;
        for path in tree.files(&format!("crates/{krate}/src"), &skipped) {
            if !path.ends_with(".rs") || is_test_file(&path) {
                continue;
            }
            read += 1;
            let code = blank(&tree.read(&path).unwrap_or_default());
            let ranges = test_ranges(&code);
            let mut at = 0;
            while at < code.len() {
                let (ident, end) = ident_at(&code, at);
                if ident.is_empty() {
                    at += code[at..].chars().next().map_or(1, char::len_utf8);
                    continue;
                }
                let start = end - ident.len();
                let banned = words.iter().any(|w| match w.strip_suffix('*') {
                    Some(prefix) => ident.starts_with(prefix),
                    None => ident == *w,
                });
                if banned && !ranges.iter().any(|&(s, e)| start >= s && start <= e) {
                    bad.push(format!(
                        "{path}:{}: `{ident}`: {why}",
                        line_of(&code, start)
                    ));
                }
                at = end;
            }
        }
        if read == 0 {
            bad.push(format!(
                "{krate}: no source file under crates/{krate}/src; the scan is broken"
            ));
        }
    }

    // A decoder's features pull no encoder, an encoder's no decoder.
    if let Some(codec) = by_name.get("tutti-codec")
        && let Some(features) = codec.toml.section("features")
    {
        for (feat, value) in &features.entries {
            let names: BTreeSet<String> = strings(value)
                .iter()
                .map(|e| {
                    let head = e.split('/').next().unwrap_or(e);
                    let head = head.strip_prefix("dep:").unwrap_or(head);
                    head.strip_suffix('?').unwrap_or(head).to_owned()
                })
                .collect();
            let hit: Vec<&str> = ENCODERS
                .iter()
                .copied()
                .filter(|e| names.contains(*e))
                .collect();
            if feat.starts_with("decode-") && !hit.is_empty() {
                bad.push(format!(
                    "{feat} enables an encoder library ({hit:?}); a build that reads pulls no encoder"
                ));
            }
            if feat.starts_with("encode-") && names.contains("symphonia") {
                bad.push(format!(
                    "{feat} enables symphonia; a build that writes pulls no decoder"
                ));
            }
        }
    }

    // Whole dependency sets.
    let deps_of = |name: &str, kinds: &[Kind]| -> BTreeSet<String> {
        by_name
            .get(name)
            .map(|p| {
                internal(p)
                    .into_iter()
                    .filter(|(_, k)| kinds.contains(k))
                    .map(|(d, _)| d)
                    .collect()
            })
            .unwrap_or_default()
    };
    let all = [Kind::Normal, Kind::Build, Kind::Dev];
    let built = [Kind::Normal, Kind::Build];
    for (name, allowed) in ONLY.iter().filter(|(n, _)| by_name.contains_key(n)) {
        let extra: BTreeSet<String> = deps_of(name, &built)
            .into_iter()
            .filter(|d| !allowed.contains(&d.as_str()))
            .collect();
        if !extra.is_empty() {
            let mut sorted = allowed.to_vec();
            sorted.sort_unstable();
            bad.push(format!(
                "{name} may depend on {sorted:?} only; it depends on {extra:?}"
            ));
        }
    }
    // The process edge knows the wire's vocabulary and no plugin format, and no
    // format host knows it: a format is served through it, never built on it.
    if by_name.contains_key("tutti-plugin-ipc") {
        for p in &pkgs {
            if p.name.starts_with("tutti-")
                && p.name.ends_with("-host")
                && deps_of(&p.name, &all).contains("tutti-plugin-ipc")
            {
                bad.push(format!(
                    "{} depends on tutti-plugin-ipc; a format host knows no hosting",
                    p.name
                ));
            }
        }
    }
    // The format-generic half of hosting names no facade above it, and the probe
    // binary none either, nor does anything depend on it (a binary).
    for (name, banned) in [
        ("tutti-plugin-host", "tutti-plugin"),
        ("tutti-plugin-probe", "tutti-plugin"),
        ("tutti-plugin-server", "tutti-plugin"),
        ("tutti-plugin-server", "tutti-plugin-host"),
        ("tutti-plugin-server", "tutti-vst2-host"),
    ] {
        if deps_of(name, &all).contains(banned) {
            bad.push(format!(
                "{name} depends on {banned}, even as a dev-dependency"
            ));
        }
    }
    for p in &pkgs {
        if deps_of(&p.name, &all).contains("tutti-plugin-probe") {
            bad.push(format!(
                "{} depends on tutti-plugin-probe; nothing does, it is a binary",
                p.name
            ));
        }
        let users: BTreeSet<String> = internal(p).into_iter().map(|(d, _)| d).collect();
        if users.contains("tutti-session") && p.name != "tutti" {
            bad.push(format!(
                "{} depends on tutti-session; only tutti does",
                p.name
            ));
        }
        // A testing crate may read the facade the way an app does (the book's
        // programs), through a dev-dependency; nothing shipped builds on it.
        let reads_facade = internal(p)
            .iter()
            .any(|(d, k)| d == "tutti" && !(*k == Kind::Dev && p.dir.starts_with("testing/")));
        if reads_facade {
            bad.push(format!("{} depends on tutti, the top of the tree", p.name));
        }
        for (d, _) in &p.deps {
            if d == "bevy" || d.starts_with("bevy_") || d.starts_with("bevy-") {
                bad.push(format!(
                    "{} depends on {d}; no workspace crate depends on Bevy (the adapter is its own repository)",
                    p.name
                ));
            }
        }
    }

    super::decode_boundary::check(tree, &mut bad);
    super::test_paths::check(tree, &mut bad);

    let mut r = Report::default();
    if bad.is_empty() {
        let ranked: usize = LAYERS.iter().map(|l| l.len()).sum();
        r.say(format!(
            "check-layout: ok ({} members, {ranked} ranked)",
            pkgs.len()
        ));
        r.say("check-layout: no depth-dependent test paths");
    } else {
        r.fail("check-layout: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    /// A workspace of `(dir, name, manifest body)` members.
    fn ws(members: &[(&str, &str, &str)]) -> Mem {
        let list: Vec<String> = members
            .iter()
            .map(|(d, ..)| format!("    \"{d}\","))
            .collect();
        let mut m = Mem::with(&[(
            "Cargo.toml",
            &format!("[workspace]\nmembers = [\n{}\n]\n", list.join("\n")),
        )]);
        for (dir, name, body) in members {
            m.files.insert(
                format!("{dir}/Cargo.toml"),
                format!("[package]\nname = \"{name}\"\n{body}"),
            );
            m.files.insert(format!("{dir}/src/lib.rs"), String::new());
        }
        m
    }

    fn base() -> Vec<(&'static str, &'static str, &'static str)> {
        vec![
            ("xtask", "xtask", "[dependencies]\n"),
            ("crates/tutti-units", "tutti-units", ""),
            ("crates/tutti-ump", "tutti-ump", ""),
            ("crates/tutti-rt", "tutti-rt", ""),
            (
                "crates/tutti-graph",
                "tutti-graph",
                "[dependencies]\ntutti-units.path = \"../tutti-units\"\n",
            ),
            ("testing/tutti-fixtures", "tutti-fixtures", ""),
        ]
    }

    fn run(members: &[(&str, &str, &str)]) -> Report {
        check(&ws(members))
    }

    fn with(extra: (&'static str, &'static str, &'static str)) -> Report {
        let mut v = base();
        v.retain(|m| m.1 != extra.1);
        v.push(extra);
        run(&v)
    }

    const UNITS_LIB: &str = "crates/tutti-units/src/lib.rs";

    fn units_with(files: &[(&str, &str)]) -> Report {
        let mut m = ws(&base());
        for (path, text) in files {
            m.files.insert((*path).to_owned(), (*text).to_owned());
        }
        check(&m)
    }

    #[test]
    fn a_tutti_units_without_sources_fails_the_scan() {
        let mut m = ws(&base());
        m.files.remove(UNITS_LIB);
        let r = check(&m);
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("tutti-units: no source file under crates/tutti-units/src")),
            "{:?}",
            r.findings
        );
    }

    #[test]
    fn every_banned_name_in_tutti_units_fails_with_its_line() {
        for (src, name) in [
            ("extern crate alloc;\n", "alloc"),
            ("use alloc::borrow::ToOwned;\n", "alloc"),
            ("fn f(_: Vec<u8>) {}\n", "Vec"),
            ("fn f(_: String) {}\n", "String"),
            ("fn f(_: Box<u8>) {}\n", "Box"),
            ("fn f(_: Arc<u8>) {}\n", "Arc"),
            ("use core::sync::atomic::Ordering;\n", "atomic"),
            ("struct C(core::sync::atomic::AtomicU64);\n", "AtomicU64"),
        ] {
            let r = units_with(&[(UNITS_LIB, &format!("\n{src}"))]);
            assert!(
                r.findings
                    .iter()
                    .any(|l| l.contains(&format!("{UNITS_LIB}:2: `{name}`"))),
                "{src}: {:?}",
                r.findings
            );
        }
    }

    #[test]
    fn prose_literals_tests_longer_names_and_other_crates_pass_in_tutti_units() {
        let src = "/// A `Vec` of `Arc`s in prose.\n\
                   fn f() -> &'static str { \"String Box alloc atomic\" }\n\
                   struct Vector; struct Stringly; struct Allocation; struct Atomically;\n\
                   #[cfg(test)]\nmod tests { fn t(_: Vec<u8>) {} }\n";
        // A name that merely starts with "Atomic" is the one deliberate over-match.
        let r = units_with(&[(UNITS_LIB, src)]);
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert!(r.findings[1].contains("`Atomically`"));
        let src = src.replace("struct Atomically;", "");
        let r = units_with(&[
            (UNITS_LIB, &src),
            (
                "crates/tutti-units/src/units/tests.rs",
                "fn t(_: Vec<u8>) {}\n",
            ),
            (
                "crates/tutti-units/src/units/x/tests/a.rs",
                "fn t(_: Vec<u8>) {}\n",
            ),
            ("crates/tutti-ump/src/lib.rs", "fn t(_: Vec<u8>) {}\n"),
        ]);
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_layered_workspace_passes() {
        let r = run(&base());
        assert!(r.passed(), "{:?}", r.findings);
        assert_eq!(r.out[0], "check-layout: ok (6 members, 51 ranked)");
    }

    #[test]
    fn a_dependency_that_points_up_or_sideways_fails() {
        let r = with((
            "crates/tutti-units",
            "tutti-units",
            "[dependencies]\ntutti-graph = { path = \"../tutti-graph\" }\n",
        ));
        assert!(r.findings.iter().any(|l| {
            l.contains("tutti-units -> tutti-graph (normal) points up or sideways (layer 1 -> 6)")
        }));
        // A dev-dependency may point anywhere.
        let r = with((
            "crates/tutti-units",
            "tutti-units",
            "[dev-dependencies]\ntutti-graph = { path = \"../tutti-graph\" }\n",
        ));
        assert!(r.passed());
        // A crate held to a list may not name another normal dependency, and its
        // dev-dependencies are free.
        let r = with((
            "crates/tutti-ump",
            "tutti-ump",
            "[dependencies]\ntutti-graph = { path = \"../tutti-graph\" }\n",
        ));
        assert!(r.findings.iter().any(|l| {
            l.contains("tutti-ump may depend on [\"tutti-music\", \"tutti-units\"] only")
        }));
        let r = with((
            "crates/tutti-ump",
            "tutti-ump",
            "[dependencies]\ntutti-units = { path = \"../tutti-units\" }\n[dev-dependencies]\ntutti-fixtures = { path = \"../../testing/tutti-fixtures\" }\n",
        ));
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_crate_not_in_layers_and_a_wrong_directory_fail() {
        let mut v = base();
        v.push(("crates/tutti-new", "tutti-new", ""));
        assert!(run(&v).findings[1].contains("tutti-new is not in LAYERS"));
        let mut v = base();
        v.push(("crates/modx", "tutti-curve", ""));
        assert!(
            run(&v)
                .findings
                .iter()
                .any(|l| l.contains("crates/modx must be crates/tutti-curve"))
        );
        let mut v = base();
        v.push(("crates/nest/tutti-curve", "tutti-curve", ""));
        assert!(
            run(&v)
                .findings
                .iter()
                .any(|l| l.contains("nested directory under crates/"))
        );
    }

    #[test]
    fn a_directory_that_is_not_a_member_fails() {
        let mut m = ws(&base());
        m.files
            .insert("crates/stray/Cargo.toml".into(), String::new());
        assert!(check(&m).findings[1].contains("crates/stray is not a workspace member"));
    }

    #[test]
    fn the_conformance_data_directory_is_not_a_member_and_not_a_stray() {
        let mut m = ws(&base());
        m.files
            .insert("testing/conformance/clap.txt".into(), String::new());
        assert!(check(&m).passed());
        m.files
            .insert("testing/conformance-other/x.txt".into(), String::new());
        assert!(
            check(&m)
                .findings
                .iter()
                .any(|l| l.contains("testing/conformance-other is not a workspace member"))
        );
    }

    #[test]
    fn a_bevy_dependency_in_any_table_fails_and_a_lookalike_passes() {
        for body in [
            "[dependencies]\nbevy_reflect = \"0.19\"\n",
            "[dev-dependencies]\nbevy = { version = \"0.19\" }\n",
            "[build-dependencies]\nbevy-tutti = { path = \"x\" }\n",
            "[target.'cfg(unix)'.dependencies]\nbevy_ecs = \"0.19\"\n",
            "[dependencies.bevy_tasks]\nversion = \"0.19\"\n",
        ] {
            let r = with(("crates/tutti-units", "tutti-units", body));
            assert!(
                r.findings
                    .iter()
                    .any(|l| l.contains("tutti-units depends on bevy")
                        && l.contains("no workspace crate depends on Bevy")),
                "{body}: {:?}",
                r.findings
            );
        }
        let r = with((
            "crates/tutti-units",
            "tutti-units",
            "[dependencies]\nbevyish = \"1\"\nthiserror = \"2\"\n",
        ));
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn crates_do_not_build_on_testing_and_xtask_stands_alone() {
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[build-dependencies]\ntutti-fixtures = { path = \"../../testing/tutti-fixtures\" }\n",
        ));
        assert!(r.findings.iter().any(|l| {
            l.contains("build dependency on tutti-fixtures; crates/ depends on testing/")
        }));
        let r = with((
            "xtask",
            "xtask",
            "[dependencies]\ntutti-ump = { path = \"../crates/tutti-ump\" }\n",
        ));
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("xtask has dependencies; it is std only"))
        );
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("xtask: normal dependency on tutti-ump"))
        );
        let r = with((
            "crates/tutti-ump",
            "tutti-ump",
            "[dev-dependencies]\nxtask = { path = \"../../xtask\" }\n",
        ));
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("tutti-ump depends on xtask"))
        );
    }

    #[test]
    fn a_crate_held_to_its_set_fails_on_one_more() {
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[dependencies]\ntutti-units.path = \"x\"\ntutti-rt = { path = \"x\" }\ntutti-curve = { path = \"x\" }\n",
        ));
        assert!(
            r.findings.iter().all(|l| !l.contains("may depend on")),
            "{:?}",
            r.findings
        );
        let mut v = base();
        v.push((
            "crates/tutti-curve",
            "tutti-curve",
            "[dependencies]\ntutti-units = { path = \"x\" }\ntutti-ump = { path = \"x\" }\n",
        ));
        let r = run(&v);
        assert!(r.findings.iter().any(|l| l.contains(
            "tutti-curve may depend on [\"tutti-units\"] only; it depends on {\"tutti-ump\"}"
        )));
    }

    #[test]
    fn an_os_edge_names_the_ump_types_and_the_platform_only() {
        let os = |dir, krate, dep| {
            let mut v = base();
            v.push(("crates/tutti-music", "tutti-music", ""));
            v.push((dir, krate, dep));
            run(&v)
        };
        for (dir, krate) in [
            ("crates/tutti-alsa-ffi", "tutti-alsa-ffi"),
            ("crates/tutti-coremidi-ffi", "tutti-coremidi-ffi"),
        ] {
            let r = os(
                dir,
                krate,
                "[dependencies]\ntutti-ump = { path = \"x\" }\ntutti-platform = { path = \"x\" }\n",
            );
            assert!(r.passed(), "{:?}", r.findings);
            let r = os(
                dir,
                krate,
                "[dependencies]\ntutti-music = { path = \"x\" }\n",
            );
            assert!(r.findings.iter().any(|l| l.contains(&format!(
                "{krate} may depend on [\"tutti-platform\", \"tutti-ump\"] only; it depends on {{\"tutti-music\"}}"
            ))));
        }
    }

    #[test]
    fn the_runtime_primitives_name_the_units_and_the_sample_formats_only() {
        let r = with((
            "crates/tutti-rt",
            "tutti-rt",
            "[dependencies]\ntutti-units = { path = \"x\" }\ntutti-buffers = { path = \"x\" }\n",
        ));
        assert!(r.passed(), "{:?}", r.findings);
        let r = with((
            "crates/tutti-rt",
            "tutti-rt",
            "[dependencies]\ntutti-units = { path = \"x\" }\ntutti-ump = { path = \"x\" }\n",
        ));
        assert!(r.findings.iter().any(|l| l.contains(
            "tutti-rt may depend on [\"tutti-buffers\", \"tutti-platform\", \"tutti-units\"] only; it depends on {\"tutti-ump\"}"
        )));
    }

    #[test]
    fn the_platform_libraries_are_the_platform_crates_alone() {
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[dependencies]\nweb-time = \"1\"\n[dev-dependencies]\nthread-priority = \"3\"\n",
        ));
        for lib in ["web-time is a normal", "thread-priority is a dev"] {
            assert!(
                r.findings.iter().any(|l| l.contains(lib)
                    && l.contains("the platform libraries are tutti-platform's alone")),
                "{lib}: {:?}",
                r.findings
            );
        }
        let mut v = base();
        v.push((
            "crates/tutti-platform",
            "tutti-platform",
            "[dependencies]\nweb-time = \"1\"\nthread-priority = \"3\"\n",
        ));
        let r = run(&v);
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn the_codec_libraries_and_winnow_have_homes() {
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[dependencies]\nsymphonia = \"0.5\"\nwinnow = \"1\"\n",
        ));
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("symphonia is a normal dependency of tutti-graph"))
        );
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("winnow is a normal dependency of tutti-graph"))
        );
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[dev-dependencies]\nsymphonia = \"0.5\"\n",
        ));
        assert!(r.passed());
        let mut m = ws(&base());
        m.files.insert(
            "crates/tutti-midi-file/src/lib.rs".into(),
            "// winnow\nuse winnow::Parser;\n".into(),
        );
        assert!(
            check(&m)
                .findings
                .iter()
                .any(|l| l.contains("crates/tutti-midi-file/src/lib.rs names winnow outside"))
        );
        m.files.insert(
            "crates/tutti-midi-file/src/lib.rs".into(),
            "// winnow\n".into(),
        );
        m.files.insert(
            "crates/tutti-midi-file/src/smf/read.rs".into(),
            "use winnow::Parser;\n".into(),
        );
        assert!(check(&m).findings.iter().all(|l| !l.contains("winnow")));
        // The Universal SysEx grammar is the protocol crate's one parser module.
        m.files.insert(
            "crates/tutti-midi-protocol/src/universal/read.rs".into(),
            "use winnow::Parser;\n".into(),
        );
        assert!(check(&m).findings.iter().all(|l| !l.contains("winnow")));
        m.files.insert(
            "crates/tutti-midi-protocol/src/universal.rs".into(),
            "use winnow::Parser;\n".into(),
        );
        assert!(check(&m).findings.iter().any(|l| {
            l.contains("crates/tutti-midi-protocol/src/universal.rs names winnow outside")
        }));
        // The MIDI-CI grammar is the MIDI-CI crate's one parser module.
        m.files.insert(
            "crates/tutti-midi-ci/src/read.rs".into(),
            "use winnow::Parser;\n".into(),
        );
        assert!(
            check(&m)
                .findings
                .iter()
                .all(|l| !l.contains("tutti-midi-ci/src/read.rs"))
        );
        m.files.insert(
            "crates/tutti-midi-ci/src/lib.rs".into(),
            "use winnow::Parser;\n".into(),
        );
        assert!(
            check(&m)
                .findings
                .iter()
                .any(|l| l.contains("crates/tutti-midi-ci/src/lib.rs names winnow outside"))
        );
    }

    #[test]
    fn codec_features_keep_readers_and_writers_apart() {
        let features = "[features]\ndecode-ogg = [\n  \"dep:symphonia\",\n  \"dep:aifc\",\n]\nencode-wav = [\"symphonia?/wav\"]\nencode-flac = [\"dep:flacenc\"]\n";
        let mut v = base();
        v.push(("crates/tutti-codec", "tutti-codec", features));
        let r = run(&v);
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("decode-ogg enables an encoder library ([\"aifc\"])"))
        );
        assert!(
            r.findings
                .iter()
                .any(|l| l.contains("encode-wav enables symphonia"))
        );
        assert_eq!(
            r.findings.iter().filter(|l| l.contains("enables")).count(),
            2
        );
    }

    #[test]
    fn only_a_testing_crate_may_read_the_facade_and_only_as_a_dev_dependency() {
        let top = |dir: &'static str, name: &'static str, body: &'static str| {
            let mut v = base();
            v.push(("crates/tutti", "tutti", ""));
            v.push((dir, name, body));
            run(&v)
                .findings
                .iter()
                .any(|l| l.contains(&format!("{name} depends on tutti, the top of the tree")))
        };
        let dev = "[dev-dependencies]\ntutti = { path = \"x\" }\n";
        let normal = "[dependencies]\ntutti = { path = \"x\" }\n";
        assert!(!top("testing/tutti-book", "tutti-book", dev));
        assert!(top("testing/tutti-book", "tutti-book", normal));
        assert!(top("crates/tutti-graph", "tutti-graph", dev));
    }

    #[test]
    fn the_session_and_the_top_have_few_users_and_hosts_know_no_hosting() {
        let r = with((
            "crates/tutti-graph",
            "tutti-graph",
            "[dev-dependencies]\ntutti-session = { path = \"x\" }\n",
        ));
        let mut v = base();
        v.push(("crates/tutti-session", "tutti-session", ""));
        v.retain(|m| m.1 != "tutti-graph");
        v.push((
            "crates/tutti-graph",
            "tutti-graph",
            "[dev-dependencies]\ntutti-session = { path = \"x\" }\n",
        ));
        assert!(
            run(&v)
                .findings
                .iter()
                .any(|l| { l.contains("tutti-graph depends on tutti-session; only tutti does") })
        );
        let _ = r;
        let mut v = base();
        v.push(("crates/tutti-plugin-ipc", "tutti-plugin-ipc", ""));
        v.push((
            "crates/tutti-clap-host",
            "tutti-clap-host",
            "[dev-dependencies]\ntutti-plugin-ipc = { path = \"x\" }\n",
        ));
        assert!(run(&v).findings.iter().any(|l| l.contains(
            "tutti-clap-host depends on tutti-plugin-ipc; a format host knows no hosting"
        )));
    }
}
