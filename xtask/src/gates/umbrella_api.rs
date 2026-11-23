//! `umbrella-api`: every type in the public signature of an item the umbrella
//! exports is nameable through the umbrella.
//!
//! The companion of `check-umbrella`, which holds that each item sits at one
//! path; this reads the other way, that no type a caller needs is missing. It is
//! a recipe and not a gate: it needs the docs nightly and `cargo-public-api`.
//!
//! The input is `cargo public-api --all-features -s` text for crates and the
//! umbrella's `lib.rs`. A type or item is named by its crate, the modules on its
//! path and its name. An *exported item* is the subject of a line (the first
//! capitalised path segment after the crate, or the whole path of a free
//! function) that `lib.rs` exports. Every type path of a `tutti_` crate in the
//! rest of the line is a type the signature names, and the types those name in
//! turn; each is *covered* when `lib.rs` exports:
//!
//! - the type at its path (a `pub use` list covers each name in it, not its
//!   alias; a crate's `Error` is not covered by another crate's);
//! - a module of its crate by name, which covers every path that starts with it;
//! - the type from a crate that re-exports it: the crate's text holds a `pub use`
//!   line for the name and no definition at that path, and the type is where the
//!   crate defines the name, or, when it defines none, a path of the name its
//!   text names. Exactly one such path is covered, and printed as a decision;
//!   several, or none, are listed as ambiguous and cover nothing. A glob
//!   re-export appears in the text as one `pub use` line per item, at the path
//!   `lib.rs` names it at.
//!
//! A type a signature names at a path in no public module of its crate and that the
//! crate's text does not re-export (the path of a private module) is covered by the umbrella's export of
//! its name from that crate, when the umbrella exports one type of the crate by that
//! name. The one false cover left: a private-module type that shares its name with a
//! different exported type of the same crate counts as that type.
//!
//! A final segment is a constant when a `pub const` or `pub static` line of the
//! texts defines that path, whatever its case; an all-capitals type is a type.
//! A whole-crate or glob export (`pub use tutti_x as x;`, `pub use tutti_x::m::*;`)
//! is ignored: it names no item, so the recipe reads an umbrella that exports
//! single items.
//!
//! The result lists the types no export covers, each with the items that name it,
//! and ranks every exported item (a *seed*) by how many uncovered types its
//! signature pulls in, transitively. A leaky seed is dropped from `lib.rs`; one
//! the umbrella needs stays, and the internal types it names are rows of
//! [`INTERNAL`]. A row of a table that no type of a crate the texts cover matches
//! is stale and listed in [`Analysis::stale`].
//!
//! Not asked for: a private `Sealed` trait, which is unnameable on purpose; a
//! crate's `Result` alias, which expands to std's `Result` and an error type the
//! umbrella exports; the rows of [`TESTING`], items a crate keeps behind its
//! `testing` feature, which the all-features text still shows.

use super::lex::word;
use super::use_tree::uses;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Why the sans-IO surface of a network `Client` and `Host` is internal.
const SANS_IO: &str = "the sans-IO surface of `Client` and `Host`, named by the umbrella only as a handle's parameter";

/// `(crate, type, why)` rows: internals of their crate (the engine, the executor,
/// the compiler) that the umbrella does not name. An exported item whose signature
/// names one is a finding for its owner, listed in [`Analysis::leaks`]. A type of
/// `*` is every type of the crate.
const INTERNAL: &[(&str, &str, &str)] = &[
    ("tutti_midi_network", "Event", SANS_IO),
    ("tutti_midi_network", "Header", SANS_IO),
    ("tutti_midi_network", "HostConfig", SANS_IO),
    ("tutti_midi_network", "NakReason", SANS_IO),
    ("tutti_midi_network", "Nonce", SANS_IO),
    ("tutti_midi_network", "SendError", SANS_IO),
    ("tutti_midi_network", "SequenceNumber", SANS_IO),
    ("tutti_midi_network", "SessionState", SANS_IO),
    ("tutti_midi_network", "Transmit", SANS_IO),
    (
        "tutti_units",
        "Text",
        "the bounded text behind a name alias the umbrella exports (`Username`); the alias is the name",
    ),
    (
        "tutti_timeline",
        "Transport",
        "the engine's playback transport; an app drives it through `Session`",
    ),
    (
        "tutti_graph",
        "Editor",
        "the graph editor; the session wraps it",
    ),
    (
        "tutti_engine",
        "StreamDriver",
        "a device backend's driver; the session runs it",
    ),
    (
        "tutti_platform",
        "WeakExecutor",
        "the executor a `Catalog` scan runs on; an app scans through `Session::scan_plugins`",
    ),
    (
        "tutti_plugin_host",
        "PluginHandle",
        "the host's control handle of a plugin; `Plugin` wraps it",
    ),
    (
        "tutti_graph",
        "Executor",
        "the graph's executor; an app renders through `Session` or `Bounce`",
    ),
    (
        "tutti_timeline",
        "OfflineClock",
        "the export render's clock; `ExportClock` wraps it",
    ),
    (
        "tutti_graph",
        "EventEdge",
        "the compiler's edge; the session's `EventEdge` is another",
    ),
    (
        "tutti_graph",
        "InputPort",
        "the compiler's port; the session's `InputPort` is another",
    ),
    (
        "tutti_graph",
        "OutputPort",
        "the compiler's port; the session's `OutputPort` is another",
    ),
    (
        "tutti_plugin",
        "PluginClient",
        "the host's client of a served plugin; `Plugin` wraps it",
    ),
    (
        "tutti_midi_protocol",
        "MmcCommand",
        "the MMC command model, a family of types the umbrella leaves to its crate",
    ),
    (
        "tutti_midi_hardware",
        "MidiEndpoints",
        "the OS endpoint set, a family of types the umbrella leaves to its crate",
    ),
    (
        "tutti_midi_hardware",
        "DeviceStream",
        "an open device's stream, a family of types the umbrella leaves to its crate",
    ),
    (
        "tutti_sampler",
        "VoiceNode",
        "the sampler's voice node and its disk plumbing",
    ),
    ("tutti_sampler", "VoiceSource", "the sampler's voice source"),
    (
        "tutti_modulation",
        "Declarations",
        "the compiler's declarations",
    ),
    ("tutti_modulation", "Source", "the compiler's signal source"),
    ("tutti_alsa_ffi", "*", "not a dependency of the umbrella"),
    ("tutti_measure", "*", "not a dependency of the umbrella"),
];

