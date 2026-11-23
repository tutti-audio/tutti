//! `check-prose-names`: prose names what exists.
//!
//! The gate reads the [prose](super::prose) of the repo: the comments of every
//! `.rs` file and the text of the `.md` files, apart from the design and
//! archive directories under `docs/`. Two checks run on it and share one report.
//!
//! # Paths and citations
//!
//! - a path rooted at a workspace crate (`tutti_graph::Node::process`,
//!   `tutti_rt::AudioThreadCell`) names, segment by segment, identifiers that occur in
//!   that crate's code (`src/`, comments and literals blanked), or past the
//!   first segment in the code of a crate it names (a re-export). A crate's
//!   `pub use other as alias;` continues a path through `alias` in `other`,
//!   and `PREFIX_*` names any identifier starting with `PREFIX_`;
//! - a design-doc citation (`doc NNN`, `design NNN`, `docs/design/NNN-`) names
//!   a file in `docs/design/` or `docs/archive/`, and a `§N.N` after it names
//!   a numbered heading of that file.
//!
//! Rustdoc refuses a broken intra-doc link; this check covers what rustdoc
//! does not read: a path in backticks, in a `//` comment or in a `.md` file.
//!
//! Limits: a segment resolves when its identifier occurs anywhere in the
//! crate's code (an item, a variant, a method, a local), so a real identifier
//! in the wrong place passes. Only the first number of a list (`docs 013,
//! 014`) is read.
//!
//! # Item names
//!
//! A backticked span (outside a fenced block) that is a Rust path, optionally
//! followed by `()`, `!` or generics, is an item name when it is:
//!
//! - one `CamelCase` word (an uppercase first letter, a lowercase letter, at
//!   least three letters) or one `SCREAMING_SNAKE` word with an underscore;
//! - a `::` path of two or more identifiers, rooted anywhere but `std`,
//!   `core`, `alloc`, `cargo` (a build script's directives) or a crate that a manifest names as a dependency.
//!
//! The leading `crate`, `self`, `super` and `Self` of a path are skipped. An
//! item name resolves when each of its identifiers is a token of the code of
//! every `.rs` file under `crates/`, `testing/` and `xtask/` (comments and
//! the insides of literals blanked; a `SCREAMING_SNAKE` identifier also
//! resolves inside a literal, where an environment variable is read), or the
//! name of a package. A lowercase
//! word alone is never an item name: it is as often an English word.
//!
//! [`ALLOW`] lists the names that are not the workspace's to define (an SDK's
//! constants and interfaces in the crates that wrap it), each with its file
//! prefix and its reason. [`BASELINE`] holds the count of unresolved names
//! each file had when the gate began; a file fails when it holds more than its
//! baseline, and the report says how far the baseline can be lowered.
//!
//! Limits: a name resolves by its identifiers, not by its path, so
//! `module::item` passes when both exist anywhere in the workspace; a span
//! that a line break splits is not read.

use super::lex::{self, blank, comments, line_of, skipped, word};
use super::prose::prose;
use super::ratchet::{self, Found, Wording};
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// A workspace crate, as its code names it.
struct Krate {
    /// Every identifier in the crate's `src/` code.
    idents: BTreeSet<String>,
    /// `pub use <crate> as <alias>;`: alias to crate name.
    aliases: BTreeMap<String, String>,
}

/// Every workspace crate under `crates/` and `testing/`, by its code name.
fn krates(tree: &dyn Tree) -> BTreeMap<String, Krate> {
    let mut dirs = BTreeSet::new();
    for top in ["crates", "testing"] {
        for path in tree.files(top, &skipped) {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() == 3 && parts[2] == "Cargo.toml" {
                dirs.insert(format!("{top}/{}", parts[1]));
            }
        }
    }
    let mut out = BTreeMap::new();
    for dir in dirs {
        let name = dir.rsplit('/').next().unwrap_or(&dir).replace('-', "_");
        let mut k = Krate {
            idents: BTreeSet::new(),
            aliases: BTreeMap::new(),
        };
        for path in tree.files(&format!("{dir}/src"), &skipped) {
            if !path.ends_with(".rs") {
                continue;
            }
            let code = blank(&tree.read(&path).unwrap_or_default());
            let b = code.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if word(b[i]) {
                    let id = lex::ident_at(&code, i).0;
                    i += id.len();
                    if !id.as_bytes()[0].is_ascii_digit() {
                        k.idents.insert(id.to_owned());
                    }
                } else {
                    i += 1;
                }
            }
            for line in code.lines() {
                let t = line.trim();
                let Some(rest) = t.strip_prefix("pub use ").and_then(|r| r.strip_suffix(';'))
                else {
                    continue;
                };
                if let Some((from, alias)) = rest.split_once(" as ")
                    && !from.contains("::")
                {
                    k.aliases
                        .insert(alias.trim().to_owned(), from.trim().to_owned());
                }
            }
        }
        out.insert(name, k);
    }
    out
}

