//! `umbrella-api`: every type in the public signature of an item the umbrella
//! exports is nameable through the umbrella.
//!
//! The companion of `check-umbrella`, which holds that each name sits at one
//! path; this reads the other way, that no type a caller needs is missing. It is
//! a recipe and not a gate: it needs the docs nightly and `cargo-public-api`.
//!
//! The input is `cargo public-api --all-features -s` text for crates and the
//! umbrella's `lib.rs`. An *exported item* is the subject of a line (the first
//! capitalised path segment after the crate) that `lib.rs` exports. Every type
//! path of a `tutti_` crate in the rest of the line is a type the signature
//! names, and the types those name in turn; each is *covered* when `lib.rs`
//! exports:
//!
//! - the type from its crate (`pub use` of a list covers each name in it, not its
//!   alias; a crate's `Error` is not covered by another crate's);
//! - a module of its crate by name, which covers everything under it;
//! - the type from a crate that only re-exports it (the text of that crate holds
//!   no definition of the name), which covers the type at the path that defines
//!   it: that is a crate the text names.
//!
//! The result lists the types no export covers, each with the items that name it,
//! and ranks every exported item (a *seed*) by how many uncovered types its
//! signature pulls in, transitively. A leaky seed is dropped from `lib.rs`; one the umbrella
//! needs stays, and the internal types it names are rows of [`INTERNAL`].
//!
//! Not asked for: a private `Sealed` trait, which is unnameable on purpose; a
//! crate's `Result` alias, which expands to std's `Result` and an error type
//! the umbrella exports; the rows of [`TESTING`], items a crate keeps behind its
//! `testing` feature, which the all-features text still shows; and the rows of
//! [`OWNER_PRIVATE`], `pub` items in a private module of their crate, which no
//! path reaches.

use super::lex::blank;
use super::session::{is_word, pub_uses, tokens};
use super::umbrella::{Leaf, leaves};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// `(crate, type, why)` rows: types the umbrella does not name because they are
/// internals of their crate (the engine, the executor, the compiler). An exported
/// item whose signature names one is a finding for the item's owner, listed in
/// [`Analysis::leaks`] and not asked for. A type of `*` is every type of the crate.
const INTERNAL: &[(&str, &str, &str)] = &[
    (
        "tutti_engine",
        "StreamDriver",
        "the engine's stream driver, which a custom device backend implements",
    ),
    ("tutti_transport", "Transport", "the engine's transport"),
    (
        "tutti_transport",
        "OfflineTransport",
        "the export render's transport",
    ),
    (
        "tutti_transport",
        "RenderClock",
        "the export render's clock",
    ),
    (
        "tutti_transport",
        "OfflineClock",
        "the export render's clock",
    ),
    (
        "tutti_graph",
        "Editor",
        "the graph editor the session wraps",
    ),
    ("tutti_graph", "Topology", "the compiler's view of a graph"),
    (
        "tutti_graph",
        "GraphInvalid",
        "the compiler's refusal of a graph",
    ),
    ("tutti_graph", "CompileError", "the compiler's error"),
    ("tutti_graph", "VerifyError", "the compiler's error"),
    ("tutti_graph", "InletNode", "the node behind an inlet"),
    ("tutti_graph", "InletRing", "the ring behind an inlet"),
    (
        "tutti_graph",
        "ParamMod",
        "the compiler's parameter modulation record",
    ),
    (
        "tutti_graph",
        "ParamRange",
        "the compiler's parameter range record",
    ),
    ("tutti_graph", "TapWriter", "the audio side of a tap"),
    (
        "tutti_graph",
        "Compensation",
        "the compiler's latency compensation record",
    ),
    (
        "tutti_graph",
        "EventEdge",
        "the compiler's event edge, which `EventEdge` of the session shadows",
    ),
    (
        "tutti_graph",
        "Edge",
        "the compiler's edge, which `Edge` of the session shadows",
    ),
    (
        "tutti_graph",
        "InPort",
        "the compiler's port, which `InPort` of the session shadows",
    ),
    (
        "tutti_graph",
        "OutPort",
        "the compiler's port, which `OutPort` of the session shadows",
    ),
    (
        "tutti_graph",
        "Source",
        "a graph edge's source, which the buffers' `Source` shadows",
    ),
    (
        "tutti_graph",
        "Sink",
        "a graph edge's sink, which the buffers' `Sink` shadows",
    ),
    (
        "tutti_plugin",
        "PluginClient",
        "the plugin host's client of a served plugin",
    ),
    (
        "tutti_plugin_host",
        "PluginHandle",
        "the plugin host's control handle",
    ),
    (
        "tutti_midi_protocol",
        "MmcCommand",
        "the MMC command model, about 98 types",
    ),
    (
        "tutti_midi_hardware",
        "MidiEndpoints",
        "the OS MIDI endpoint set, about 52 types",
    ),
    (
        "tutti_midi_hardware",
        "DeviceStream",
        "an open device's stream, about 28 types",
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
        "the modulation compiler's declarations",
    ),
    (
        "tutti_modulation",
        "Source",
        "the modulation compiler's signal source",
    ),
    (
        "tutti_cpal",
        "Error",
        "the device backend's error, which the session's `Error` wraps",
    ),
    (
        "tutti_midi_file",
        "Error",
        "the file codec's error, which `midi::Error` nests",
    ),
    (
        "tutti_midi_hardware",
        "Error",
        "the hardware layer's error, which `midi::Error` nests",
    ),
    ("tutti_alsa_ffi", "*", "not a dependency of the umbrella"),
    ("tutti_measure", "*", "not a dependency of the umbrella"),
];