/// Type names never asked for.
const EXCLUDED: &[&str] = &["Sealed", "Result"];

/// `(crate, type)` rows behind a `testing` feature of the owning crate.
const TESTING: &[(&str, &str)] = &[
    ("tutti_session", "AudioSide"),
    ("tutti_session", "Detached"),
    ("tutti_session", "Testing"),
    ("tutti_plugin_types", "FakeController"),
    ("tutti_plugin_types", "FakeLoaded"),
    ("tutti_plugin_types", "FakeProcessor"),
];

/// A type or item a text names: its crate, the modules on its path and its name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Ty {
    krate: String,
    mods: Vec<String>,
    name: String,
}

/// The constants and statics the texts define, by path: a final segment is a
/// constant only when its path is here, so an all-capitals type (`UMP`) is a type.
type Consts = BTreeSet<Ty>;

/// Whether `name` is a function: not capitalised, so not a type.
fn is_value(name: &str) -> bool {
    !name.starts_with(|c: char| c.is_ascii_uppercase())
}

impl Ty {
    fn is_value(&self) -> bool {
        is_value(&self.name)
    }

    /// The modules a module export has to prefix to cover this: its own path for a
    /// value (a function sits in a module), the modules above it for a type.
    fn under(&self) -> Vec<&str> {
        let mut p: Vec<&str> = self.mods.iter().map(String::as_str).collect();
        if self.is_value() {
            p.push(&self.name);
        }
        p
    }

    fn show(&self) -> String {
        let mut p = vec![self.krate.as_str()];
        p.extend(self.mods.iter().map(String::as_str));
        p.push(&self.name);
        p.join("::")
    }
}

/// The `tutti_` path at `at` (crate, modules, item name), and the offset after it:
/// up to the first capitalised segment, or the whole path of a function. A bare
/// crate or a path cut short after `::` names nothing.
fn named_at(s: &str, at: usize) -> Option<(Ty, usize)> {
    let b = s.as_bytes();
    let word_end = |from: usize| from + b[from..].iter().take_while(|&&c| word(c)).count();
    let mut i = word_end(at);
    let krate = s[at..i].to_owned();
    let mut mods = Vec::new();
    while s[i..].starts_with("::") {
        let e = word_end(i + 2);
        if e == i + 2 {
            break;
        }
        let seg = s[i + 2..e].to_owned();
        i = e;
        if seg.starts_with(|c: char| c.is_ascii_uppercase()) {
            return Some((
                Ty {
                    krate,
                    mods,
                    name: seg,
                },
                i,
            ));
        }
        mods.push(seg);
    }
    let name = mods.pop()?;
    Some((Ty { krate, mods, name }, i))
}

/// Every type the `tutti_` paths in `s` name (not functions or `consts`).
fn types<'a>(s: &'a str, consts: &'a Consts) -> impl Iterator<Item = Ty> + 'a {
    let b = s.as_bytes();
    s.match_indices("tutti_")
        .filter(|(i, _)| *i == 0 || !word(b[i - 1]))
        .filter_map(|(i, _)| named_at(s, i))
        .map(|(t, _)| t)
        .filter(|t| !t.is_value() && !consts.contains(t))
}

/// What follows `pub ` on a line of the text, after any attributes
/// (`#[non_exhaustive] pub enum ..`).
fn public(line: &str) -> Option<&str> {
    let mut rest = line;
    while rest.starts_with("#[") {
        rest = rest[rest.find("] ")? + 2..].trim_start();
    }
    rest.strip_prefix("pub ")
}

/// The `(crate, modules)` a `pub mod` line of the text declares.
fn public_module(line: &str) -> Option<(String, Vec<String>)> {
    let path = public(line)?.strip_prefix("mod ")?.trim();
    let mut segs = path.split("::").map(str::to_owned);
    let krate = segs.next()?;
    krate.starts_with("tutti_").then(|| (krate, segs.collect()))
}

/// `s` without leading `unsafe`, `const`, `auto`, `async`, `default` and
/// `extern "ABI"`.
fn strip_qualifiers(mut s: &str) -> &str {
    loop {
        if let Some(r) = ["unsafe ", "const ", "auto ", "async ", "default "]
            .iter()
            .find_map(|q| s.strip_prefix(q))
        {
            s = r;
        } else if let Some(r) = s
            .strip_prefix("extern \"")
            .and_then(|r| r.split_once("\" "))
        {
            s = r.1;
        } else {
            return s;
        }
    }
}

/// A `pub` line of the text: the keyword before its path (`struct`, `fn`, none for
/// a field) and the line from the path on.
struct Item<'a> {
    /// The line is a `const` or `static` item.
    constant: bool,
    keyword: Option<&'a str>,
    from: &'a str,
}