/// The `::`-separated identifiers after `at` (which is just past the crate
/// name), stopping at anything that is not `::ident`.
fn segments(text: &str, mut at: usize) -> Vec<&str> {
    let mut out = Vec::new();
    while text[at..].starts_with("::") {
        let id = lex::ident_at(text, at + 2).0;
        if id.is_empty() {
            break;
        }
        at += 2 + id.len();
        if text[at..].starts_with('*') {
            // `CMD_*`: a prefix, kept with its star.
            out.push(&text[at - id.len()..=at]);
            break;
        }
        out.push(id);
    }
    out
}

/// Whether `seg` (an identifier, or a prefix ending in `*`) is one of `idents`.
fn holds(idents: &BTreeSet<String>, seg: &str) -> bool {
    match seg.strip_suffix('*') {
        Some(prefix) => idents
            .range(prefix.to_owned()..)
            .next()
            .is_some_and(|i| i.starts_with(prefix)),
        None => idents.contains(seg),
    }
}

/// The first segment of `segs` that `krate` does not define, walking aliases.
fn unresolved_segment<'a>(
    krates: &BTreeMap<String, Krate>,
    krate: &str,
    segs: &[&'a str],
) -> Option<(String, &'a str)> {
    let mut cur = krate.to_owned();
    let mut aliasing = true;
    for (n, &seg) in segs.iter().enumerate() {
        let k = &krates[&cur];
        if aliasing
            && let Some(target) = k.aliases.get(seg)
            && krates.contains_key(target)
        {
            cur = target.clone();
            continue;
        }
        aliasing = false;
        if holds(&k.idents, seg) {
            continue;
        }
        // A re-export: past the first segment, an identifier of a crate this
        // one names resolves too.
        let reexported = n > 0
            && krates
                .iter()
                .any(|(name, other)| k.idents.contains(name) && holds(&other.idents, seg));
        if !reexported {
            return Some((cur, seg));
        }
    }
    None
}

/// The design and archive docs by number: each one's text.
fn docs(tree: &dyn Tree) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for dir in ["docs/design", "docs/archive"] {
        for path in tree.files(dir, &skipped) {
            let name = path.rsplit('/').next().unwrap_or(&path);
            let num = &name[..name.len().min(3)];
            if num.len() == 3
                && num.bytes().all(|b| b.is_ascii_digit())
                && name[3..].starts_with('-')
            {
                out.insert(num.to_owned(), tree.read(&path).unwrap_or_default());
            }
        }
    }
    out
}

/// Whether `doc` has a heading numbered `sec` (`## 2.`, `### 2.7.6 Title`,
/// `## §3`).
fn has_section(doc: &str, sec: &str) -> bool {
    doc.lines().any(|l| {
        let Some(h) = l.strip_prefix('#') else {
            return false;
        };
        let h = h.trim_start_matches('#').trim_start();
        let h = h.strip_prefix('§').unwrap_or(h);
        let Some(rest) = h.strip_prefix(sec) else {
            return false;
        };
        let rb = rest.as_bytes();
        match rb.first() {
            None => true,
            Some(b'.') => !rb.get(1).is_some_and(|c| c.is_ascii_alphanumeric()),
            Some(c) => !c.is_ascii_alphanumeric(),
        }
    })
}

