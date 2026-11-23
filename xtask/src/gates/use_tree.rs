//! The lexical reading of `pub use` items that the umbrella gates share.
//!
//! A file is lexed (comments and literals blanked, so prose is not code) and each
//! `pub use .. ;` is read as a use tree into its leaves: the item a leaf names (the
//! path it comes from and its own name), the name it exports (the alias when there
//! is an `as`) and whether it re-exports a crate or module whole. `pub(crate) use`
//! is not a re-export.
//!
//! One walker serves every reader: `check-session` reads the whole-crate leaves of
//! each statement ([`statements`]) and `umbrella-api` the exported items ([`uses`]).

use super::lex::{blank, find_word_from, line_of, matching, word};

/// `(::|\w+|[{},*])` tokens of a `use` body.
fn tokens(body: &str) -> Vec<&str> {
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b':' && b.get(i + 1) == Some(&b':') {
            out.push(&body[i..i + 2]);
            i += 2;
        } else if word(b[i]) {
            let s = i;
            while i < b.len() && word(b[i]) {
                i += 1;
            }
            out.push(&body[s..i]);
        } else if matches!(b[i], b'{' | b'}' | b',' | b'*') {
            out.push(&body[i..=i]);
            i += 1;
        } else {
            i += 1;
        }
    }
    out
}

/// `pub use` statements of lexed `code`, as `(start, end of the ';')`.
fn pub_uses(code: &str) -> Vec<(usize, usize)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    for (at, _) in code.match_indices("pub") {
        if at > 0 && (word(b[at - 1]) || b[at - 1] == b')') {
            continue;
        }
        let rest = &code[at + 3..];
        let after = rest.trim_start();
        if after.len() == rest.len() {
            continue;
        }
        let Some(kw) = after.strip_prefix("use") else {
            continue;
        };
        if kw.bytes().next().is_some_and(word) {
            continue;
        }
        let body = code.len() - kw.len();
        if let Some(semi) = code[body..].find(';') {
            out.push((at, body + semi));
        }
    }
    out
}

/// One leaf of a `pub use` tree.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Leaf {
    /// The name the leaf exports; `*` for a glob.
    pub(super) name: String,
    /// The path the item comes from, crate first, and its own name before an `as`.
    pub(super) path: Vec<String>,
    pub(super) source: String,
    /// The leaf is a glob, a bare crate or `self`: a crate or module whole.
    pub(super) whole: bool,
    /// The leaf is `self` of a group (`m::{self}`): `source` is then the group's
    /// own name and `path` what precedes it.
    pub(super) is_self: bool,
}

impl Leaf {
    /// The crate the item comes from; empty for a bare name.
    pub(super) fn krate(&self) -> &str {
        self.path.first().map_or("", String::as_str)
    }

    /// The item's full path, crate, modules and name.
    pub(super) fn item(&self) -> String {
        let mut p = self.path.clone();
        p.push(self.source.clone());
        p.join("::")
    }
}

/// The leaves of the use tree in `t` at `*k` under the prefix `pre`.
fn leaves(t: &[&str], k: &mut usize, pre: &[String], out: &mut Vec<Leaf>) {
    let mut path = pre.to_vec();
    if t.get(*k) == Some(&"::") {
        *k += 1;
    }
    while *k < t.len() {
        let tok = t[*k];
        if tok == "{" {
            *k += 1;
            while *k < t.len() && t[*k] != "}" {
                leaves(t, k, &path, out);
                if t.get(*k) == Some(&",") {
                    *k += 1;
                }
            }
            *k += 1;
            return;
        }
        let glob = tok == "*";
        if !glob && !tok.bytes().all(word) {
            *k += 1;
            return;
        }
        *k += 1;
        if !glob && t.get(*k) == Some(&"::") {
            path.push(tok.to_owned());
            *k += 1;
            continue;
        }
        let alias = if t.get(*k) == Some(&"as") {
            *k += 2;
            t.get(*k - 1).copied()
        } else {
            None
        };
        let self_leaf = tok == "self";
        let (source, path) = match (self_leaf, path.split_last()) {
            (true, Some((last, rest))) => (last.clone(), rest.to_vec()),
            _ => (tok.to_owned(), path),
        };
        let tutti = path.first().is_some_and(|p| p.starts_with("tutti_"));
        out.push(Leaf {
            name: alias.unwrap_or(&source).to_owned(),
            whole: glob
                || (self_leaf && tutti)
                || (path.is_empty() && source.starts_with("tutti_")),
            path,
            source,
            is_self: self_leaf,
        });
        return;
    }
}

/// A `pub use` statement of lexed code: where it sits and its leaves.
pub(super) struct Statement {
    /// Offset of the `pub`.
    pub(super) at: usize,
    pub(super) leaves: Vec<Leaf>,
}

/// The `pub use` statements of `code`, which [`blank`] has lexed.
pub(super) fn statements(code: &str) -> Vec<Statement> {
    pub_uses(code)
        .into_iter()
        .map(|(at, semi)| {
            let body = code[at + 3..semi].trim_start();
            let t = tokens(&body["use".len()..]);
            let mut leaves_ = Vec::new();
            leaves(&t, &mut 0, &[], &mut leaves_);
            Statement {
                at,
                leaves: leaves_,
            }
        })
        .collect()
}