impl<'a> Item<'a> {
    fn read(line: &'a str) -> Option<Item<'a>> {
        let public = public(line)?;
        let constant = ["const ", "static "].iter().any(|k| {
            public
                .strip_prefix(k)
                .is_some_and(|r| r.starts_with("tutti_"))
        });
        let rest = strip_qualifiers(public);
        let at = rest.find("tutti_")?;
        let before = &rest[..at];
        if !(before.is_empty() || before.ends_with(' ')) {
            return None;
        }
        let words: Vec<&str> = before.split_whitespace().collect();
        if !words.iter().all(|w| w.bytes().all(word)) {
            return None;
        }
        Some(Item {
            constant,
            keyword: words.last().copied(),
            from: &rest[at..],
        })
    }

    /// The item the line is about and what follows its path.
    fn subject(&self) -> Option<(Ty, &'a str)> {
        let (t, end) = named_at(self.from, 0)?;
        Some((t, &self.from[end..]))
    }

    /// The type the line defines (not a member of one).
    fn defines(&self) -> Option<Ty> {
        const KEYWORDS: [&str; 6] = ["struct", "enum", "trait", "union", "type", "macro"];
        if !self.keyword.is_some_and(|k| KEYWORDS.contains(&k)) {
            return None;
        }
        let (t, rest) = self.subject()?;
        (!rest.starts_with("::")).then_some(t)
    }

    /// The constant or static the line defines.
    fn constant(&self) -> Option<Ty> {
        let (t, rest) = self.subject()?;
        (self.constant && rest.starts_with(':') && !rest.starts_with("::")).then_some(t)
    }

    /// The path a `pub use` line re-exports at.
    fn re_exports(&self) -> Option<Ty> {
        (self.keyword == Some("use")).then(|| self.subject().map(|s| s.0))?
    }
}

fn consts(texts: &[(&str, &str)]) -> Consts {
    texts
        .iter()
        .flat_map(|(_, t)| t.lines())
        .filter_map(Item::read)
        .filter_map(|i| i.constant())
        .collect()
}

/// What `lib.rs` exports, as the item or module each `pub use` leaf names. A
/// whole-crate or glob leaf is not an item.
fn exports(lib: &str) -> Vec<Ty> {
    uses(lib)
        .into_iter()
        .filter(|l| !l.whole && l.krate().starts_with("tutti_"))
        .map(|l| Ty {
            krate: l.krate().to_owned(),
            mods: l.path[1..].to_vec(),
            name: l.source,
        })
        .collect()
}

/// The `tutti_` crates the umbrella exports from, sorted.
pub(crate) fn reexported_crates(lib: &str) -> Vec<String> {
    let set: BTreeSet<String> = exports(lib).into_iter().map(|t| t.krate).collect();
    set.into_iter().collect()
}

/// What the umbrella exports, as the coverage rules read it.
struct Covers {
    /// The exported items: types and functions, by path.
    items: BTreeSet<Ty>,
    /// The exported modules, as `(crate, path)`; a function is one too.
    modules: Vec<(String, Vec<String>)>,
    /// The paths a crate that re-exports an exported name has it at.
    foreign: BTreeSet<Ty>,
    /// `re-exporter::Name -> covered path` for each foreign cover.
    decisions: Vec<String>,
    /// The re-exports of a name that more than one path could be, or none names.
    ambiguous: Vec<String>,
}

impl Covers {
    fn new(lib: &str, texts: &[(&str, &str)], consts: &Consts, named: &BTreeSet<Ty>) -> Covers {
        let mut defined = BTreeSet::new();
        let mut re_exported = BTreeSet::new();
        let mut public_mods = BTreeSet::new();
        for (_, text) in texts {
            public_mods.extend(text.lines().filter_map(public_module));
            for item in text.lines().filter_map(Item::read) {
                defined.extend(item.defines());
                re_exported.extend(item.re_exports());
            }
        }
        let mut c = Covers {
            items: BTreeSet::new(),
            modules: Vec::new(),
            foreign: BTreeSet::new(),
            decisions: Vec::new(),
            ambiguous: Vec::new(),
        };
        let exported = exports(lib);
        for e in exported.iter().cloned() {
            if e.is_value() {
                let mut path = e.mods.clone();
                path.push(e.name.clone());
                c.modules.push((e.krate.clone(), path));
            }
            let text = texts.iter().find(|(c, _)| *c == e.krate).map(|(_, t)| *t);
            if let Some(text) = text
                && re_exported.contains(&e)
                && !defined.contains(&e)
            {
                // The crate's own definitions of the name, else (a type of another
                // crate) the other paths of the name its text names. Exactly one
                // is the type; more, or none, is reported and covers nothing.
                let mut cands: BTreeSet<Ty> = defined
                    .iter()
                    .filter(|t| t.krate == e.krate && t.name == e.name)
                    .cloned()
                    .collect();
                if cands.is_empty() {
                    cands = types(text, consts)
                        .filter(|t| t.name == e.name && *t != e)
                        .collect();
                }
                let mut paths = cands.iter().map(Ty::show);
                match (paths.next(), paths.next()) {
                    (Some(one), None) => {
                        c.decisions.push(format!("{} -> {one}", e.show()));
                        c.foreign.extend(cands);
                    }
                    (Some(_), Some(_)) => {
                        let all: Vec<String> = cands.iter().map(Ty::show).collect();
                        c.ambiguous
                            .push(format!("{}: ambiguous: {}", e.show(), all.join(", ")));
                    }
                    (None, _) => c.ambiguous.push(format!(
                        "{}: unresolved: no signature of its crate names it",
                        e.show()
                    )),
                }
            }
            if defined.contains(&e)
                && exported
                    .iter()
                    .filter(|x| x.krate == e.krate && x.name == e.name)
                    .count()
                    == 1
            {
                // A signature names a type at the path it is defined at, which can
                // be a private module (the text has no `pub mod` for it): that path
                // is the type the umbrella exports under the name. A path in a
                // public module, or one the text re-exports, is its own type.
                for t in named.iter().filter(|t| {
                    t.krate == e.krate
                        && t.name == e.name
                        && **t != e
                        && !t.mods.is_empty()
                        && !re_exported.contains(*t)
                        && !public_mods.contains(&(t.krate.clone(), t.mods.clone()))
                }) {
                    c.decisions.push(format!("{} -> {}", t.show(), e.show()));
                    c.foreign.insert(t.clone());
                }
            }
            c.items.insert(e);
        }
        c
    }

    fn covers(&self, t: &Ty) -> bool {
        self.items.contains(t)
            || self.foreign.contains(t)
            || self.modules.iter().any(|(k, path)| {
                let under = t.under();
                *k == t.krate
                    && under.len() >= path.len()
                    && under.iter().zip(path).all(|(a, b)| a == b)
            })
    }
}

/// One exported item and how much the umbrella leaves out of its signature.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Seed {
    /// Unexported types its signature pulls in, transitively.
    pub(crate) pulled: usize,
    /// Of them, the internal ones and those reached only through one.
    pub(crate) internal_or_behind: usize,
    pub(crate) item: String,
}

/// What the recipe reports.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Analysis {
    /// `crate::Type <- users` for each type no export covers.
    pub(crate) missing: Vec<String>,
    /// `crate::Type <- users` for each [`INTERNAL`] type an exported item names.
    pub(crate) leaks: Vec<String>,
    /// Each exported item that pulls in any type not exported, most first.
    pub(crate) seeds: Vec<Seed>,
    /// Crates a signature names whose text was not given.
    pub(crate) wanted: Vec<String>,
    /// Table rows, for a crate whose text was given, that no type named in the
    /// texts matches.
    pub(crate) stale: Vec<String>,
    /// Crates named that have no text (not packages).
    pub(crate) skipped: Vec<String>,
    /// `re-exporter::Name -> path` for each name covered by what its crate
    /// re-exports, so a wrong cover is visible.
    pub(crate) decisions: Vec<String>,
    /// Re-exported names that more than one path could be, or that no signature
    /// names: covered by nothing.
    pub(crate) ambiguous: Vec<String>,
}