/// Every design-doc citation in `text`: (offset, number, section).
fn citations(text: &str) -> Vec<(usize, &str, Option<&str>)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !word(b[i]) || (i > 0 && word(b[i - 1])) {
            i += 1;
            continue;
        }
        let id = lex::ident_at(text, i).0;
        let mut after = i + id.len();
        let lead = if text[i..].starts_with("docs/design/") {
            after = i + "docs/design/".len();
            Some(after)
        } else if text[i..].starts_with("docs/archive/") {
            after = i + "docs/archive/".len();
            Some(after)
        } else if matches!(id, "doc" | "docs" | "Doc" | "Docs" | "design" | "Design") {
            let ws = text[after..].len() - text[after..].trim_start_matches(' ').len();
            (ws > 0).then(|| after + ws)
        } else {
            None
        };
        let Some(n) = lead else {
            i = after.max(i + 1);
            continue;
        };
        let num = &text[n..(n + 3).min(text.len())];
        let tail = b.get(n + 3).copied();
        if num.len() != 3
            || !num.bytes().all(|c| c.is_ascii_digit())
            || tail.is_some_and(|c| c.is_ascii_digit())
        {
            i = after.max(i + 1);
            continue;
        }
        let mut s = n + 3;
        if text[s..].starts_with(',') {
            s += 1;
        }
        s += text[s..].len() - text[s..].trim_start_matches(' ').len();
        let sec = text[s..].strip_prefix('§').and_then(|r| {
            let end = r
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
                .unwrap_or(r.len());
            let sec = r[..end].trim_end_matches('.');
            (!sec.is_empty()).then_some(sec)
        });
        out.push((i, num, sec));
        i = n + 3;
    }
    out
}

fn check_refs(tree: &dyn Tree) -> Report {
    let krates = krates(tree);
    let docs = docs(tree);
    let mut bad = Vec::new();
    for p in prose(tree) {
        let text = &p.text;
        let b = text.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if !word(b[i]) || (i > 0 && (word(b[i - 1]) || b[i - 1] == b':')) {
                i += 1;
                continue;
            }
            let id = lex::ident_at(text, i).0;
            let end = i + id.len();
            if krates.contains_key(id) && text[end..].starts_with("::") {
                let segs = segments(text, end);
                if let Some((krate, seg)) = unresolved_segment(&krates, id, &segs) {
                    bad.push(format!(
                        "{}:{}: `{id}::{}` names `{seg}`, which {krate}'s code does not hold",
                        p.path,
                        line_of(text, i),
                        segs.join("::"),
                    ));
                }
            }
            i = end;
        }
        for (at, num, sec) in citations(text) {
            let line = line_of(text, at);
            match (docs.get(num), sec) {
                (None, _) => bad.push(format!(
                    "{}:{line}: cites design doc {num}, which is not in docs/design or docs/archive",
                    p.path
                )),
                (Some(doc), Some(sec)) if !has_section(doc, sec) => bad.push(format!(
                    "{}:{line}: cites doc {num} §{sec}, which has no such heading",
                    p.path
                )),
                _ => {}
            }
        }
    }
    let mut r = Report::default();
    if bad.is_empty() {
        r.say("check-prose-names: OK");
    } else {
        r.fail("check-prose-names: FAIL (prose names what does not exist):");
        for line in bad {
            r.fail(format!("  {line}"));
        }
    }
    r
}

/// `<count> <path>` lines: each file's unresolved names when the gate began.
pub(crate) const BASELINE: &str = "xtask/doc-idents.txt";

const WORDING: Wording<1> = Wording {
    gate: "check-prose-names",
    baseline: BASELINE,
    nouns: ["unresolved names"],
    headline: "prose names an item no code uses; name the real one",
    paste: false,
};

const CODE_ROOTS: [&str; 3] = ["crates", "testing", "xtask"];

/// A name the workspace does not define: the files that may say it (path
/// prefixes; `""` is every file), the names (tested against the first
/// identifier that does not resolve), and why.
pub(super) type Allow = (&'static [&'static str], fn(&str) -> bool, &'static str);

/// `s` is `prefix` followed by an uppercase letter.
fn camel_after(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_uppercase()))
}

/// `s` is one of `prefixes` followed by more text.
fn starts(s: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| s.starts_with(p))
}

