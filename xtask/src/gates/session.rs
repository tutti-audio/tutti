//! `check-session`: `tutti-session` is the API layer, and not an umbrella
//! itself (design 015 section 8).
//!
//! Two assertions no compile can make:
//!
//! 1. **No Bevy crate in its normal dependency closure, with every feature on.**
//!    A `cargo tree`, not the workspace's metadata, which resolves features once
//!    for the whole workspace; `--all-features` because a Bevy crate behind an
//!    optional feature is absent from the default tree; `-e normal` keeps a
//!    dev-dependency walk out.
//! 2. **No whole-crate re-export in `tutti-session`**: its source re-exports no
//!    crate or module whole (`pub use tutti_x as ..;`, `pub use tutti_x;`,
//!    `pub use tutti_x::{self ..}`, or a glob anywhere under a `tutti_` path),
//!    with or without attributes in front, across lines, in groups nested to any
//!    depth and in a root group. Single items are allowed: the types its own
//!    signatures take. `pub(crate) use` is not a re-export. Each file is
//!    lexed (comments and literals blanked) and each `pub use .. ;` read as a use
//!    tree.

use super::lex::{blank, line_of, skipped};
use super::use_tree::{Leaf, statements};
use super::{Cargo, Report, Tree};

const SESSION: &str = "tutti-session";
const SESSION_SRC: &str = "crates/tutti-session/src";
/// The whole-crate leaves of one `pub use` statement, as the text the report
/// shows: a crate itself (with or without `as NAME`), `self`, or a glob at any
/// depth, each under a `tutti_` crate.
fn whole(leaves: &[Leaf]) -> Vec<String> {
    leaves
        .iter()
        .filter(|l| l.whole)
        .filter_map(|l| {
            let mut full: Vec<&str> = l.path.iter().map(String::as_str).collect();
            if l.source == "*" {
                full.push("*");
                return l.krate().starts_with("tutti_").then(|| full.join("::"));
            }
            full.push(&l.source);
            if l.is_self {
                full.push("self");
            }
            Some(full.join("::"))
        })
        .collect()
}