/// The signature types of every item in `texts`, by the item's path.
type Edges = BTreeMap<Ty, Vec<Ty>>;

fn edges(texts: &[(&str, &str)], consts: &Consts) -> Edges {
    let mut out = Edges::new();
    for item in texts
        .iter()
        .flat_map(|(_, t)| t.lines())
        .filter_map(Item::read)
    {
        let Some((subject, rest)) = item.subject() else {
            continue;
        };
        out.entry(subject).or_default().extend(types(rest, consts));
    }
    out
}

fn skip(t: &Ty) -> bool {
    let id = (t.krate.as_str(), t.name.as_str());
    EXCLUDED.contains(&id.1) || TESTING.contains(&id)
}

fn internal_id(krate: &str, name: &str) -> bool {
    INTERNAL
        .iter()
        .any(|(c, n, _)| *c == krate && (*n == "*" || *n == name))
}

fn internal(t: &Ty) -> bool {
    internal_id(&t.krate, &t.name)
}

/// Each uncovered type `refs` name, transitively, with the type that named it (one
/// pair for every naming, so a type shows all its users). Internal types are
/// returned and followed only when `through`.
fn reach(
    edges: &Edges,
    covers: &Covers,
    me: &str,
    refs: &[Ty],
    through: bool,
) -> Vec<(Ty, String)> {
    let mut followed = BTreeSet::new();
    let mut out = Vec::new();
    let mut queue: VecDeque<(Ty, String)> =
        refs.iter().map(|t| (t.clone(), me.to_owned())).collect();
    while let Some((t, user)) = queue.pop_front() {
        if covers.covers(&t) || skip(&t) {
            continue;
        }
        if followed.insert(t.clone()) && (through || !internal(&t)) {
            for next in edges.get(&t).into_iter().flatten() {
                queue.push_back((next.clone(), t.show()));
            }
        }
        out.push((t, user));
    }
    out
}

fn distinct(walk: &[(Ty, String)]) -> BTreeSet<&Ty> {
    walk.iter().map(|(t, _)| t).collect()
}

fn list(m: BTreeMap<Ty, BTreeSet<String>>) -> Vec<String> {
    m.into_iter()
        .map(|(t, users)| {
            let more = if users.len() > 4 { " ..." } else { "" };
            let users: Vec<String> = users.into_iter().take(4).collect();
            format!("{} <- {}{more}", t.show(), users.join(", "))
        })
        .collect()
}

/// The rows of the tables that no type in `named` matches, for the crates in
/// `loaded` (a row of a crate with no text cannot be judged): `INTERNAL`
/// and `TESTING` rows by crate and name, `*` for any name, and each excluded name.
fn stale_rows(
    named: &BTreeSet<Ty>,
    loaded: &BTreeSet<&str>,
    internal: &[(&str, &str, &str)],
    testing: &[(&str, &str)],
    excluded: &[&str],
) -> Vec<String> {
    let hit = |krate: &str, name: &str| {
        named
            .iter()
            .any(|t| t.krate == krate && (name == "*" || t.name == name))
    };
    let rows = internal
        .iter()
        .map(|(c, n, _)| ("INTERNAL", *c, *n))
        .chain(testing.iter().map(|(c, n)| ("TESTING", *c, *n)));
    let mut out: Vec<String> = rows
        .filter(|(_, krate, name)| loaded.contains(krate) && !hit(krate, name))
        .map(|(label, krate, name)| format!("{label} {krate}::{name}"))
        .collect();
    for name in excluded {
        if !loaded.is_empty() && !named.iter().any(|t| t.name == *name) {
            out.push(format!("EXCLUDED {name}"));
        }
    }
    out
}

/// The types the exported items of `texts` name that the umbrella does not export,
/// and the seeds ranked by how many each pulls in.
fn analyze(texts: &[(&str, &str)], lib: &str) -> Analysis {
    let consts = consts(texts);
    let named: BTreeSet<Ty> = texts.iter().flat_map(|(_, t)| types(t, &consts)).collect();
    let covers = Covers::new(lib, texts, &consts, &named);
    let edges = edges(texts, &consts);
    let loaded: BTreeSet<&str> = texts.iter().map(|(c, _)| *c).collect();
    let mut a = Analysis::default();
    let (mut missing, mut leaks) = (BTreeMap::new(), BTreeMap::new());
    for (subject, refs) in &edges {
        if !covers.covers(subject) {
            continue;
        }
        let me = subject.show();
        let near = reach(&edges, &covers, &me, refs, false);
        for (t, user) in &near {
            let into = if internal(t) {
                &mut leaks
            } else {
                &mut missing
            };
            into.entry(t.clone())
                .or_insert_with(BTreeSet::new)
                .insert(user.clone());
        }
        // `near` stops at internal types (their users are the leaks); `far` follows
        // them, for the count and for the crates still to load.
        let far = reach(&edges, &covers, &me, refs, true);
        a.wanted.extend(
            far.iter()
                .map(|(t, _)| t.krate.as_str())
                .filter(|c| !loaded.contains(c))
                .map(str::to_owned),
        );
        let all = distinct(&far).len();
        if all > 0 {
            let shown = distinct(&near).into_iter().filter(|t| !internal(t)).count();
            a.seeds.push(Seed {
                pulled: all,
                internal_or_behind: all - shown,
                item: me,
            });
        }
    }
    a.seeds
        .sort_by(|x, y| y.pulled.cmp(&x.pulled).then(x.item.cmp(&y.item)));
    a.missing = list(missing);
    a.leaks = list(leaks);
    a.wanted.sort();
    a.wanted.dedup();
    a.decisions = covers.decisions;
    a.ambiguous = covers.ambiguous;
    a.stale = stale_rows(&named, &loaded, INTERNAL, TESTING, EXCLUDED);
    a
}