/// The standard library's items that prose names bare (a path rooted at
/// `std`, `core` or `alloc` passes already). The pinned toolchain ships no
/// library source to read them from, so they are listed.
const STD: &[&str] = &[
    "Arc",
    "Barrier",
    "BinaryHeap",
    "BTreeMap",
    "BTreeSet",
    "Cell",
    "Condvar",
    "Cow",
    "Duration",
    "HashMap",
    "HashSet",
    "Instant",
    "LazyLock",
    "ManuallyDrop",
    "MaybeUninit",
    "Mutex",
    "NonNull",
    "NonZero",
    "OnceCell",
    "OnceLock",
    "PhantomData",
    "Pin",
    "Rc",
    "RefCell",
    "RwLock",
    "SystemTime",
    "UnsafeCell",
    "VecDeque",
    "Waker",
    "Weak",
];

pub(super) const ALLOW: &[Allow] = &[
    (
        &[""],
        |s| STD.contains(&s),
        "standard-library items named bare (`RwLock`, `NonZero`)",
    ),
    (
        &[""],
        |s| s.starts_with("ArcSwap"),
        "arc-swap's cells, named where tutti says why the audio thread reads none (`ArcSwap`, `ArcSwapOption`)",
    ),
    (&[""], |s| s == "NaN", "the IEEE 754 value, not an item"),
    (
        &[""],
        |s| {
            starts(
                s,
                &[
                    "SO_",
                    "O_",
                    "SCHED_",
                    "RLIMIT_",
                    "CAP_",
                    "RTLD_",
                    "PIPE_",
                    "ERROR_",
                    "WAIT_",
                    "THREAD_PRIORITY_",
                    "CONFIG_",
                    "DEP_",
                    "CARGO_",
                    "INT32_",
                    "UINT16_",
                    "WAVE_",
                    "KSAUDIO_",
                    "MPEG_",
                ],
            ) || matches!(
                s,
                "OUT_DIR"
                    | "ABOVE_NORMAL"
                    | "GetEnvironmentVariableA"
                    | "LoadLibrary"
                    | "SwitchToThread"
                    | "ReadFile"
                    | "CoInitialize"
                    | "MsgWaitForMultipleObjectsEx"
            )
        },
        "operating-system, C-header and cargo constants and functions (`SO_RCVTIMEO`, `CAP_SYS_NICE`, `WAVE_FORMAT_EXTENSIBLE`, `OUT_DIR`, `LoadLibrary`)",
    ),
    (
        &[""],
        |s| {
            ["NS", "CG", "CF", "FS", "UI", "AU", "MIDI", "MusicDevice"]
                .iter()
                .any(|p| camel_after(s, p))
                || starts(
                    s,
                    &[
                        "UInt",
                        "SInt",
                        "MIDI",
                        "CA_",
                        "CF_",
                        "DISPATCH_",
                        "AudioUnit",
                        "AudioComponent",
                    ],
                )
                || starts(
                    s,
                    &["AudioConvert", "AudioObject", "AudioTimeStamp", "mHostTime"],
                )
        },
        "Apple SDK types and functions (`NSView`, `UInt32`, `MIDIPacketNext`, `AudioUnitRender`, `CA_REALTIME_API`)",
    ),
    (
        &[
            "crates/tutti-vst2-",
            "testing/tutti-vst2-",
            "crates/tutti-plugin/",
            "crates/tutti-plugin-host/",
        ],
        |s| {
            camel_after(s, "Vst")
                || camel_after(s, "Offline")
                || starts(s, &["eff", "audioMaster", "AEff"])
                || s == "ERect"
        },
        "VST 2 SDK opcodes (`effOpen`, `audioMasterVersion`), structs (`VstTimeInfo`, `AEffect`) and the offline interface (`OfflineTask`)",
    ),
    (
        &[
            "crates/tutti-vst3-",
            "crates/tutti-plugin-types/",
            "testing/tutti-shared-plugin",
        ],
        |s| camel_after(s, "I"),
        "VST 3 SDK COM interfaces (`IComponent`, `IPluginFactory`)",
    ),
    (
        &["crates/tutti-clap-", "testing/tutti-clap-test-plugin/"],
        |s| s.starts_with("CLAP_") || s.starts_with("clap_"),
        "CLAP SDK extension ids (`CLAP_EXT_*`) and C structs (`clap_host`)",
    ),
    (
        &["crates/tutti-alsa-ffi/", "crates/tutti-midi-hardware/"],
        |s| s.starts_with("snd_") || s.starts_with("SND_"),
        "ALSA sequencer C functions (`snd_seq_open`) and constants",
    ),
    (
        &["crates/tutti-winmm-ffi/", "crates/tutti-midi-hardware/"],
        |s| {
            ["MIM_", "MMSYSERR_", "MIDIERR_", "midiIn", "midiOut"]
                .iter()
                .any(|p| s.starts_with(p))
        },
        "Windows Multimedia MIDI functions (`midiInOpen`) and constants (`MIM_DATA`)",
    ),
    (
        &["docs/reference/"],
        |_| true,
        "the SDK interface inventories name the SDKs' items, not the workspace's",
    ),
];

