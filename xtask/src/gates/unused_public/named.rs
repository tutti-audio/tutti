//! The textual second look at items the index shows no use of.
//!
//! The index records no reference made inside a macro's expansion (`Sweep::new`,
//! which `declare_params!` calls in every node that declares params) and none made by
//! a doc example (``` blocks in `///` and `//!` comments, and in the book's Markdown),
//! which nothing but `cargo test --doc` compiles. So an item's name is searched for in
//! the text of both. The match is by name: a method by `.name(` or `Type::name`, a free
//! function by `name(`, any other item by its bare name. It can over-label, and that is
//! the safe direction.

use super::{Item, Kind};
use crate::gates::Tree;
use crate::gates::building::macro_bodies;
use crate::gates::lex::{blank, line_of, skipped, word};

/// A line of text and where it is.
type Line = (String, usize, String);

/// The code lines of doc examples and the lines of `macro_rules!` bodies.
#[derive(Debug, Default)]
pub(super) struct Corpus {
    docs: Vec<Line>,
    macros: Vec<Line>,
}

/// The first doc-example hit of an item.
pub(super) type Hit = (String, usize);

/// Info strings of a fence that are not Rust.
const OTHER: [&str; 9] = [
    "text", "console", "toml", "sh", "bash", "json", "yaml", "python", "markdown",
];

/// The code lines of the fenced blocks of `text`; `comment` reads only `///` and `//!`
/// lines (a Rust file), and no other line carries a fence open.
fn fenced(file: &str, text: &str, comment: bool, out: &mut Vec<Line>) {
    // 0: outside a fence, 1: in a Rust block, 2: in a block of another language.
    let mut state = 0u8;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_start();
        let body = if comment {
            match line
                .strip_prefix("///")
                .or_else(|| line.strip_prefix("//!"))
            {
                Some(b) if !b.starts_with('/') => b.strip_prefix(' ').unwrap_or(b),
                _ => {
                    state = 0;
                    continue;
                }
            }
        } else {
            line
        };
        if let Some(info) = body.trim_start().strip_prefix("```") {
            state = match state {
                0 if OTHER.iter().any(|o| info.trim().starts_with(o)) => 2,
                0 => 1,
                _ => 0,
            };
            continue;
        }
        if state == 1 {
            out.push((file.to_owned(), i + 1, body.to_owned()));
        }
    }
}

impl Corpus {
    /// The corpus of `crates/` sources and the Markdown under `book/`.
    pub(super) fn read(tree: &dyn Tree) -> Corpus {
        let mut files: Vec<(String, String)> = Vec::new();
        for dir in ["crates", "testing"] {
            for p in tree.files(dir, &skipped) {
                if p.ends_with(".rs")
                    && let Some(t) = tree.read(&p)
                {
                    files.push((p, t));
                }
            }
        }
        for p in tree.files("book", &skipped) {
            if p.ends_with(".md")
                && let Some(t) = tree.read(&p)
            {
                files.push((p, t));
            }
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        Corpus::from_texts(&refs)
    }

    pub(super) fn from_texts(files: &[(&str, &str)]) -> Corpus {
        let mut c = Corpus::default();
        for (path, text) in files {
            fenced(path, text, path.ends_with(".rs"), &mut c.docs);
            if !path.ends_with(".rs") {
                continue;
            }
            let code = blank(text);
            for (open, close, _) in macro_bodies(&code) {
                let first = line_of(&code, open);
                for (k, l) in code[open..close].lines().enumerate() {
                    c.macros.push(((*path).to_owned(), first + k, l.to_owned()));
                }
            }
        }
        c
    }

    /// The first doc-example line that names `it`.
    pub(super) fn doc_example(&self, it: &Item) -> Option<Hit> {
        self.docs
            .iter()
            .find(|(_, _, l)| doc_names(l, it))
            .map(|(f, l, _)| (f.clone(), *l))
    }

    /// Whether a `macro_rules!` body names `it`.
    pub(super) fn in_macro(&self, it: &Item) -> bool {
        self.macros.iter().any(|(_, _, l)| macro_names(l, it))
    }
}

/// Whether a doc-example line names `it`: `.name(` or `Type::name` for a method,
/// `name(` for a free function, the bare name for anything else.
fn doc_names(line: &str, it: &Item) -> bool {
    match (it.kind == Kind::Fn, &it.owner) {
        (true, Some(t)) => {
            has(line, &format!(".{}(", it.name)) || has(line, &format!("{t}::{}", it.name))
        }
        (true, None) => has(line, &format!("{}(", it.name)),
        (false, _) => has(line, &it.name),
    }
}

/// Whether a macro-body line names `it`: `Type::name` or `name(` for a function, the
/// bare name for anything else.
fn macro_names(line: &str, it: &Item) -> bool {
    match (it.kind == Kind::Fn, &it.owner) {
        (true, Some(t)) => {
            has(line, &format!("{t}::{}", it.name)) || has(line, &format!("{}(", it.name))
        }
        (true, None) => has(line, &format!("{}(", it.name)),
        (false, _) => has(line, &it.name),
    }
}

/// Whether `needle` occurs in `line` at word boundaries.
fn has(line: &str, needle: &str) -> bool {
    let b = line.as_bytes();
    line.match_indices(needle).any(|(i, _)| {
        let first = needle.as_bytes()[0];
        let last = needle.as_bytes()[needle.len() - 1];
        (!word(first) || i == 0 || !word(b[i - 1]))
            && (!word(last) || b.get(i + needle.len()).is_none_or(|&c| !word(c)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, owner: Option<&str>, kind: Kind) -> Item {
        Item {
            krate: "c".into(),
            file: "f".into(),
            line: 1,
            kind,
            owner: owner.map(str::to_owned),
            name: name.into(),
            in_macro: false,
            refs: Vec::new(),
        }
    }

    #[test]
    fn a_name_in_a_doc_comment_code_block_is_a_hit_and_prose_is_not() {
        let src = "/// Reads the rate: sample_rate() is prose.\n///\n/// ```\n/// let r = s.sample_rate();\n/// ```\npub fn f() {}\n";
        let c = Corpus::from_texts(&[("crates/c/src/lib.rs", src)]);
        let m = item("sample_rate", Some("OutputStream"), Kind::Fn);
        assert_eq!(
            c.doc_example(&m),
            Some(("crates/c/src/lib.rs".to_owned(), 4))
        );
        let prose = Corpus::from_texts(&[(
            "crates/c/src/lib.rs",
            &src.replace("let r = s.sample_rate();", "text"),
        )]);
        assert_eq!(prose.doc_example(&m), None);
    }

    #[test]
    fn a_markdown_fence_counts_and_a_non_rust_fence_does_not() {
        let md = "```rust\nlet w = Wave::new();\n```\n```toml\nWave = 1\n```\n";
        let c = Corpus::from_texts(&[("book/src/a.md", md)]);
        assert_eq!(
            c.doc_example(&item("Wave", None, Kind::Struct))
                .map(|h| h.1),
            Some(2)
        );
        assert_eq!(c.doc_example(&item("Gone", None, Kind::Struct)), None);
    }

    #[test]
    fn a_macro_body_that_calls_a_method_by_path_labels_it() {
        let src = "macro_rules! m {\n    () => { $crate::Sweep::new().with(1) };\n}\nfn g() { other::new(); }\n";
        let c = Corpus::from_texts(&[("crates/c/src/lib.rs", src)]);
        assert!(c.in_macro(&item("new", Some("Sweep"), Kind::Fn)));
        assert!(!c.in_macro(&item("gone", Some("Sweep"), Kind::Fn)));
    }
}