/// `(crate, type)` rows that share a name with a type another crate defines and
/// the umbrella re-exports from a crate that only re-exports it: they are not the
/// exported type.
const DISTINCT: &[(&str, &str)] = &[
    ("tutti_graph", "Playhead"),
    ("tutti_graph", "Lookahead"),
    ("tutti_midi_device", "Outcome"),
    ("tutti_modulation", "Clock"),
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

/// `(crate, type)` rows that are `pub` in a private module of their crate, so no
/// path reaches them: the owning crate has to export them first.
const OWNER_PRIVATE: &[(&str, &str)] = &[
    ("tutti_nodes", "BrickwallSet"),
    ("tutti_nodes", "CompressorSet"),
    ("tutti_nodes", "CrossoverSet"),
    ("tutti_nodes", "DelaySet"),
    ("tutti_nodes", "DistortionSet"),
    ("tutti_nodes", "FdnSet"),
    ("tutti_nodes", "FreeverbSet"),
    ("tutti_nodes", "GateSet"),
    ("tutti_nodes", "HasControl"),
    ("tutti_nodes", "LadderSet"),
    ("tutti_nodes", "LimiterSet"),
    ("tutti_nodes", "ModDelaySet"),
    ("tutti_nodes", "PhaserSet"),
    ("tutti_nodes", "SvfSet"),
];

/// The `pub use` leaves of `lib` that name an item of a `tutti_` crate.
fn exports(lib: &str) -> Vec<Leaf> {
    let code = blank(lib);
    let mut out = Vec::new();
    for (at, semi) in pub_uses(&code) {
        let body = code[at + 3..semi].trim_start();
        let t = tokens(&body["use".len()..]);
        let (mut k, mut found) = (0, Vec::new());
        leaves(&t, &mut k, &[], &mut found);
        out.extend(
            found
                .into_iter()
                .filter(|l| !l.whole && l.krate.starts_with("tutti_")),
        );
    }
    out
}

/// The `tutti_` crates the umbrella exports from, sorted.
pub(crate) fn reexported_crates(lib: &str) -> Vec<String> {
    let set: BTreeSet<String> = exports(lib).into_iter().map(|l| l.krate).collect();
    set.into_iter().collect()
}

/// A type a signature names: its crate, the modules on the path and its name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Ty {
    krate: String,
    mods: Vec<String>,
    name: String,
}

impl Ty {
    fn id(&self) -> (String, String) {
        (self.krate.clone(), self.name.clone())
    }
}

/// The type the crate path at `at` names (crate, modules, item), and the offset after it.
fn type_at(s: &str, at: usize) -> Option<(Ty, usize)> {
    let b = s.as_bytes();
    let word = |from: usize| from + b[from..].iter().take_while(|&&c| is_word(c)).count();
    let end = word(at);
    let krate = s[at..end].to_owned();
    let mut mods = Vec::new();
    let mut i = end;
    loop {
        if !s[i..].starts_with("::") {
            return None;
        }
        i += 2;
        let e = word(i);
        if e == i {
            return None;
        }
        if b[i].is_ascii_uppercase() {
            let name = s[i..e].to_owned();
            return Some((Ty { krate, mods, name }, e));
        }
        mods.push(s[i..e].to_owned());
        i = e;
    }
}