/// The texts the umbrella's exports need and the analysis of them. `fetch` gives
/// the `public-api` text of a crate, `Ok(None)` for a crate that is not a package.
/// It is asked for each crate the umbrella exports from, and then for each crate
/// the signatures reach that has no text yet, until none is left. A failure, or no
/// text for a crate the umbrella exports from, is an error; no text for a crate
/// reached only through signatures is a skip.
pub(crate) fn collect(
    lib: &str,
    fetch: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<(Vec<(String, String)>, Analysis), String> {
    let exporting = reexported_crates(lib);
    let mut texts: Vec<(String, String)> = Vec::new();
    let mut tried = BTreeSet::new();
    let mut skipped = Vec::new();
    let mut want = exporting.clone();
    loop {
        for krate in std::mem::take(&mut want) {
            if !tried.insert(krate.clone()) {
                continue;
            }
            match fetch(&krate).map_err(|e| format!("{krate}: {e}"))? {
                Some(text) => texts.push((krate, text)),
                None if exporting.contains(&krate) => {
                    return Err(format!("{krate}: no public-api text"));
                }
                None => skipped.push(krate),
            }
        }
        let refs: Vec<(&str, &str)> = texts
            .iter()
            .map(|(c, t)| (c.as_str(), t.as_str()))
            .collect();
        let mut a = analyze(&refs, lib);
        want = a
            .wanted
            .iter()
            .filter(|c| !tried.contains(*c))
            .cloned()
            .collect();
        if want.is_empty() {
            a.skipped = skipped;
            return Ok((texts, a));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIB: &str = "\
pub use tutti_graph::{Node, Shape as NodeShape, make_node};
pub mod types {
    pub use tutti_units::Samples;
    pub use tutti_units::value;
}
pub use tutti_nodes::SvfType;
";

    /// The texts a test gives: `tutti_graph` holds the items; `tutti_nodes`
    /// re-exports `SvfType`, which `tutti_dsp` defines, and defines nothing.
    fn run_in(lib: &str, api: &str) -> Analysis {
        analyze(
            &[
                ("tutti_graph", api),
                (
                    "tutti_nodes",
                    "pub struct tutti_nodes::LfoNode\npub use tutti_nodes::SvfType\n\
                     pub fn tutti_nodes::LfoNode::f(tutti_dsp::filter::SvfType)\n",
                ),
            ],
            lib,
        )
    }

    fn run(api: &str) -> Analysis {
        run_in(LIB, api)
    }

    fn missing(api: &str) -> Vec<String> {
        run(api).missing
    }

    fn none() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn an_attribute_before_pub_does_not_hide_a_line() {
        let api = "#[non_exhaustive] pub enum tutti_graph::Node\n\
                   #[non_exhaustive] pub fn tutti_graph::Node::cx(&self) -> tutti_graph::Cx\n";
        assert_eq!(missing(api), ["tutti_graph::Cx <- tutti_graph::Node"]);
    }

    #[test]
    fn a_path_cut_short_names_no_type() {
        let api = "pub struct tutti_graph::Node\npub fn tutti_graph::Node::a(tutti_graph::\n\
                   pub fn tutti_graph::Node::b(tutti_graph";
        assert_eq!(missing(api), none());
    }

    #[test]
    fn a_re_exported_name_covers_the_path_its_crate_defines_it_at() {
        let api = "pub use tutti_graph::Shape\n\
                   pub struct tutti_graph::spec::Shape\n\
                   pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::shape(&self) -> tutti_graph::spec::Shape\n";
        assert_eq!(missing(api), none());
        // Another module's `Shape` is another type.
        let api = format!("{api}pub fn tutti_graph::Node::b(&self) -> tutti_graph::other::Shape\n");
        assert_eq!(
            missing(&api),
            ["tutti_graph::other::Shape <- tutti_graph::Node"]
        );
    }

    #[test]
    fn an_exported_type_covers_every_path_of_its_name_in_its_crate_and_no_other_crate_s() {
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_graph::Node\n\
                   pub fn tutti_graph::Node::b(&self) -> tutti_graph::deep::Node\n";
        assert_eq!(missing(api), none());
        let api = format!("{api}pub fn tutti_graph::Node::c(&self) -> tutti_nodes::Node\n");
        assert_eq!(missing(&api), ["tutti_nodes::Node <- tutti_graph::Node"]);
    }

    #[test]
    fn an_unexported_type_is_reported_with_its_users() {
        let api = "pub fn tutti_graph::Node::cx(&self) -> tutti_graph::node::Cx\n\
                   pub fn tutti_graph::Node::prepare(&mut self, tutti_graph::node::Cx)\n\
                   pub fn tutti_graph::Shape::cx(&self) -> core::option::Option<tutti_graph::node::Cx>\n";
        assert_eq!(
            missing(api),
            ["tutti_graph::node::Cx <- tutti_graph::Node, tutti_graph::Shape"]
        );
    }

    #[test]
    fn a_type_another_unexported_type_names_is_reported_and_counted_for_the_seed() {
        let api = "pub struct tutti_graph::Cx\n\
                   pub fn tutti_graph::Node::cx(&self) -> tutti_graph::Cx\n\
                   pub fn tutti_graph::Cx::plan(&self) -> tutti_graph::Plan\n\
                   pub fn tutti_graph::Plan::ops(&self) -> tutti_graph::Op\n";
        let a = run(api);
        assert_eq!(
            a.missing,
            [
                "tutti_graph::Cx <- tutti_graph::Node",
                "tutti_graph::Op <- tutti_graph::Plan",
                "tutti_graph::Plan <- tutti_graph::Cx",
            ]
        );
        assert_eq!(
            a.seeds,
            [Seed {
                pulled: 3,
                internal_or_behind: 0,
                item: "tutti_graph::Node".to_owned()
            }]
        );
    }

    #[test]
    fn same_named_types_in_different_modules_keep_separate_edges() {
        let api = "pub fn tutti_graph::Node::a(&self) -> tutti_graph::a::Cx\n\
                   pub fn tutti_graph::a::Cx::p(&self) -> tutti_graph::P\n\
                   pub fn tutti_graph::b::Cx::q(&self) -> tutti_graph::Q\n";
        assert_eq!(
            missing(api),
            [
                "tutti_graph::P <- tutti_graph::a::Cx",
                "tutti_graph::a::Cx <- tutti_graph::Node",
            ]
        );
    }

    #[test]
    fn an_exported_name_covers_its_own_crate_only() {
        // `Error` of another crate is not covered by an exported `tutti_graph::Error`.
        let lib = "pub use tutti_graph::{Node, Error};\n";
        let graph = "pub enum tutti_graph::Error\n\
                     pub fn tutti_graph::Node::a(&self) -> tutti_graph::Error\n\
                     pub fn tutti_graph::Node::b(&self) -> tutti_engine::Error\n";
        let a = analyze(&[("tutti_graph", graph)], lib);
        assert_eq!(a.missing, ["tutti_engine::Error <- tutti_graph::Node"]);
        assert_eq!(a.wanted, ["tutti_engine"]);
    }

    #[test]
    fn a_module_exported_by_name_covers_the_paths_that_start_with_it() {
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_units::value::Unit\n\
                   pub fn tutti_graph::Node::b(&self) -> tutti_units::value::units::Beat\n\
                   pub fn tutti_graph::Node::c(&self) -> tutti_units::other::Unit\n\
                   pub fn tutti_graph::Node::d(&self) -> tutti_units::other::value::Unit\n\
                   pub fn tutti_graph::Node::e(&self) -> tutti_units::values::Unit\n";
        assert_eq!(
            missing(api),
            [
                "tutti_units::other::Unit <- tutti_graph::Node",
                "tutti_units::other::value::Unit <- tutti_graph::Node",
                "tutti_units::values::Unit <- tutti_graph::Node",
            ]
        );
    }

    #[test]
    fn a_name_a_crate_only_re_exports_covers_the_type_at_the_path_it_names() {
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_dsp::filter::SvfType\n\
                   pub fn tutti_graph::Node::b(&self) -> tutti_dsp::other::SvfType\n";
        assert_eq!(
            missing(api),
            ["tutti_dsp::other::SvfType <- tutti_graph::Node"]
        );
    }

    #[test]
    fn a_re_exported_name_covers_only_a_crate_the_re_exporter_names() {
        // `tutti_nodes` names `tutti_dsp`, not `tutti_engine`.
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_engine::SvfType\n";
        assert_eq!(missing(api), ["tutti_engine::SvfType <- tutti_graph::Node"]);
    }

    #[test]
    fn an_export_with_no_pub_use_line_in_its_crate_covers_only_itself() {
        let lib = "pub use tutti_nodes::{LfoNode, SvfType};\n";
        let nodes = "pub fn tutti_nodes::LfoNode::f(&self, tutti_dsp::filter::SvfType)\n";
        let a = analyze(&[("tutti_nodes", nodes)], lib);
        assert_eq!(
            a.missing,
            ["tutti_dsp::filter::SvfType <- tutti_nodes::LfoNode"]
        );
    }

    #[test]
    fn a_definition_with_a_qualifier_is_a_definition() {
        // `Raw` is defined at the exported path, so the name is the umbrella's and
        // `other::Raw`, the same name in the crate, is covered by it.
        for def in [
            "pub unsafe trait tutti_graph::Raw",
            "pub auto trait tutti_graph::Raw",
        ] {
            let api = format!(
                "pub use tutti_graph::Raw\n{def}\npub struct tutti_graph::Node\n\
                 pub fn tutti_graph::Node::a(&self) -> tutti_graph::other::Raw\n"
            );
            let lib = "pub use tutti_graph::{Node, Raw};\n";
            assert_eq!(run_in(lib, &api).missing, none(), "{def}");
        }
    }

    #[test]
    fn an_extern_fn_and_an_unsafe_trait_are_read() {
        let api = "pub extern \"C\" fn tutti_graph::make_node(tutti_graph::Cx)\n\
                   pub unsafe trait tutti_graph::Node: tutti_graph::Hook\n";
        assert_eq!(
            missing(api),
            [
                "tutti_graph::Cx <- tutti_graph::make_node",
                "tutti_graph::Hook <- tutti_graph::Node",
            ]
        );
    }

    #[test]
    fn an_aliased_export_covers_the_name_it_renames() {
        let api = "pub struct tutti_graph::Shape\n\
                   pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_graph::Shape\n";
        assert_eq!(missing(api), none());
    }

    #[test]
    fn a_type_named_at_its_private_module_is_the_type_the_umbrella_exports_by_name() {
        let texts = [
            (
                "tutti_graph",
                "pub struct tutti_graph::Node\n\
                 pub fn tutti_graph::Node::a(&self, tutti_units::value::samples::Samples)\n",
            ),
            (
                "tutti_units",
                "pub struct tutti_units::Samples(pub usize)\n",
            ),
        ];
        let lib = "pub use tutti_graph::Node;\npub use tutti_units::Samples;\n";
        assert_eq!(analyze(&texts, lib).missing, none());
        let lib = "pub use tutti_graph::Node;\n";
        assert_eq!(
            analyze(&texts, lib).missing,
            ["tutti_units::value::samples::Samples <- tutti_graph::Node"]
        );
    }

    #[test]
    fn a_private_path_is_not_aliased_when_the_name_is_exported_twice_or_the_path_is_published() {
        let graph = "pub struct tutti_graph::Node\n\
                     pub fn tutti_graph::Node::a(&self, tutti_units::hidden::Samples)\n";
        let covered = |types: &'static str, lib: &str| {
            analyze(&[("tutti_graph", graph), ("tutti_units", types)], lib).missing
        };
        let one = "pub struct tutti_units::Samples(pub usize)\n";
        let lib = "pub use tutti_graph::Node;\npub use tutti_units::Samples;\n";
        assert_eq!(covered(one, lib), none());
        // Two exports of the name leave the private path unresolved.
        let two = "pub struct tutti_units::Samples(pub usize)\n\
                   pub struct tutti_units::other::Samples(pub usize)\n";
        let lib2 = "pub use tutti_graph::Node;\npub use tutti_units::Samples;\n\
                    pub use tutti_units::other::Samples as OtherSamples;\n";
        assert_eq!(
            covered(two, lib2),
            ["tutti_units::hidden::Samples <- tutti_graph::Node"]
        );
        // A path in a public module, or with its own `pub use`, is its own type.
        for own in [
            "pub mod tutti_units::hidden\npub struct tutti_units::hidden::Samples(pub usize)\n",
            "pub use tutti_units::hidden::Samples\n",
        ] {
            let text: &'static str = Box::leak(format!("{one}{own}").into_boxed_str());
            assert_eq!(
                covered(text, lib),
                ["tutti_units::hidden::Samples <- tutti_graph::Node"],
                "{own}"
            );
        }
    }

    #[test]
    fn sealed_result_and_the_excluded_rows_are_not_asked_for() {
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_graph::Result<()>\n\
                   pub trait tutti_graph::Node: tutti_graph::private::Sealed\n\
                   pub fn tutti_graph::Node::b(&self, tutti_session::Detached)\n";
        assert_eq!(missing(api), none());
    }

    #[test]
    fn only_lines_whose_subject_is_exported_are_read() {
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Hidden::a(&self) -> tutti_graph::Cx\n\
                   impl tutti_graph::Hidden for tutti_graph::Cx\n\
                   pub fn tutti_graph::free_fn() -> tutti_graph::Cx\n\
                   pub fn tutti_other::Node::a(&self) -> tutti_graph::Cx\n";
        assert_eq!(missing(api), none());
    }

    #[test]
    fn a_struct_field_and_a_const_are_signatures_too() {
        let api = "pub tutti_graph::Node::field: tutti_graph::Cx\n\
                   pub const tutti_graph::Node::C: tutti_graph::Other\n";
        assert_eq!(
            missing(api),
            [
                "tutti_graph::Cx <- tutti_graph::Node",
                "tutti_graph::Other <- tutti_graph::Node"
            ]
        );
    }

    #[test]
    fn an_enum_variant_and_a_where_clause_are_signatures_too() {
        let api = "pub tutti_graph::Node::Variant(tutti_graph::Cx)\n\
                   pub tutti_graph::Node::Named::field: tutti_graph::Other\n\
                   pub fn tutti_graph::Node::f<T>(&self, T) where T: tutti_graph::Bound\n";
        assert_eq!(
            missing(api),
            [
                "tutti_graph::Bound <- tutti_graph::Node",
                "tutti_graph::Cx <- tutti_graph::Node",
                "tutti_graph::Other <- tutti_graph::Node",
            ]
        );
    }

    #[test]
    fn an_exported_free_function_is_a_seed() {
        let api = "pub fn tutti_graph::make_node(tutti_graph::Cx) -> tutti_graph::Node\n\
                   pub fn tutti_graph::other_fn(tutti_graph::Hidden)\n";
        let a = run(api);
        assert_eq!(a.missing, ["tutti_graph::Cx <- tutti_graph::make_node"]);
        assert_eq!(
            a.seeds,
            [Seed {
                pulled: 1,
                internal_or_behind: 0,
                item: "tutti_graph::make_node".to_owned()
            }]
        );
    }

    #[test]
    fn a_constant_is_known_by_its_definition_and_an_all_caps_type_is_a_type() {
        let api = "pub struct tutti_graph::Node\n\
                   pub const tutti_graph::MAX_CH: usize\n\
                   pub static tutti_graph::spec::Table: [u8; 4]\n\
                   pub struct tutti_graph::UMP\n\
                   pub fn tutti_graph::Node::a(&self) -> [f32; tutti_graph::MAX_CH]\n\
                   pub fn tutti_graph::Node::b(&self) -> &tutti_graph::spec::Table\n\
                   pub fn tutti_graph::Node::c(&self) -> tutti_graph::UMP\n\
                   pub fn tutti_graph::Node::d(&self) -> tutti_graph::SF2\n";
        // The path decides, not the case: `Table` is a static, `UMP` and `SF2` types.
        assert_eq!(
            missing(api),
            [
                "tutti_graph::SF2 <- tutti_graph::Node",
                "tutti_graph::UMP <- tutti_graph::Node"
            ]
        );
    }

    #[test]
    fn whole_and_glob_exports_name_no_item() {
        let lib = "pub use tutti_graph as graph;\npub use tutti_units::value::*;\n\
                   pub use tutti_graph::Node;\n";
        assert_eq!(reexported_crates(lib), ["tutti_graph"]);
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_units::value::Unit\n\
                   pub fn tutti_graph::Hidden::a(&self) -> tutti_graph::Cx\n";
        // The glob covers nothing, and the whole crate makes no export of its items.
        assert_eq!(
            run_in(lib, api).missing,
            ["tutti_units::value::Unit <- tutti_graph::Node"]
        );
    }

    #[test]
    fn an_internal_type_is_a_leak_and_counts_in_the_seed_second_column() {
        let api = "pub fn tutti_graph::Node::e(&self) -> tutti_graph::Editor\n\
                   pub fn tutti_graph::Editor::t(&self) -> tutti_graph::Topology\n\
                   pub fn tutti_graph::Editor::c(&self) -> tutti_graph::Cx\n";
        let a = run(api);
        assert_eq!(a.leaks, ["tutti_graph::Editor <- tutti_graph::Node"]);
        assert_eq!(a.missing, none());
        // Editor, and Topology and Cx behind it.
        assert_eq!(
            a.seeds,
            [Seed {
                pulled: 3,
                internal_or_behind: 3,
                item: "tutti_graph::Node".to_owned()
            }]
        );
        let api = "pub fn tutti_graph::Node::e(&self) -> tutti_graph::Editor\n\
                   pub fn tutti_graph::Node::c(&self) -> tutti_graph::Cx\n";
        let s = &run(api).seeds[0];
        assert_eq!((s.pulled, s.internal_or_behind), (2, 1));
    }

    #[test]
    fn the_exporting_crates_are_listed_once_each() {
        assert_eq!(
            reexported_crates(LIB),
            [
                "tutti_graph".to_owned(),
                "tutti_nodes".to_owned(),
                "tutti_units".to_owned()
            ]
        );
    }

    fn set(names: &[(&str, &str)]) -> BTreeSet<Ty> {
        names
            .iter()
            .map(|(c, n)| Ty {
                krate: (*c).to_owned(),
                mods: Vec::new(),
                name: (*n).to_owned(),
            })
            .collect()
    }

    #[test]
    fn a_row_no_named_type_matches_is_stale() {
        let named = set(&[("tutti_a", "Used"), ("tutti_a", "Seen")]);
        let loaded: BTreeSet<&str> = ["tutti_a", "tutti_b"].into();
        let stale = stale_rows(
            &named,
            &loaded,
            &[
                ("tutti_a", "Used", "why"),
                ("tutti_a", "Gone", "why"),
                ("tutti_a", "*", "why"),
                ("tutti_b", "*", "why"),
                ("tutti_c", "Elsewhere", "no text for its crate"),
            ],
            &[("tutti_a", "Seen"), ("tutti_a", "Renamed")],
            &["Seen", "Result"],
        );
        assert_eq!(
            stale,
            [
                "INTERNAL tutti_a::Gone",
                "INTERNAL tutti_b::*",
                "TESTING tutti_a::Renamed",
                "EXCLUDED Result",
            ]
        );
    }

    #[test]
    fn analyze_reports_the_stale_rows_of_the_tables() {
        let a = run("pub struct tutti_graph::Node\n");
        assert!(a.stale.contains(&"INTERNAL tutti_graph::Editor".to_owned()));
        assert!(!a.stale.iter().any(|r| r.contains("tutti_engine")));
    }

    fn fetch_of(
        texts: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Result<Option<String>, String> {
        move |k| {
            Ok(texts
                .iter()
                .find(|(c, _)| *c == k)
                .map(|(_, t)| (*t).to_owned()))
        }
    }

    const CHAIN: &[(&str, &str)] = &[
        (
            "tutti_a",
            "pub struct tutti_a::Item\npub fn tutti_a::Item::f(&self) -> tutti_b::X\n",
        ),
        (
            "tutti_b",
            "pub struct tutti_b::X\npub fn tutti_b::X::g(&self) -> tutti_c::Y\n",
        ),
        ("tutti_c", "pub struct tutti_c::Y\n"),
    ];

    #[test]
    fn a_crate_reached_only_through_another_unexported_type_is_loaded_and_read() {
        let (texts, a) = collect("pub use tutti_a::Item;\n", &fetch_of(CHAIN)).unwrap();
        let crates: Vec<&str> = texts.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(crates, ["tutti_a", "tutti_b", "tutti_c"]);
        assert_eq!(
            a.missing,
            ["tutti_b::X <- tutti_a::Item", "tutti_c::Y <- tutti_b::X"]
        );
        assert_eq!(a.seeds[0].pulled, 2);
        assert!(a.skipped.is_empty());
    }

    #[test]
    fn a_crate_reached_that_is_not_a_package_is_skipped() {
        let (_, a) = collect("pub use tutti_a::Item;\n", &fetch_of(&CHAIN[..2])).unwrap();
        assert_eq!(a.skipped, ["tutti_c"]);
        assert_eq!(
            a.missing,
            ["tutti_b::X <- tutti_a::Item", "tutti_c::Y <- tutti_b::X"]
        );
    }

    #[test]
    fn a_failed_or_missing_text_of_an_exporting_crate_is_an_error() {
        let lib = "pub use tutti_a::Item;\n";
        let failing = |_: &str| Err("cargo public-api failed: no such package".to_owned());
        let e = collect(lib, &failing).unwrap_err();
        assert!(
            e.starts_with("tutti_a: ") && e.contains("no such package"),
            "{e}"
        );
        let e = collect(lib, &fetch_of(&[])).unwrap_err();
        assert_eq!(e, "tutti_a: no public-api text");
        // A failure while reading a crate reached through signatures is one too.
        let reach = |k: &str| match k {
            "tutti_a" => Ok(Some(CHAIN[0].1.to_owned())),
            _ => Err("boom".to_owned()),
        };
        assert_eq!(collect(lib, &reach).unwrap_err(), "tutti_b: boom");
    }

    fn re_export(nodes: &str) -> Analysis {
        analyze(
            &[("tutti_nodes", nodes)],
            "pub use tutti_nodes::{LfoNode, Amp};\n",
        )
    }

    #[test]
    fn a_re_export_with_one_candidate_is_covered_and_reported_as_a_decision() {
        let a = re_export(
            "pub use tutti_nodes::Amp\n\
             pub fn tutti_nodes::LfoNode::f(&self, tutti_units::level::Amp, tutti_units::level::Other)\n",
        );
        assert_eq!(
            a.missing,
            ["tutti_units::level::Other <- tutti_nodes::LfoNode"]
        );
        assert_eq!(a.decisions, ["tutti_nodes::Amp -> tutti_units::level::Amp"]);
        assert_eq!(a.ambiguous, none());
    }

    #[test]
    fn a_name_defined_in_two_modules_is_ambiguous_and_covers_neither() {
        let a = re_export(
            "pub use tutti_nodes::Amp\n\
             pub struct tutti_nodes::a::Amp\n\
             pub struct tutti_nodes::b::Amp\n\
             pub fn tutti_nodes::LfoNode::f(&self, tutti_nodes::a::Amp, tutti_nodes::b::Amp)\n",
        );
        assert_eq!(
            a.ambiguous,
            ["tutti_nodes::Amp: ambiguous: tutti_nodes::a::Amp, tutti_nodes::b::Amp"]
        );
        assert_eq!(a.decisions, none());
        assert_eq!(
            a.missing,
            [
                "tutti_nodes::a::Amp <- tutti_nodes::LfoNode",
                "tutti_nodes::b::Amp <- tutti_nodes::LfoNode"
            ]
        );
    }

    #[test]
    fn a_leaf_named_from_two_crates_is_ambiguous() {
        let a = re_export(
            "pub use tutti_nodes::Amp\n\
             pub fn tutti_nodes::LfoNode::f(&self, tutti_units::Amp, tutti_dsp::Amp)\n",
        );
        assert_eq!(
            a.ambiguous,
            ["tutti_nodes::Amp: ambiguous: tutti_dsp::Amp, tutti_units::Amp"]
        );
        assert_eq!(a.decisions, none());
    }

    #[test]
    fn a_re_export_no_signature_names_is_unresolved() {
        let a = re_export("pub use tutti_nodes::Amp\npub struct tutti_nodes::LfoNode\n");
        assert_eq!(
            a.ambiguous,
            ["tutti_nodes::Amp: unresolved: no signature of its crate names it"]
        );
    }

    #[test]
    fn a_re_export_in_a_module_is_read_at_the_path_the_text_prints() {
        // A glob re-export prints as one `pub use` line per item, at the path of
        // the module the glob sits in; `lib.rs` names that path.
        let lib = "pub use tutti_nodes::automation::{Curve, Law};\n";
        let nodes = "pub use tutti_nodes::automation::Curve\n\
                     pub use tutti_nodes::automation::Law\n\
                     pub fn tutti_nodes::automation::Lane::f(&self, tutti_units::curve::Curve, tutti_units::curve::Law)\n";
        let a = analyze(&[("tutti_nodes", nodes)], lib);
        assert_eq!(
            a.decisions,
            [
                "tutti_nodes::automation::Curve -> tutti_units::curve::Curve",
                "tutti_nodes::automation::Law -> tutti_units::curve::Law"
            ]
        );
        assert_eq!(a.ambiguous, none());
    }
}