fn allowed(path: &str, ident: &str) -> bool {
    ALLOW
        .iter()
        .any(|(files, name, _)| files.iter().any(|f| path.starts_with(f)) && name(ident))
}

/// Every identifier of `blanked` code, plus the package names of the
/// manifests, and the names the manifests list as dependencies.
struct Known {
    tokens: BTreeSet<String>,
    /// The identifiers of the code with its string literals kept: the
    /// environment variables and C names a program reads by string.
    literal_tokens: BTreeSet<String>,
    foreign: BTreeSet<String>,
}

fn tokens_of(blanked: &str, into: &mut BTreeSet<String>) {
    let b = blanked.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let s = i;
            while i < b.len() && word(b[i]) {
                i += 1;
            }
            if !into.contains(&blanked[s..i]) {
                into.insert(blanked[s..i].to_owned());
            }
        } else if b[i].is_ascii_digit() {
            // A number's suffix (`1u8`, `0x1f`) is no identifier.
            while i < b.len() && word(b[i]) {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
}

/// The package name and the dependency names of one manifest, with `-` as `_`.
fn manifest_names(text: &str, packages: &mut BTreeSet<String>, deps: &mut BTreeSet<String>) {
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(h) = line.strip_prefix('[') {
            section = h.trim_end_matches(']').trim_matches('[').to_owned();
            if let Some(dep) = section
                .split_once("dependencies.")
                .map(|(_, d)| d.to_owned())
            {
                deps.insert(dep.replace('-', "_"));
            }
            continue;
        }
        let key = line
            .split(['=', '.', ' '])
            .next()
            .unwrap_or("")
            .trim_matches('"');
        if section == "package" && key == "name" {
            if let Some(v) = line.split('"').nth(1) {
                packages.insert(v.replace('-', "_"));
            }
        } else if section.ends_with("dependencies") && !key.is_empty() && !key.starts_with('#') {
            deps.insert(key.replace('-', "_"));
        }
    }
}

/// `src` with its comments blanked and its literals kept.
fn uncommented(src: &str) -> String {
    let prose = comments(src);
    let bytes: Vec<u8> = src
        .bytes()
        .zip(prose.bytes())
        .map(|(c, p)| if p == b' ' || p == b'\n' { c } else { b' ' })
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn known(tree: &dyn Tree) -> Known {
    let mut tokens = BTreeSet::new();
    let mut literal_tokens = BTreeSet::new();
    let mut packages = BTreeSet::new();
    let mut deps = BTreeSet::new();
    for root in CODE_ROOTS {
        for path in tree.files(root, &skipped) {
            let Some(text) = tree.read(&path) else {
                continue;
            };
            if path.ends_with(".rs") {
                tokens_of(&blank(&text), &mut tokens);
                tokens_of(&uncommented(&text), &mut literal_tokens);
            } else if path.ends_with("Cargo.toml") {
                manifest_names(&text, &mut packages, &mut deps);
            }
        }
    }
    if let Some(text) = tree.read("Cargo.toml") {
        manifest_names(&text, &mut packages, &mut deps);
    }
    let foreign = deps.difference(&packages).cloned().collect();
    tokens.extend(packages);
    Known {
        tokens,
        literal_tokens,
        foreign,
    }
}

/// The backticked spans of `text` outside fenced blocks: (line, span).
fn spans(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut fenced = false;
    for (n, line) in text.lines().enumerate() {
        let body = line
            .trim_start()
            .trim_start_matches(['/', '!', '*'])
            .trim_start();
        if body.starts_with("```") || body.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let b = line.as_bytes();
        let run = |at: usize| b[at..].iter().take_while(|&&c| c == b'`').count();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'`' {
                i += 1;
                continue;
            }
            let open = run(i);
            let from = i + open;
            let mut j = from;
            let mut close = None;
            while j < b.len() {
                if b[j] == b'`' {
                    let r = run(j);
                    if r == open {
                        close = Some(j);
                        break;
                    }
                    j += r;
                } else {
                    j += 1;
                }
            }
            let Some(end) = close else { break };
            out.push((n + 1, line[from..end].trim()));
            i = end + open;
        }
    }
    out
}