/// Every `tutti_` type path in `s`.
fn types_in(s: &str) -> Vec<Ty> {
    let b = s.as_bytes();
    s.match_indices("tutti_")
        .filter(|(i, _)| *i == 0 || !is_word(b[i - 1]))
        .filter_map(|(i, _)| type_at(s, i).map(|(t, _)| t))
        .collect()
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

/// The `tutti_` crates whose paths `text` names.
fn crates_named(text: &str) -> BTreeSet<String> {
    let b = text.as_bytes();
    text.match_indices("tutti_")
        .filter(|(i, _)| *i == 0 || !is_word(b[i - 1]))
        .filter_map(|(i, _)| {
            let end = i + b[i..].iter().take_while(|&&c| is_word(c)).count();
            text[end..]
                .starts_with("::")
                .then(|| text[i..end].to_owned())
        })
        .collect()
}

/// What the umbrella exports, as the coverage rules read it.
struct Covers {
    /// `(crate, item)` of `pub use tutti_x::Item`.
    items: BTreeSet<(String, String)>,
    /// `(crate, module)` of `pub use tutti_x::module`.
    modules: BTreeSet<(String, String)>,
    /// Names exported from a crate that only re-exports them, with the crates
    /// that crate's text names (the one it re-exports from is among them).
    foreign: BTreeMap<String, BTreeSet<String>>,
}

impl Covers {
    fn new(lib: &str, texts: &[(&str, &str)]) -> Covers {
        // `(crate, name)` of the definitions each crate's text holds.
        let mut defined = BTreeSet::new();
        for (krate, text) in texts {
            for line in text.lines() {
                let Some(rest) = public(line) else {
                    continue;
                };
                let Some(rest) = ["struct ", "enum ", "trait ", "union ", "type ", "macro "]
                    .iter()
                    .find_map(|k| rest.strip_prefix(k))
                else {
                    continue;
                };
                if let Some((t, end)) = type_at(rest, 0)
                    && !rest[end..].starts_with("::")
                {
                    defined.insert((t.krate.clone(), t.name.clone()));
                    let _ = krate;
                }
            }
        }
        let loaded: BTreeSet<&str> = texts.iter().map(|(c, _)| *c).collect();
        let mut c = Covers {
            items: BTreeSet::new(),
            modules: BTreeSet::new(),
            foreign: BTreeMap::new(),
        };
        for l in exports(lib) {
            if l.source.starts_with(|ch: char| ch.is_ascii_lowercase()) {
                c.modules.insert((l.krate, l.source));
            } else {
                if loaded.contains(l.krate.as_str())
                    && !defined.contains(&(l.krate.clone(), l.source.clone()))
                {
                    let text = texts
                        .iter()
                        .find(|(k, _)| *k == l.krate)
                        .map_or("", |(_, t)| *t);
                    c.foreign
                        .entry(l.source.clone())
                        .or_default()
                        .extend(crates_named(text));
                }
                c.items.insert((l.krate, l.source));
            }
        }
        c
    }

    fn covers(&self, t: &Ty) -> bool {
        self.items.contains(&t.id())
            || self
                .foreign
                .get(&t.name)
                .is_some_and(|named| named.contains(&t.krate))
                && !DISTINCT.contains(&(t.krate.as_str(), t.name.as_str()))
            || t.mods
                .iter()
                .any(|m| self.modules.contains(&(t.krate.clone(), m.clone())))
    }
}

/// What the recipe reports.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Analysis {
    /// `crate::Type <- users` for each type no export covers.
    pub(crate) missing: Vec<String>,
    /// `crate::Type <- users` for each [`INTERNAL`] type an exported item names.
    pub(crate) leaks: Vec<String>,
    /// `(types pulled in, of them reached through an internal type, item)` for each
    /// exported item that pulls in any not exported, most first.
    pub(crate) seeds: Vec<(usize, usize, String)>,
    /// Crates a signature names whose text was not given.
    pub(crate) wanted: Vec<String>,
}