pub(super) fn check(tree_: &dyn Tree, cargo: &dyn Cargo) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let mut r = Report::default();

    // 1. The dependency closure.
    match cargo.tree(&[
        "-p",
        SESSION,
        "-e",
        "normal",
        "--all-features",
        "--prefix",
        "none",
    ]) {
        Err(e) => bad.push(format!("cargo tree -p {SESSION} did not run: {}", e.trim())),
        Ok(out) => {
            let mut crates: Vec<&str> = out
                .lines()
                .filter(|l| {
                    l.strip_prefix("bevy")
                        .is_some_and(|r| r.is_empty() || r.starts_with(['_', ' ']))
                })
                .collect();
            crates.sort_unstable();
            crates.dedup();
            for c in crates {
                bad.push(c.to_owned());
            }
            if !bad.is_empty() {
                bad.push(format!(
                    "a Bevy crate is in {SESSION}'s normal dependency closure (with --all-features)"
                ));
            }
        }
    }

    // 2. Whole-crate re-exports.
    let files: Vec<String> = tree_
        .files(SESSION_SRC, &skipped)
        .into_iter()
        .filter(|p| p.ends_with(".rs"))
        .collect();
    if files.is_empty() {
        bad.push(format!(
            "{SESSION_SRC} holds no .rs file; was the crate moved?"
        ));
    }
    for path in &files {
        let src = tree_.read(path).unwrap_or_default();
        let code = blank(&src);
        for stmt in statements(&code) {
            let (at, semi) = (stmt.at, stmt.semi);
            let hit = whole(&stmt.leaves);
            if hit.is_empty() {
                continue;
            }
            let text = src[at..=semi]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            bad.push(format!(
                "{path}:{}: {text}\n    whole: {} (tutti-session is not an umbrella: the umbrella is `tutti`; \
                 re-export the single items its own signatures take)",
                line_of(&code, at),
                hit.join(", ")
            ));
        }
    }

    if bad.is_empty() {
        r.say(format!(
            "check-session: ok ({SESSION} reaches no Bevy crate and re-exports no crate whole; {} files)",
            files.len()
        ));
    } else {
        r.fail("check-session: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::{Mem, TreeText};
    use super::*;

    const LIB: &str = "crates/tutti-session/src/lib.rs";

    fn clean() -> Mem {
        Mem::with(&[(
            LIB,
            "pub use tutti_graph::Editor;\npub(crate) use tutti_x;\n",
        )])
    }

    fn lib(src: &str) -> Report {
        let mut m = clean();
        m.files.insert(LIB.into(), src.into());
        check(
            &m,
            &TreeText("tutti-session v0\ntutti-graph v0\nbevytypo v0\n"),
        )
    }

    #[test]
    fn a_clean_session_passes() {
        let r = check(&clean(), &TreeText("tutti-session v0.1\n"));
        assert!(r.passed(), "{:?}", r.findings);
        assert_eq!(
            r.out[0],
            "check-session: ok (tutti-session reaches no Bevy crate and re-exports no crate whole; 1 files)"
        );
    }

    #[test]
    fn a_bevy_crate_in_the_closure_fails_and_a_lookalike_does_not() {
        let r = check(
            &clean(),
            &TreeText("tutti-session v0\nbevy_ecs v0.19\nbevy v0\nbevy_ecs v0.19\n"),
        );
        assert_eq!(r.findings[1], "  bevy v0");
        assert_eq!(r.findings[2], "  bevy_ecs v0.19");
        assert!(r.findings[3].contains("normal dependency closure"));
        assert!(check(&clean(), &TreeText("tutti-bevy v0\nbevytypo v0\n")).passed());
        let r = check(&clean(), &crate::gates::fake::NoCargo);
        assert!(r.findings[1].contains("cargo tree -p tutti-session did not run"));
    }

    #[test]
    fn whole_crate_reexports_in_every_spelling_fail() {
        for src in [
            "pub use tutti_modulation as modulation;\n",
            "pub use tutti_modulation;\n",
            "pub use tutti_modulation::*;\n",
            "pub use tutti_engine::prelude::*;\n",
            "#[cfg(feature = \"midi\")] pub use tutti_modulation as modulation;\n",
            "pub use tutti_modulation::{self as modulation};\n",
            "pub use tutti_modulation::{a::{B}, self};\n",
            "pub use {tutti_modulation as m};\n",
            "pub use ::tutti_modulation::{x::{y::{*}}};\n",
            "pub use tutti_modulation::{\n    a::{B, C},\n    self as m,\n};\n",
        ] {
            let r = lib(src);
            assert!(!r.passed(), "{src}");
            assert!(r.findings[1].contains("whole: "), "{src}");
        }
    }

    #[test]
    fn single_items_crate_private_uses_comments_and_strings_pass() {
        for src in [
            "pub use tutti_modulation::{a::{B}, C};\n",
            "pub use {tutti_modulation::A as M};\n",
            "pub(crate) use tutti_modulation;\n",
            "// pub use tutti_modulation;\nconst S: &str = \"pub use tutti_modulation;\";\n",
            "pub use std::*;\n",
        ] {
            assert!(lib(src).passed(), "{src}");
        }
    }

    #[test]
    fn the_report_names_file_line_and_statement() {
        let r = lib("\n\npub use tutti_modulation\n    as modulation;\n");
        assert!(r.findings[1].starts_with(&format!(
            "  {LIB}:3: pub use tutti_modulation as modulation;\n    whole: tutti_modulation"
        )));
    }

    #[test]
    fn a_session_with_no_source_fails() {
        let mut m = clean();
        m.files.remove(LIB);
        assert!(check(&m, &TreeText("")).findings[1].contains("holds no .rs file"));
    }
}