/// The identifier at the start of `s`: it starts with a letter or `_`.
fn item_ident(s: &str) -> Option<&str> {
    let (id, _) = lex::ident_at(s, 0);
    id.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        .then_some(id)
}

fn camel(s: &str) -> bool {
    s.len() >= 3
        && s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars().any(|c| c.is_ascii_lowercase())
        && !s.contains('_')
}

fn screaming(s: &str) -> bool {
    s.contains('_')
        && s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn snake(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The identifiers of a span that is an item name, or `None`.
fn item_name(span: &str) -> Option<Vec<&str>> {
    let mut rest = span;
    let mut segs = Vec::new();
    loop {
        let id = item_ident(rest)?;
        segs.push(id);
        rest = &rest[id.len()..];
        match rest.strip_prefix("::") {
            Some(next) if item_ident(next).is_some() => rest = next,
            _ => break,
        }
    }
    let tail_ok = rest.is_empty()
        || rest == "::*"
        || rest.starts_with(['(', '<', '!'])
        || rest.starts_with("::<");
    if !tail_ok {
        return None;
    }
    while segs.len() > 1 && matches!(segs[0], "crate" | "self" | "super" | "Self") {
        segs.remove(0);
    }
    if matches!(segs[0], "crate" | "self" | "super" | "Self") {
        return None;
    }
    let shaped = match segs.as_slice() {
        [one] => camel(one) || screaming(one),
        many => many.iter().any(|s| camel(s)) || many.iter().all(|s| snake(s) || screaming(s)),
    };
    shaped.then_some(segs)
}

/// The first identifier of `segs` that no code of the workspace uses; `None`
/// when the name resolves or is rooted at a foreign crate.
fn unresolved<'a>(segs: &[&'a str], known: &Known) -> Option<&'a str> {
    if segs.len() > 1
        && (matches!(segs[0], "std" | "core" | "alloc" | "cargo")
            || known.foreign.contains(segs[0]))
    {
        return None;
    }
    segs.iter()
        .copied()
        .find(|s| !(known.tokens.contains(*s) || screaming(s) && known.literal_tokens.contains(*s)))
}

fn check_idents(tree: &dyn Tree) -> Report {
    let baseline = match ratchet::load(tree, &WORDING) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let known = known(tree);
    let mut found = Vec::new();
    for p in prose(tree) {
        let mut sites = Vec::new();
        for (line, span) in spans(&p.text) {
            let Some(segs) = item_name(span) else {
                continue;
            };
            let Some(bad) = unresolved(&segs, &known) else {
                continue;
            };
            if !allowed(&p.path, bad) {
                sites.push(format!(
                    "    {}:{line}: `{span}` names no item (`{bad}` is in no code)",
                    p.path
                ));
            }
        }
        found.push(Found {
            path: p.path,
            sites: [sites],
        });
    }
    ratchet::settle(&WORDING, &baseline, found)
}