/// The signature types of every item in `texts`, by the item's `(crate, name)`.
fn edges(texts: &[(&str, &str)]) -> BTreeMap<(String, String), (Ty, Vec<Ty>)> {
    let mut out: BTreeMap<(String, String), (Ty, Vec<Ty>)> = BTreeMap::new();
    for (_, text) in texts {
        for line in text.lines() {
            let Some(rest) = public(line) else {
                continue;
            };
            // The words before the path (`fn`, `struct`, `const`, ..).
            let Some(at) = rest.find("tutti_") else {
                continue;
            };
            if !rest[..at].split(' ').all(|w| w.bytes().all(is_word)) {
                continue;
            }
            let Some((subject, end)) = type_at(rest, at) else {
                continue;
            };
            out.entry(subject.id())
                .or_insert_with(|| (subject, Vec::new()))
                .1
                .extend(types_in(&rest[end..]));
        }
    }
    out
}

/// The uncovered types `refs` name, transitively, internal ones included.
fn reach(
    edges: &BTreeMap<(String, String), (Ty, Vec<Ty>)>,
    covers: &Covers,
    skip: &dyn Fn(&Ty) -> bool,
    refs: &[Ty],
) -> BTreeSet<(String, String)> {
    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<&Ty> = refs.iter().collect();
    while let Some(t) = queue.pop_front() {
        if covers.covers(t) || skip(t) || !seen.insert(t.id()) {
            continue;
        }
        queue.extend(edges.get(&t.id()).map(|e| &e.1).into_iter().flatten());
    }
    seen
}