/// Every `use` statement of lexed `code` (public or not), as `(offset of the `use`, its
/// leaves)`.
pub(super) fn item_uses(code: &str) -> Vec<(usize, Vec<Leaf>)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "use", from) {
        from = at + 3;
        let before = code[..at].trim_end();
        let starts_item = match before.as_bytes().last() {
            None => true,
            Some(b';' | b'}' | b'{' | b']' | b')') => true,
            Some(_) => before.ends_with("pub"),
        };
        if !starts_item || b.get(from).is_some_and(|c| word(*c)) {
            continue;
        }
        let Some(semi) = code[from..].find(';') else {
            continue;
        };
        let mut found = Vec::new();
        leaves(&tokens(&code[from..from + semi]), &mut 0, &[], &mut found);
        out.push((at, found));
    }
    out
}

/// The `pub use` leaves of the source `src`.
pub(super) fn uses(src: &str) -> Vec<Leaf> {
    statements(&blank(src))
        .into_iter()
        .flat_map(|s| s.leaves)
        .collect()
}

/// A `pub use` leaf and where it stands.
pub(super) struct Use {
    pub(super) leaf: Leaf,
    /// The inline `mod` blocks around it, outermost first.
    pub(super) module: Vec<String>,
    /// Its line, 1-based.
    pub(super) line: usize,
}

/// The `pub use` leaves of the source `src` with their inline modules and lines.
pub(super) fn placed_uses(src: &str) -> Vec<Use> {
    let code = blank(src);
    let mods = inline_mods(&code);
    let mut out = Vec::new();
    for s in statements(&code) {
        let module: Vec<String> = mods
            .iter()
            .filter(|(_, open, close)| *open < s.at && s.at < *close)
            .map(|(n, ..)| n.clone())
            .collect();
        let line = line_of(&code, s.at);
        out.extend(s.leaves.into_iter().map(|leaf| Use {
            leaf,
            module: module.clone(),
            line,
        }));
    }
    out
}

/// The inline `mod NAME { .. }` blocks of lexed `code`, as `(name, open, close)`.
pub(super) fn inline_mods(code: &str) -> Vec<(String, usize, usize)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "mod", from) {
        from = at + 3;
        let mut i = from;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let s = i;
        while i < b.len() && word(b[i]) {
            i += 1;
        }
        let name = &code[s..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if !name.is_empty()
            && b.get(i) == Some(&b'{')
            && let Some(close) = matching(code, i, b'{', b'}')
        {
            out.push((name.to_owned(), i, close));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(src: &str) -> Vec<(Vec<String>, String, String, bool)> {
        uses(src)
            .into_iter()
            .map(|l| (l.path, l.source, l.name, l.whole))
            .collect()
    }

    fn leaf(
        path: &[&str],
        source: &str,
        name: &str,
        whole: bool,
    ) -> (Vec<String>, String, String, bool) {
        (
            path.iter().map(|p| (*p).to_owned()).collect(),
            source.to_owned(),
            name.to_owned(),
            whole,
        )
    }

    #[test]
    fn a_tree_is_read_into_the_item_each_leaf_names_and_the_name_it_exports() {
        let got =
            items("pub use tutti_x::{A, B as C, m::{D, self as E}};\npub(crate) use tutti_y::Z;\n");
        assert_eq!(
            got,
            [
                leaf(&["tutti_x"], "A", "A", false),
                leaf(&["tutti_x"], "B", "C", false),
                leaf(&["tutti_x", "m"], "D", "D", false),
                leaf(&["tutti_x"], "m", "E", true),
            ]
        );
    }

    #[test]
    fn a_crate_a_self_and_a_glob_are_whole() {
        for src in [
            "pub use tutti_x;",
            "pub use ::tutti_x as y;",
            "pub use tutti_x::{self};",
            "pub use tutti_x::m::*;",
            "pub use local::*;",
        ] {
            assert!(items(src).iter().all(|i| i.3), "{src}");
        }
        assert!(!items("pub use tutti_x::m;")[0].3);
    }

    #[test]
    fn a_statement_knows_where_it_sits() {
        let code = blank("// c\npub use tutti_x::{A, B};\n");
        let s = statements(&code);
        assert_eq!(s.len(), 1);
        assert!(code[s[0].at..].starts_with("pub use tutti_x::{A, B};"));
        assert_eq!(s[0].leaves.len(), 2);
    }

    #[test]
    fn comments_and_strings_are_not_code() {
        assert!(uses("// pub use a::A;\nconst S: &str = \"pub use b::B;\";\n").is_empty());
    }

    #[test]
    fn an_inline_module_and_the_line_are_kept() {
        let u = placed_uses(
            "pub use a::A;\npub mod m {\n    pub mod n {\n        pub use b::B;\n    }\n}\n",
        );
        assert_eq!((u[0].module.clone(), u[0].line), (vec![], 1));
        assert_eq!(
            (u[1].module.clone(), u[1].line),
            (vec!["m".to_owned(), "n".to_owned()], 4)
        );
    }
}