/// Both checks; their findings and lines are one report.
pub(super) fn check(tree: &dyn Tree) -> Report {
    let mut r = check_refs(tree);
    let idents = check_idents(tree);
    r.out.extend(idents.out);
    r.findings.extend(idents.findings);
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const LIB: &str =
        "pub struct Node;\nimpl Node { pub fn process(&self) {} }\npub use tutti_b as b;\n";

    fn run_refs(prose: &str) -> Vec<String> {
        let tree = Mem::with(&[
            ("crates/tutti-a/Cargo.toml", ""),
            ("crates/tutti-a/src/lib.rs", LIB),
            ("crates/tutti-b/Cargo.toml", ""),
            ("crates/tutti-b/src/lib.rs", "pub enum Mode { Fast }\n"),
            ("crates/tutti-c/Cargo.toml", ""),
            ("crates/tutti-c/src/lib.rs", prose),
            (
                "docs/design/013-graph.md",
                "# 013\n## 2. Design\n### 2.7.6 Sums\n## §3 Rest\n",
            ),
            ("docs/archive/007-old.md", "# 007\n"),
        ]);
        check_refs(&tree).findings
    }

    #[test]
    fn a_path_whose_segments_the_crate_holds_resolves() {
        assert!(run_refs("// `tutti_a::Node::process` and tutti_a::Node.\n").is_empty());
        assert!(run_refs("/// [`Mode`](tutti_a::b::Mode::Fast), `tutti_b::Mode`\n").is_empty());
        assert!(run_refs("// `tutti_a::*` and `tutti_a::{Node}` stop at the glob\n").is_empty());
        assert!(run_refs("// `tutti_a::No*` is a prefix of `Node`\n").is_empty());
        assert!(
            run_refs("// `tutti_a::Node::Fast`: tutti_a names tutti_b, which holds it\n")
                .is_empty()
        );
    }

    #[test]
    fn a_segment_the_crate_does_not_hold_fails_naming_it() {
        let bad = run_refs("// x\n// `tutti_a::Node::render`\n");
        assert_eq!(
            bad[1],
            "  crates/tutti-c/src/lib.rs:2: `tutti_a::Node::render` names `render`, which tutti_a's code does not hold"
        );
        assert_eq!(run_refs("// `tutti_a::downmix`\n").len(), 2);
        assert_eq!(
            run_refs("// `tutti_a::Fast`: a re-export is not a root item\n").len(),
            2
        );
        assert_eq!(run_refs("// `tutti_a::Zz*`\n").len(), 2);
        assert_eq!(
            run_refs("// `tutti_a::b::Slow`\n").len(),
            2,
            "{:?}",
            run_refs("// `tutti_a::b::Slow`\n")
        );
    }

    #[test]
    fn code_and_strings_are_not_prose() {
        assert!(run_refs("fn f() { let s = \"tutti_a::Gone\"; }\n").is_empty());
    }

    #[test]
    fn a_citation_names_a_doc_and_a_section_it_has() {
        assert!(run_refs("// doc 013, design 013 §2.7.6, doc 013 §2, doc 013 §3, docs/design/013-graph.md, doc 007\n").is_empty());
        let bad = run_refs("// design 022 §1\n// doc 013 §2.8\n// doc 013 §2.7\n");
        assert_eq!(
            bad[1..],
            [
                "  crates/tutti-c/src/lib.rs:1: cites design doc 022, which is not in docs/design or docs/archive",
                "  crates/tutti-c/src/lib.rs:2: cites doc 013 §2.8, which has no such heading",
                "  crates/tutti-c/src/lib.rs:3: cites doc 013 §2.7, which has no such heading",
            ]
        );
        assert_eq!(run_refs("// docs/design/026-stretch.md\n").len(), 2);
    }

    #[test]
    fn numbers_that_are_not_citations_pass() {
        assert!(
            run_refs("// a doc 1234 long, doc 12, the design 2026 roadmap, mydoc 999\n").is_empty()
        );
    }

    fn run_idents(files: &[(&str, &str)]) -> Report {
        let mut all = vec![(BASELINE, "")];
        all.extend_from_slice(files);
        check_idents(&Mem::with(&all))
    }

    #[test]
    fn a_name_no_code_uses_fails_with_its_file_and_line() {
        let r = run_idents(&[(
            "crates/a/src/lib.rs",
            "struct Real;\n// the `Real` type, not the `Gone` one\n/// see `Real::new` and `Real::gone()`\nfn new() {}\n",
        )]);
        assert_eq!(
            r.findings,
            [
                "check-prose-names: FAIL (prose names an item no code uses; name the real one):",
                "  crates/a/src/lib.rs: 2 unresolved names, baseline 0",
                "    crates/a/src/lib.rs:2: `Gone` names no item (`Gone` is in no code)",
                "    crates/a/src/lib.rs:3: `Real::gone()` names no item (`gone` is in no code)",
            ]
        );
    }

    #[test]
    fn a_name_that_code_uses_resolves_even_when_only_a_comment_or_string_says_the_word() {
        let r = run_idents(&[(
            "crates/a/src/lib.rs",
            "// `Ghost` and `Phantom`\nconst S: &str = \"Ghost\";\nstruct Phantom;\n",
        )]);
        assert_eq!(r.findings.len(), 3, "{:?}", r.findings);
        assert!(r.findings[2].contains("`Ghost`"));
    }

    #[test]
    fn markdown_prose_is_read_but_fenced_blocks_and_the_records_are_not() {
        let r = run_idents(&[
            ("crates/a/src/lib.rs", "struct Real;\n"),
            (
                "crates/a/README.md",
                "A `Real` and a `Fake`.\n```rust\nlet x = `Hidden`;\n```\n",
            ),
            ("docs/design/001-x.md", "`Record`\n"),
            ("docs/archive/x.md", "`Archived`\n"),
        ]);
        assert_eq!(
            r.findings[1..],
            [
                "  crates/a/README.md: 1 unresolved names, baseline 0",
                "    crates/a/README.md:1: `Fake` names no item (`Fake` is in no code)",
            ]
        );
    }

    #[test]
    fn only_item_shaped_spans_are_names() {
        let names = |s: &str| item_name(s).map(|v| v.join("::"));
        assert_eq!(names("Foo"), Some("Foo".into()));
        assert_eq!(names("Foo::bar()"), Some("Foo::bar".into()));
        assert_eq!(names("Vec<Foo>"), Some("Vec".into()));
        assert_eq!(names("a::b"), Some("a::b".into()));
        assert_eq!(names("crate::a::Bar"), Some("a::Bar".into()));
        assert_eq!(names("MAX_TAIL"), Some("MAX_TAIL".into()));
        for not in [
            "word", "L2", "Ok", "x.y", "Foo bar", "foo_bar", "MIDI", "Self", "self::x", "1.0",
            "-Foo", "a::", "Foo.bar",
        ] {
            assert_eq!(item_name(not), None, "{not}");
        }
    }

    #[test]
    fn std_and_dependency_rooted_paths_are_not_checked_but_workspace_ones_are() {
        let r = run_idents(&[
            (
                "crates/a/Cargo.toml",
                "[package]\nname = \"tutti-a\"\n[dependencies]\ncpal = \"1\"\n[dependencies.symphonia]\nversion = \"1\"\n",
            ),
            (
                "crates/a/src/lib.rs",
                "// `std::sync::Atomic` `cpal::Stream` `symphonia::Probe` `tutti_a::Gone`\n",
            ),
        ]);
        assert_eq!(r.findings.len(), 3, "{:?}", r.findings);
        assert!(r.findings[2].contains("`tutti_a::Gone`"));
    }

    #[test]
    fn a_standard_library_name_and_arc_swaps_cells_are_known_anywhere() {
        let r = run_idents(&[(
            "crates/tutti-graph/src/lib.rs",
            "// `RwLock<T>: Sync`, `NonZero`, `ArcSwapOption`, `RwLocked`\n",
        )]);
        assert_eq!(
            r.findings[1..],
            [
                "  crates/tutti-graph/src/lib.rs: 1 unresolved names, baseline 0",
                "    crates/tutti-graph/src/lib.rs:1: `RwLocked` names no item (`RwLocked` is in no code)",
            ]
        );
    }

    #[test]
    fn an_sdk_name_is_allowed_only_in_the_crate_that_wraps_the_sdk() {
        let r = run_idents(&[
            (
                "crates/tutti-vst3-ffi/src/lib.rs",
                "// `IComponent` `Gone`\n",
            ),
            ("crates/tutti-graph/src/lib.rs", "// `IComponent`\n"),
        ]);
        assert_eq!(
            r.findings[1..],
            [
                "  crates/tutti-graph/src/lib.rs: 1 unresolved names, baseline 0",
                "    crates/tutti-graph/src/lib.rs:1: `IComponent` names no item (`IComponent` is in no code)",
                "  crates/tutti-vst3-ffi/src/lib.rs: 1 unresolved names, baseline 0",
                "    crates/tutti-vst3-ffi/src/lib.rs:1: `Gone` names no item (`Gone` is in no code)",
            ]
        );
    }
}