/// The types the exported items of `texts` name that the umbrella does not export,
/// and the seeds ranked by how many each pulls in.
pub(crate) fn analyze(texts: &[(&str, &str)], lib: &str) -> Analysis {
    let covers = Covers::new(lib, texts);
    let edges = edges(texts);
    let loaded: BTreeSet<&str> = texts.iter().map(|(c, _)| *c).collect();
    let internal = |t: &Ty| {
        INTERNAL
            .iter()
            .any(|(c, n, _)| *c == t.krate && (*n == "*" || *n == t.name))
    };
    let skip = |t: &Ty| {
        EXCLUDED.contains(&t.name.as_str())
            || TESTING.contains(&(t.krate.as_str(), t.name.as_str()))
            || OWNER_PRIVATE.contains(&(t.krate.as_str(), t.name.as_str()))
    };
    let mut seeds = Vec::new();
    let mut missing: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut leaks: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut wanted = BTreeSet::new();
    for ((krate, item), (subject, refs)) in &edges {
        if !covers.covers(subject) {
            continue;
        }
        let me = format!("{krate}::{item}");
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        let mut queue: VecDeque<(Ty, String)> =
            refs.iter().map(|t| (t.clone(), me.clone())).collect();
        while let Some((t, user)) = queue.pop_front() {
            if !loaded.contains(t.krate.as_str()) {
                wanted.insert(t.krate.clone());
            }
            if covers.covers(&t) || skip(&t) {
                continue;
            }
            if internal(&t) {
                leaks.entry(t.id()).or_default().insert(user);
                continue;
            }
            missing.entry(t.id()).or_default().insert(user);
            if !seen.insert(t.id()) {
                continue;
            }
            let here = format!("{}::{}", t.krate, t.name);
            for next in edges.get(&t.id()).map(|e| &e.1).into_iter().flatten() {
                queue.push_back((next.clone(), here.clone()));
            }
        }
        // The same walk through the internal types, to size what they hide.
        let all = reach(&edges, &covers, &skip, refs);
        if !all.is_empty() {
            seeds.push((all.len(), all.len() - seen.len(), me));
        }
    }
    seeds.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.cmp(&b.2)));
    let list = |m: BTreeMap<(String, String), BTreeSet<String>>| -> Vec<String> {
        m.into_iter()
            .map(|((c, ty), users)| {
                let more = if users.len() > 4 { " ..." } else { "" };
                let users: Vec<String> = users.into_iter().take(4).collect();
                format!("{c}::{ty} <- {}{more}", users.join(", "))
            })
            .collect()
    };
    Analysis {
        missing: list(missing),
        leaks: list(leaks),
        seeds,
        wanted: wanted.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIB: &str = "\
pub use tutti_graph::{Node, Shape as NodeShape};
pub mod types {
    pub use tutti_types::Samples;
    pub use tutti_types::value;
}
pub use tutti_nodes::SvfType;
";

    /// The texts a test gives: `tutti_graph` holds the items and defines `Node`
    /// and `Shape`; `tutti_nodes` re-exports `SvfType` and defines nothing.
    fn run(api: &str) -> Analysis {
        analyze(
            &[
                ("tutti_graph", api),
                (
                    "tutti_nodes",
                    "pub struct tutti_nodes::LfoNode\npub use tutti_nodes::SvfType\n\
                     pub fn tutti_nodes::LfoNode::f(tutti_dsp::filter::SvfType)\n",
                ),
            ],
            LIB,
        )
    }

    fn missing(api: &str) -> Vec<String> {
        run(api).missing
    }

    #[test]
    fn an_attribute_before_pub_does_not_hide_a_line() {
        let api = "#[non_exhaustive] pub enum tutti_graph::Node\n\
                   #[non_exhaustive] pub fn tutti_graph::Node::cx(&self) -> tutti_graph::Cx\n";
        assert_eq!(missing(api), ["tutti_graph::Cx <- tutti_graph::Node"]);
    }

    #[test]
    fn a_signature_of_exported_types_is_nameable() {
        let api = "pub struct tutti_graph::Node\n\
                   pub struct tutti_graph::Shape\n\
                   pub fn tutti_graph::Node::shape(&self) -> tutti_graph::spec::Shape\n\
                   pub fn tutti_graph::Node::len(&self) -> tutti_types::meter::Samples\n";
        assert_eq!(missing(api), Vec::<String>::new());
    }

    #[test]
    fn an_unexported_type_is_reported_with_its_users() {
        let api = "pub fn tutti_graph::Node::cx(&self) -> tutti_graph::node::Cx\n\
                   pub fn tutti_graph::Node::prepare(&mut self, tutti_graph::node::Cx)\n\
                   pub fn tutti_graph::Shape::cx(&self) -> core::option::Option<tutti_graph::Cx>\n";
        assert_eq!(
            missing(api),
            ["tutti_graph::Cx <- tutti_graph::Node, tutti_graph::Shape"]
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
        assert_eq!(a.seeds, [(3, 0, "tutti_graph::Node".to_owned())]);
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
    fn a_module_exported_by_name_covers_what_is_under_it() {
        // `value` is a module of `tutti_types`; `Unit` is under it.
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_types::value::Unit\n\
                   pub fn tutti_graph::Node::b(&self) -> tutti_types::other::Unit\n";
        assert_eq!(missing(api), ["tutti_types::Unit <- tutti_graph::Node"]);
    }

    #[test]
    fn a_name_a_crate_only_re_exports_covers_the_type_at_its_defining_path() {
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_dsp::filter::SvfType\n";
        assert_eq!(missing(api), Vec::<String>::new());
    }

    #[test]
    fn a_re_exported_name_covers_only_a_crate_the_re_exporter_names() {
        // `tutti_nodes` names `tutti_dsp`, not `tutti_engine`.
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_engine::SvfType\n";
        assert_eq!(missing(api), ["tutti_engine::SvfType <- tutti_graph::Node"]);
    }

    #[test]
    fn an_aliased_export_covers_the_name_it_renames() {
        let api = "pub struct tutti_graph::Shape\n\
                   pub fn tutti_graph::Node::a(&self) -> tutti_graph::Shape\n";
        assert_eq!(missing(api), Vec::<String>::new());
    }

    #[test]
    fn sealed_result_and_the_excluded_rows_are_not_asked_for() {
        let api = "pub fn tutti_graph::Node::a(&self) -> tutti_graph::Result<()>\n\
                   pub trait tutti_graph::Node: tutti_graph::private::Sealed\n\
                   pub fn tutti_graph::Node::b(&self, tutti_session::Detached)\n\
                   pub fn tutti_graph::Node::c(&self, tutti_nodes::SvfSet)\n";
        assert_eq!(missing(api), Vec::<String>::new());
    }

    #[test]
    fn only_lines_whose_subject_is_exported_are_read() {
        let api = "pub struct tutti_graph::Node\n\
                   pub fn tutti_graph::Hidden::a(&self) -> tutti_graph::Cx\n\
                   impl tutti_graph::Hidden for tutti_graph::Cx\n\
                   pub fn tutti_graph::free_fn() -> tutti_graph::Cx\n\
                   pub fn tutti_other::Node::a(&self) -> tutti_graph::Cx\n";
        assert_eq!(missing(api), Vec::<String>::new());
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
    fn the_exporting_crates_are_listed_once_each() {
        assert_eq!(
            reexported_crates(LIB),
            [
                "tutti_graph".to_owned(),
                "tutti_nodes".to_owned(),
                "tutti_types".to_owned()
            ]
        );
    }
}
