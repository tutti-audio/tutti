//! `umbrella-api`: every type in the public signature of an item the umbrella
//! exports is nameable through the umbrella.
//!
//! The companion of `check-umbrella`, which holds that each name sits at one
//! path; this reads the other way, that no name a caller needs is missing. It is
//! a recipe and not a gate: it needs the docs nightly and `cargo-public-api`.
//!
//! The input is `cargo public-api --all-features -s` text for each crate the
//! umbrella exports from, and the umbrella's `lib.rs`. An *exported item* is a
//! line of those texts whose subject (`pub fn tutti_x::Item::method ..`, the first
//! capitalised path segment) is a `pub use tutti_x::..::Item` of `lib.rs`. Every
//! `tutti_y::..::Type` in the rest of the line is a type the signature names; it
//! is nameable when some `pub use` exports a name or an `as` alias equal to
//! `Type`. The match is by name: a type is printed at the path that defines it,
//! which a re-export does not repeat.
//!
//! Not asked for: a private `Sealed` trait, which is unnameable on purpose; a
//! crate's `Result` alias, which expands to std's `Result` and an error type the
//! umbrella exports; and the rows of [`TESTING`], items a crate keeps behind its
//! `testing` feature, which the all-features text still shows; and the rows of
//! [`OWNER_PRIVATE`], `pub` items in a private module of their crate, which no
//! path reaches.

use super::lex::blank;
use super::session::{is_word, pub_uses, tokens};
use super::umbrella::{Leaf, leaves};
use std::collections::{BTreeMap, BTreeSet};

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

/// The crate and the first capitalised segment of a path (`tutti_` crate, modules, item),
/// and the offset after that segment.
fn type_at(s: &str, at: usize) -> Option<(String, String, usize)> {
    let b = s.as_bytes();
    let word = |from: usize| from + b[from..].iter().take_while(|&&c| is_word(c)).count();
    let end = word(at);
    let krate = &s[at..end];
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
            return Some((krate.to_owned(), s[i..e].to_owned(), e));
        }
        i = e;
    }
}

/// Every `tutti_` path in `s` as `(crate, type)`, with the offset past it.
fn types_in(s: &str) -> Vec<(String, String, usize)> {
    let b = s.as_bytes();
    s.match_indices("tutti_")
        .filter(|(i, _)| *i == 0 || !is_word(b[i - 1]))
        .filter_map(|(i, _)| type_at(s, i))
        .collect()
}

/// The types the exported items' signatures name that the umbrella does not,
/// each as `crate::Type <- users` (up to four users, sorted).
pub(crate) fn unnameable(texts: &[(&str, &str)], lib: &str) -> Vec<String> {
    let ex = exports(lib);
    let names: BTreeSet<&str> = ex
        .iter()
        .flat_map(|l| [l.name.as_str(), l.source.as_str()])
        .collect();
    let items: BTreeSet<(&str, &str)> = ex
        .iter()
        .map(|l| (l.krate.as_str(), l.source.as_str()))
        .collect();
    let mut missing: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (_, text) in texts {
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("pub ") else {
                continue;
            };
            // The words before the path (`fn`, `struct`, `const`, ..).
            let Some(at) = rest.find("tutti_") else {
                continue;
            };
            if !rest[..at].split(' ').all(|w| w.bytes().all(is_word)) {
                continue;
            }
            let Some((krate, item, end)) = type_at(rest, at) else {
                continue;
            };
            if !items.contains(&(krate.as_str(), item.as_str())) {
                continue;
            }
            for (c, ty, _) in types_in(&rest[end..]) {
                let skip = EXCLUDED.contains(&ty.as_str())
                    || names.contains(ty.as_str())
                    || TESTING.contains(&(c.as_str(), ty.as_str()))
                    || OWNER_PRIVATE.contains(&(c.as_str(), ty.as_str()));
                if !skip {
                    missing
                        .entry((c, ty))
                        .or_default()
                        .insert(format!("{krate}::{item}"));
                }
            }
        }
    }
    missing
        .into_iter()
        .map(|((c, ty), users)| {
            let more = if users.len() > 4 { " ..." } else { "" };
            let users: Vec<String> = users.into_iter().take(4).collect();
            format!("{c}::{ty} <- {}{more}", users.join(", "))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIB: &str = "\
pub use tutti_graph::{Node, Shape as NodeShape};
pub mod types {
    pub use tutti_types::Samples;
}
";

    fn run(api: &str) -> Vec<String> {
        unnameable(&[("tutti_graph", api)], LIB)
    }

    #[test]
    fn a_signature_of_exported_types_is_nameable() {
        let api = "pub fn tutti_graph::Node::shape(&self) -> tutti_graph::spec::Shape\n\
                   pub fn tutti_graph::Node::len(&self) -> tutti_types::value::Samples\n";
        assert_eq!(run(api), Vec::<String>::new());
    }

    #[test]
    fn an_unexported_type_is_reported_with_its_users() {
        let api = "pub fn tutti_graph::Node::cx(&self) -> tutti_graph::node::Cx\n\
                   pub fn tutti_graph::Node::prepare(&mut self, tutti_graph::node::Cx)\n\
                   pub fn tutti_graph::Shape::cx(&self) -> core::option::Option<tutti_graph::Cx>\n";
        assert_eq!(
            run(api),
            ["tutti_graph::Cx <- tutti_graph::Node, tutti_graph::Shape"]
        );
    }

    #[test]
    fn an_alias_name_counts_and_so_does_the_name_it_renames() {
        // Shape is exported as an alias: either spelling is a name.
        let api = "pub fn tutti_graph::Node::a(&self) -> tutti_graph::Shape\n\
                   pub fn tutti_graph::Node::b(&self) -> tutti_graph::NodeShape\n";
        assert_eq!(run(api), Vec::<String>::new());
    }

    #[test]
    fn sealed_result_and_the_testing_rows_are_not_asked_for() {
        let api = "pub fn tutti_graph::Node::a(&self) -> tutti_graph::Result<()>\n\
                   pub trait tutti_graph::Node: tutti_graph::private::Sealed\n\
                   pub fn tutti_graph::Node::b(&self, tutti_session::Detached)\n\
                   pub fn tutti_graph::Node::c(&self, tutti_nodes::SvfSet)\n";
        assert_eq!(run(api), Vec::<String>::new());
    }

    #[test]
    fn only_lines_whose_subject_is_exported_are_read() {
        let api = "pub fn tutti_graph::Hidden::a(&self) -> tutti_graph::Cx\n\
                   impl tutti_graph::Hidden for tutti_graph::Cx\n\
                   pub fn tutti_graph::free_fn() -> tutti_graph::Cx\n\
                   pub fn tutti_other::Node::a(&self) -> tutti_graph::Cx\n";
        assert_eq!(run(api), Vec::<String>::new());
    }

    #[test]
    fn a_struct_field_and_a_const_are_signatures_too() {
        let api = "pub tutti_graph::Node::field: tutti_graph::Cx\n\
                   pub const tutti_graph::Node::C: tutti_graph::Other\n";
        assert_eq!(
            run(api),
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
            ["tutti_graph".to_owned(), "tutti_types".to_owned()]
        );
    }
}
