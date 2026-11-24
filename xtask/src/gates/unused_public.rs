//! `unused-public`: the public items no production code refers to. A report, not
//! a gate: it needs `rust-analyzer` and `cargo-public-api`, and it exits 0 with
//! whatever it finds.
//!
//! A row is a `pub` function, method, struct, enum, trait, type alias, const or
//! static of a named crate that
//!
//! - the umbrella `tutti` does not name: the item (or, for a method, its type)
//!   appears by name in `tutti`'s `cargo public-api` text. The match is by name,
//!   not by resolved path, and that text lists no method, so every method of an
//!   exported type counts as exported;
//! - no reference outside test code reaches: the references are the
//!   occurrences in rust-analyzer's SCIP index (`scip.rs`), minus the definition,
//!   minus an `impl` header's own mention of its type. A reference is test code
//!   when its file is under `tests`, `benches`, `examples` or `testing`, is
//!   `tests.rs`, `testing.rs` or `*_tests.rs`, or lies in a `#[cfg(test)]` item.
//!
//! Items used by being implemented are left out: what a `trait` block declares,
//! what an `impl <Trait> for <Type>` block holds, and a public trait that some
//! `impl .. <Trait> for ..` outside test code (anywhere under `crates/`, matched
//! by the trait's name) implements. A definition inside a macro invocation or a
//! `macro_rules!` body is kept and labelled `in macro`.
//!
//! What the report cannot see: an item reached only through a trait object or a
//! macro expansion may show as unreferenced; trait methods are not listed; a
//! `pub` item the umbrella re-exports is filtered by name, not by path; code
//! behind a `cfg` of another platform is not indexed; visibility is read from the
//! declaration's text (`pub ` and no restriction).

use super::lex::{blank, find_word_from, ident_at, is_test_file, strip_attrs, test_ranges};
use super::scip::{self, Document};
use super::{Tree, lex};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

/// The shape of a listed item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Fn,
    Struct,
    Enum,
    Trait,
    Type,
    Const,
    Static,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Fn => "fn",
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Trait => "trait",
            Kind::Type => "type",
            Kind::Const => "const",
            Kind::Static => "static",
        }
    }
}

/// A public item and where it is referred to.
#[derive(Debug, Clone)]
pub(super) struct Item {
    pub(super) krate: String,
    pub(super) file: String,
    /// 1-based.
    pub(super) line: usize,
    pub(super) kind: Kind,
    /// The type of the `impl` block a method sits in.
    pub(super) owner: Option<String>,
    pub(super) name: String,
    pub(super) in_macro: bool,
    /// `(file, 1-based line)` of each reference.
    pub(super) refs: Vec<(String, usize)>,
}

impl Item {
    fn path(&self) -> String {
        match &self.owner {
            Some(t) => format!("{t}::{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// How the references of a listed item read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refs {
    None,
    TestOnly(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    pub(super) krate: String,
    pub(super) file: String,
    pub(super) line: usize,
    pub(super) kind: Kind,
    pub(super) path: String,
    pub(super) refs: Refs,
    pub(super) in_macro: bool,
}

/// What each rule removed, per crate.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Removed {
    /// Declared in a `trait` block.
    pub(super) trait_items: usize,
    /// Held by an `impl <Trait> for <Type>` block.
    pub(super) trait_impl_items: usize,
    /// A trait some non-test `impl` implements.
    pub(super) implemented_traits: usize,
    /// Named by the umbrella.
    pub(super) umbrella: usize,
    /// Kept, labelled `in macro`.
    pub(super) in_macro: usize,
}

/// The items that have no production reference, as rows, and the umbrella and
/// implemented-trait rules' counts. `umbrella` is the names `tutti` exports, or
/// `None` when that was not checked.
pub(super) fn classify(
    items: &[Item],
    is_test: &dyn Fn(&str, usize) -> bool,
    umbrella: Option<&Reach>,
    implemented: &BTreeSet<String>,
) -> (Vec<Row>, Removed) {
    let mut rows = Vec::new();
    let mut removed = Removed::default();
    for it in items {
        if umbrella.is_some_and(|u| u.names(&it.krate, it.owner.as_deref().unwrap_or(&it.name))) {
            removed.umbrella += 1;
            continue;
        }
        if it.kind == Kind::Trait && implemented.contains(&it.name) {
            removed.implemented_traits += 1;
            continue;
        }
        let test = it.refs.iter().filter(|(f, l)| is_test(f, *l)).count();
        if test < it.refs.len() {
            continue;
        }
        if it.in_macro {
            removed.in_macro += 1;
        }
        rows.push(Row {
            krate: it.krate.clone(),
            file: it.file.clone(),
            line: it.line,
            kind: it.kind,
            path: it.path(),
            refs: if test == 0 {
                Refs::None
            } else {
                Refs::TestOnly(test)
            },
            in_macro: it.in_macro,
        });
    }
    rows.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    (rows, removed)
}

// ---- reading a declaration from source ----

/// What a definition's enclosing block makes of it.
#[derive(Debug, PartialEq, Eq)]
enum Context {
    Module,
    Inherent(String),
    TraitBody,
    TraitImpl,
    Other,
}

/// The `{`, `(` or `[` blocks around `at`, innermost first, each with the text
/// before it back to the previous `;`, `{` or `}`.
fn enclosing(code: &str, at: usize) -> Vec<(u8, &str)> {
    let b = code.as_bytes();
    let (mut depth, mut out, mut i) = (0usize, Vec::new(), at);
    while i > 0 {
        i -= 1;
        match b[i] {
            b'}' | b')' | b']' => depth += 1,
            b'{' | b'(' | b'[' if depth > 0 => depth -= 1,
            open @ (b'{' | b'(' | b'[') => {
                let start = code[..i].rfind([';', '{', '}']).map_or(0, |p| p + 1);
                out.push((open, code[start..i].trim()));
            }
            _ => {}
        }
    }
    out
}

/// Whether `header` ends a macro invocation (`name!`, `name! ident`) or opens a
/// `macro_rules!` body.
fn is_macro(header: &str) -> bool {
    let b = header.as_bytes();
    b.windows(2)
        .enumerate()
        .any(|(i, w)| lex::word(w[0]) && w[1] == b'!' && b.get(i + 2) != Some(&b'='))
}

fn strip_head(mut s: &str) -> &str {
    loop {
        s = strip_attrs(s.trim_start());
        if let Some(r) = s.strip_prefix("pub(") {
            s = r.split_once(')').map_or("", |x| x.1);
        } else if let Some(r) = ["pub ", "unsafe ", "default ", "auto "]
            .iter()
            .find_map(|p| s.strip_prefix(p))
        {
            s = r;
        } else {
            return s.trim_start();
        }
    }
}

fn context(header: &str) -> Context {
    let h = strip_head(header);
    let word = |w: &str| {
        h.strip_prefix(w)
            .is_some_and(|r| !r.bytes().next().is_some_and(lex::word))
    };
    if word("trait") {
        Context::TraitBody
    } else if word("mod") {
        Context::Module
    } else if word("impl") {
        let mut rest = h["impl".len()..].trim_start();
        if rest.starts_with('<') {
            let end = lex::matching(rest, 0, b'<', b'>').map_or(0, |e| e + 1);
            rest = rest[end..].trim_start();
        }
        let mut from = 0;
        while let Some(p) = find_word_from(rest, "for", from) {
            if !rest[p + 3..].trim_start().starts_with('<') {
                return Context::TraitImpl;
            }
            from = p + 3;
        }
        let path: String = rest
            .trim_start_matches(['&', '*'])
            .trim_start_matches("dyn ")
            .chars()
            .take_while(|&c| lex::word_char(c) || c == ':')
            .collect();
        Context::Inherent(path.rsplit("::").next().unwrap_or("_").to_owned())
    } else {
        Context::Other
    }
}

/// The `(is public, kind, name)` a declaration line opens.
fn head(line: &str) -> Option<(bool, Kind, String)> {
    let mut s = strip_attrs(line.trim());
    let mut public = false;
    if let Some(r) = s.strip_prefix("pub ") {
        public = true;
        s = r;
    } else if let Some(r) = s.strip_prefix("pub(") {
        s = r.split_once(')')?.1;
    }
    loop {
        s = s.trim_start();
        if let Some(r) = ["unsafe ", "async ", "default "]
            .iter()
            .find_map(|q| s.strip_prefix(q))
        {
            s = r;
        } else if let Some(r) = s.strip_prefix("extern ") {
            s = r.trim_start();
            if s.starts_with('"') {
                s = s[1..].split_once('"')?.1;
            }
        } else if let Some(r) = s.strip_prefix("const ") {
            let next = r.trim_start();
            if ["fn ", "unsafe ", "async ", "extern "]
                .iter()
                .any(|q| next.starts_with(q))
            {
                s = next;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    let (kind, rest) = [
        ("fn ", Kind::Fn),
        ("struct ", Kind::Struct),
        ("enum ", Kind::Enum),
        ("trait ", Kind::Trait),
        ("type ", Kind::Type),
        ("const ", Kind::Const),
        ("static ", Kind::Static),
    ]
    .iter()
    .find_map(|(w, k)| s.strip_prefix(w).map(|r| (*k, r)))?;
    let rest = rest.strip_prefix("mut ").unwrap_or(rest);
    let (name, _) = ident_at(rest, 0);
    (!name.is_empty()).then(|| (public, kind, name.to_owned()))
}

/// What the definition at `line` (0-based) of `code` (blanked) is.
#[derive(Debug, PartialEq, Eq)]
enum Declared {
    /// Not a public function, struct, enum, trait, type, const or static.
    Not,
    /// A member of a `trait` block.
    TraitItem,
    /// A member of an `impl <Trait> for <Type>` block.
    TraitImplItem,
    Public {
        kind: Kind,
        owner: Option<String>,
        name: String,
        in_macro: bool,
    },
}

fn declared(code: &str, starts: &[usize], line: usize) -> Declared {
    let Some(&at) = starts.get(line) else {
        return Declared::Not;
    };
    let end = starts.get(line + 1).copied().unwrap_or(code.len());
    let text = &code[at..end];
    let Some((public, kind, name)) = head(text) else {
        return Declared::Not;
    };
    let at = at + (text.len() - text.trim_start().len());
    let chain = enclosing(code, at);
    let in_macro = chain.iter().any(|(_, h)| is_macro(h));
    let ctx = chain
        .iter()
        .find(|(o, h)| *o == b'{' && !is_macro(h))
        .map_or(Context::Module, |(_, h)| context(h));
    match ctx {
        Context::TraitBody => Declared::TraitItem,
        Context::TraitImpl => Declared::TraitImplItem,
        Context::Other if !in_macro => Declared::Not,
        _ if !public => Declared::Not,
        ctx => Declared::Public {
            kind,
            // A macro body's own braces are no impl: its items are taken as the module's.
            owner: match ctx {
                Context::Inherent(t) => Some(t),
                _ => None,
            },
            name,
            in_macro,
        },
    }
}

/// The names of the traits an `impl .. Trait for ..` header in `code` (blanked)
/// implements.
fn implemented(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(p) = find_word_from(code, "impl", from) {
        from = p + 4;
        let rest = &code[p + 4..];
        let end = rest.find(['{', ';']).unwrap_or(rest.len());
        let mut header = rest[..end].trim_start();
        if header.starts_with('<') {
            let e = lex::matching(header, 0, b'<', b'>').map_or(0, |e| e + 1);
            header = header[e..].trim_start();
        }
        let mut at = 0;
        while let Some(f) = find_word_from(header, "for", at) {
            at = f + 3;
            if header[at..].trim_start().starts_with('<') {
                continue;
            }
            let before = header[..f].trim_end().trim_start_matches('!');
            // The trait path without its generic arguments.
            let mut depth = 0usize;
            let bare: String = before
                .chars()
                .filter(|&c| {
                    match c {
                        '<' => depth += 1,
                        '>' => depth = depth.saturating_sub(1),
                        _ => return depth == 0,
                    }
                    false
                })
                .collect();
            if let Some(name) = bare.rsplit("::").next().map(str::trim)
                && !name.is_empty()
            {
                out.push(name.to_owned());
            }
            break;
        }
    }
    out
}

// ---- sources ----

/// A file's blanked text and its line starts.
struct Src {
    code: String,
    starts: Vec<usize>,
    tests: Vec<(usize, usize)>,
}

impl Src {
    fn new(text: &str) -> Src {
        let code = blank(text);
        let mut starts = vec![0];
        starts.extend(code.match_indices('\n').map(|(i, _)| i + 1));
        let tests = test_ranges(&code)
            .into_iter()
            .map(|(a, b)| {
                let line = |o: usize| starts.partition_point(|&s| s <= o);
                (line(a), line(b.saturating_sub(1).max(a)))
            })
            .collect();
        Src {
            code,
            starts,
            tests,
        }
    }

    /// The text of 1-based `line`.
    fn line(&self, line: usize) -> &str {
        let at = self.starts.get(line - 1).copied().unwrap_or(0);
        let end = self.starts.get(line).copied().unwrap_or(self.code.len());
        &self.code[at.min(end)..end]
    }
}

/// Whether `path` holds test code by its place alone.
fn test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    is_test_file(path)
        || path
            .split('/')
            .any(|c| matches!(c, "tests" | "benches" | "examples" | "testing"))
        || matches!(name, "tests.rs" | "testing.rs")
        || name.ends_with("_tests.rs")
}

/// The files read so far.
struct Sources<'a> {
    tree: &'a dyn Tree,
    files: BTreeMap<String, Option<Src>>,
}

impl<'a> Sources<'a> {
    fn load(&mut self, path: &str) {
        if !self.files.contains_key(path) {
            let src = self.tree.read(path).map(|t| Src::new(&t));
            self.files.insert(path.to_owned(), src);
        }
    }

    fn get(&self, path: &str) -> Option<&Src> {
        self.files.get(path)?.as_ref()
    }

    fn is_test(&self, path: &str, line: usize) -> bool {
        test_path(path)
            || self
                .get(path)
                .is_some_and(|s| s.tests.iter().any(|&(a, b)| a <= line && line <= b))
    }
}

/// The names `tutti`'s `public-api` text exports: the last path segment of each
/// `pub use` and item line.
pub(super) fn umbrella_names(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("pub ") else {
            continue;
        };
        let rest = rest
            .trim_start_matches("unsafe ")
            .trim_start_matches("const ");
        let rest = match rest.strip_prefix("use ") {
            Some(r) => r,
            None => match rest.split_once(' ') {
                Some((kw, r)) if kw != "mod" => r,
                _ => continue,
            },
        };
        let path = rest.split(['(', '<', ' ']).next().unwrap_or("");
        if let Some(name) = path.rsplit("::").next().filter(|n| !n.is_empty()) {
            out.insert(name.to_owned());
        }
    }
    out
}

/// What the umbrella exports: the `(crate, name)` of each symbol the umbrella's own
/// source refers to (the index resolves a re-export to its defining crate), and the
/// names of `tutti`'s `public-api` text, which also holds what a glob re-exports.
#[derive(Debug, Default)]
pub(super) struct Reach {
    resolved: BTreeSet<(String, String)>,
    resolved_names: BTreeSet<String>,
    by_name: BTreeSet<String>,
}

impl Reach {
    pub(super) fn new(resolved: BTreeSet<(String, String)>, by_name: BTreeSet<String>) -> Reach {
        let resolved_names = resolved.iter().map(|r| r.1.clone()).collect();
        Reach {
            resolved,
            resolved_names,
            by_name,
        }
    }

    /// Whether `name` of `krate` is exported: resolved to that crate, or named in
    /// the text when no resolved export of the name sits in another crate.
    fn names(&self, krate: &str, name: &str) -> bool {
        self.resolved.contains(&(krate.to_owned(), name.to_owned()))
            || (self.by_name.contains(name) && !self.resolved_names.contains(name))
    }
}

/// The `(crate, name)` a SCIP symbol names: `rust-analyzer cargo tutti-graph 0.0.1
/// exec/Executor#`: the package, and the last type or item of its path.
fn symbol_name(symbol: &str) -> Option<(String, String)> {
    let mut parts = symbol.splitn(5, ' ');
    let krate = parts.nth(2)?;
    let last = parts.nth(1)?.rsplit('/').next()?;
    let name = last.split('#').next()?.trim_end_matches(['.', '(', ')']);
    (!name.is_empty()).then(|| (krate.to_owned(), name.to_owned()))
}

/// The crate a `crates/<crate>/..` path belongs to.
fn crate_of(path: &str) -> Option<&str> {
    path.strip_prefix("crates/")?.split('/').next()
}

/// The report over `index` (a serialized SCIP index) for `crates`, read against
/// `tree`. `umbrella` is `tutti`'s `public-api` text, or `None`.
pub(crate) fn report(
    tree: &dyn Tree,
    index: &[u8],
    crates: &[String],
    umbrella: Option<&str>,
) -> Result<String, String> {
    let by_name = umbrella.map(umbrella_names);
    let mut resolved = BTreeSet::new();
    // What `lib.rs` names, whatever features the index was built with.
    if let Some(lib) = tree.read("crates/tutti/src/lib.rs") {
        resolved.extend(
            super::umbrella_api::export_names(&lib)
                .into_iter()
                .map(|(k, n)| (k.replace('_', "-"), n)),
        );
    }
    let mut sources = Sources {
        tree,
        files: BTreeMap::new(),
    };
    // Pass 1: the definitions in the named crates' production files.
    let mut items: HashMap<String, Item> = HashMap::new();
    let mut counts: BTreeMap<String, Removed> = BTreeMap::new();
    for doc in scip::documents(index)? {
        let doc: Document = doc?;
        let Some(krate) = crate_of(&doc.path).filter(|k| crates.iter().any(|c| c == k)) else {
            continue;
        };
        if test_path(&doc.path) || !doc.path.ends_with(".rs") {
            continue;
        }
        sources.load(&doc.path);
        let Some(src) = sources.get(&doc.path) else {
            continue;
        };
        for o in doc.occurrences.iter().filter(|o| o.is_definition()) {
            if o.symbol.starts_with("local ") || items.contains_key(&o.symbol) {
                continue;
            }
            let line = o.line as usize;
            if src.tests.iter().any(|&(a, b)| a <= line + 1 && line < b) {
                continue;
            }
            match declared(&src.code, &src.starts, line) {
                Declared::Not => {}
                Declared::TraitItem => counts.entry(krate.to_owned()).or_default().trait_items += 1,
                Declared::TraitImplItem => {
                    counts.entry(krate.to_owned()).or_default().trait_impl_items += 1;
                }
                Declared::Public {
                    kind,
                    owner,
                    name,
                    in_macro,
                } => {
                    items.insert(
                        o.symbol.clone(),
                        Item {
                            krate: krate.to_owned(),
                            file: doc.path.clone(),
                            line: line + 1,
                            kind,
                            owner,
                            name,
                            in_macro,
                            refs: Vec::new(),
                        },
                    );
                }
            }
        }
    }
    // Pass 2: the references to them, anywhere.
    for doc in scip::documents(index)? {
        let doc = doc?;
        if doc.path.starts_with("crates/tutti/src/") {
            resolved.extend(
                doc.occurrences
                    .iter()
                    .filter(|o| !o.is_definition() && !o.symbol.starts_with("local "))
                    .filter_map(|o| symbol_name(&o.symbol)),
            );
        }
        for o in doc.occurrences.iter().filter(|o| !o.is_definition()) {
            if let Some(it) = items.get_mut(&o.symbol) {
                it.refs.push((doc.path.clone(), o.line as usize + 1));
            }
        }
    }
    let names = by_name.map(|b| Reach::new(resolved, b));
    // A mention in an `impl` header is not a use of the type.
    let paths: BTreeSet<String> = items
        .values()
        .flat_map(|i| i.refs.iter().map(|r| r.0.clone()))
        .filter(|p| !test_path(p))
        .collect();
    for p in &paths {
        sources.load(p);
    }
    for it in items.values_mut() {
        it.refs.retain(|(f, l)| {
            sources.get(f).is_none_or(|s| {
                let t = s.line(*l).trim_start();
                !(t.starts_with("impl") || t.starts_with("unsafe impl"))
            })
        });
    }
    // The traits implemented outside test code, by name.
    let mut implemented_by: BTreeSet<String> = BTreeSet::new();
    for path in tree.files("crates", &lex::skipped) {
        if path.ends_with(".rs") && !test_path(&path) {
            sources.load(&path);
            if let Some(s) = sources.get(&path) {
                let production = strip_ranges(&s.code, &test_ranges(&s.code));
                implemented_by.extend(implemented(&production));
            }
        }
    }
    let mut out = String::new();
    out.push_str(HEADER);
    if names.is_none() {
        out.push_str("umbrella reachability not checked (cargo public-api did not run)\n");
    }
    out.push('\n');
    let is_test = |f: &str, l: usize| sources.is_test(f, l);
    let (mut total_rows, mut total_items) = (0, 0);
    for krate in crates {
        let mine: Vec<Item> = items
            .values()
            .filter(|i| &i.krate == krate)
            .cloned()
            .collect();
        let (rows, removed) = classify(&mine, &is_test, names.as_ref(), &implemented_by);
        let mut removed = Removed {
            trait_items: counts.get(krate).map_or(0, |c| c.trait_items),
            trait_impl_items: counts.get(krate).map_or(0, |c| c.trait_impl_items),
            ..removed
        };
        removed.in_macro = rows.iter().filter(|r| r.in_macro).count();
        let _ = writeln!(out, "{krate}: {} items", rows.len());
        for r in &rows {
            let refs = match r.refs {
                Refs::None => "none".to_owned(),
                Refs::TestOnly(k) => format!("test-only ({k})"),
            };
            let note = if r.in_macro { "  in macro" } else { "" };
            let unchecked = if names.is_none() {
                "  umbrella reachability not checked"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "{}:{}  {}  {}  refs: {refs}{note}{unchecked}",
                r.file,
                r.line,
                r.kind.word(),
                r.path
            );
        }
        let none = rows.iter().filter(|r| r.refs == Refs::None).count();
        let _ = writeln!(
            out,
            "{krate}: {} listed ({none} with no reference, {} test-only); left out: {} trait-block, {} trait-impl, {} implemented-trait, {} umbrella-named; labelled in macro: {}\n",
            rows.len(),
            rows.len() - none,
            removed.trait_items,
            removed.trait_impl_items,
            removed.implemented_traits,
            removed.umbrella,
            removed.in_macro
        );
        total_rows += rows.len();
        total_items += 1;
    }
    let _ = writeln!(out, "total: {total_rows} items in {total_items} crates");
    Ok(out)
}

/// `code` with the byte `ranges` blanked to spaces (newlines kept).
fn strip_ranges(code: &str, ranges: &[(usize, usize)]) -> String {
    let mut b = code.as_bytes().to_vec();
    for &(a, e) in ranges {
        for c in &mut b[a..e.min(code.len())] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    }
    String::from_utf8(b).unwrap_or_default()
}

const HEADER: &str = "unused-public: public items with no reference outside test code\n\
A report, not proof. It lists a pub fn, method, struct, enum, trait, type, const or static that\n\
the umbrella `tutti` does not name (a symbol tutti's own source refers to is resolved by the index\n\
to its defining crate; a name only in tutti's public-api text, as a glob re-exports it, is matched by\n\
name; neither lists a method, so every method of an exported type counts as exported) and no code outside\n\
tests refers to (rust-analyzer's SCIP index; an `impl` header's mention of its type is not a use).\n\
Left out: items declared in a `trait` block, items of an `impl <Trait> for <Type>` block, and a\n\
public trait that some non-test `impl .. <Trait> for ..` under crates/ implements (by name).\n\
A definition inside a macro invocation is kept and labelled `in macro`.\n\
It cannot see: use only through a trait object or a macro expansion (may show as unreferenced);\n\
trait methods (not listed); a re-exported item used by an app (filtered by name); code behind a\n\
cfg of another platform (macOS and Windows code is not indexed on Linux).\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, owner: Option<&str>, refs: &[(&str, usize)]) -> Item {
        Item {
            krate: "c".to_owned(),
            file: "crates/c/src/lib.rs".to_owned(),
            line: 1,
            kind: Kind::Fn,
            owner: owner.map(str::to_owned),
            name: name.to_owned(),
            in_macro: false,
            refs: refs.iter().map(|(f, l)| ((*f).to_owned(), *l)).collect(),
        }
    }

    fn run(items: &[Item], umbrella: &[&str]) -> Vec<Row> {
        let u = Reach::new(
            BTreeSet::new(),
            umbrella.iter().map(|s| (*s).to_owned()).collect(),
        );
        let t = |f: &str, _: usize| f.contains("/tests/");
        classify(items, &t, Some(&u), &BTreeSet::new()).0
    }

    #[test]
    fn a_production_reference_is_not_listed() {
        assert!(run(&[item("f", None, &[("crates/c/src/a.rs", 3)])], &[]).is_empty());
    }

    #[test]
    fn test_only_references_are_counted() {
        let rows = run(
            &[item(
                "f",
                None,
                &[("crates/c/tests/a.rs", 3), ("crates/c/tests/b.rs", 1)],
            )],
            &[],
        );
        assert_eq!(rows[0].refs, Refs::TestOnly(2));
    }

    #[test]
    fn no_reference_is_listed_as_none() {
        assert_eq!(run(&[item("f", None, &[])], &[])[0].refs, Refs::None);
    }

    #[test]
    fn an_item_or_a_method_of_a_type_the_umbrella_names_is_not_listed() {
        let items = [item("f", None, &[]), item("m", Some("T"), &[])];
        assert!(run(&items, &["f", "T"]).is_empty());
        assert_eq!(run(&items, &["m"]).len(), 2);
    }

    #[test]
    fn a_resolved_export_hides_only_its_own_crate_and_a_name_hides_the_unresolved() {
        let r = Reach::new(
            BTreeSet::from([("a".to_owned(), "Editor".to_owned())]),
            BTreeSet::from(["Editor".to_owned(), "Glob".to_owned()]),
        );
        assert!(r.names("a", "Editor") && !r.names("b", "Editor"));
        assert!(r.names("b", "Glob"));
        assert_eq!(
            symbol_name("rust-analyzer cargo tutti-graph 0.0.1 exec/Executor#"),
            Some(("tutti-graph".to_owned(), "Executor".to_owned()))
        );
    }

    #[test]
    fn an_implemented_trait_is_not_listed() {
        let mut t = item("Tr", None, &[]);
        t.kind = Kind::Trait;
        let implemented = BTreeSet::from(["Tr".to_owned()]);
        let (rows, removed) = classify(&[t], &|_, _| false, None, &implemented);
        assert!(rows.is_empty());
        assert_eq!(removed.implemented_traits, 1);
    }

    fn decl(src: &str, line: usize) -> Declared {
        let code = blank(src);
        let src = Src::new(src);
        declared(&code, &src.starts, line)
    }

    #[test]
    fn only_an_unrestricted_pub_item_is_declared_public() {
        let src = "pub fn a() {}\npub(crate) fn b() {}\nfn c() {}\npub struct S { pub x: u8 }\n";
        assert!(matches!(
            decl(src, 0),
            Declared::Public { kind: Kind::Fn, .. }
        ));
        assert_eq!(decl(src, 1), Declared::Not);
        assert_eq!(decl(src, 2), Declared::Not);
        assert!(matches!(
            decl(src, 3),
            Declared::Public {
                kind: Kind::Struct,
                ..
            }
        ));
    }

    #[test]
    fn methods_know_their_type_and_trait_members_are_set_apart() {
        let src = "impl<T> Foo<T> {\n    pub const fn m() {}\n}\ntrait Tr {\n    fn t();\n}\n\
                   impl Tr for Foo<u8> {\n    fn t() {}\n}\n";
        assert_eq!(
            decl(src, 1),
            Declared::Public {
                kind: Kind::Fn,
                owner: Some("Foo".to_owned()),
                name: "m".to_owned(),
                in_macro: false
            }
        );
        assert_eq!(decl(src, 4), Declared::TraitItem);
        assert_eq!(decl(src, 7), Declared::TraitImplItem);
    }

    #[test]
    fn an_item_in_a_macro_invocation_is_labelled() {
        let src = "declare! {\n    pub fn a() {}\n}\nmacro_rules! m {\n    () => {\n        pub fn b() {}\n    };\n}\n";
        assert!(matches!(
            decl(src, 1),
            Declared::Public { in_macro: true, .. }
        ));
        assert!(matches!(
            decl(src, 5),
            Declared::Public { in_macro: true, .. }
        ));
    }

    #[test]
    fn an_item_inside_a_function_body_is_not_declared() {
        assert_eq!(decl("fn f() {\n    pub struct L;\n}\n", 1), Declared::Not);
    }

    #[test]
    fn implemented_traits_are_read_from_impl_headers() {
        let code = blank(
            "impl<T> a::Tr<T> for X {}\nimpl Y {}\nunsafe impl Send for Z {}\nimpl<F> Q for F where F: for<'a> W<'a> {}\n",
        );
        assert_eq!(implemented(&code), ["Tr", "Send", "Q"]);
    }

    #[test]
    fn test_code_is_a_path_or_a_cfg_test_range() {
        for p in [
            "crates/a/tests/x.rs",
            "crates/a/benches/x.rs",
            "crates/a/examples/x.rs",
            "testing/t/src/lib.rs",
            "crates/a/src/tests.rs",
            "crates/a/src/foo_tests.rs",
            "crates/a/src/testing.rs",
        ] {
            assert!(test_path(p), "{p}");
        }
        assert!(!test_path("crates/a/src/lib.rs"));
        let src = Src::new("fn p() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn q() {}\n");
        assert_eq!(src.tests, [(2, 5)]);
    }

    #[test]
    fn the_umbrella_names_are_the_last_segments() {
        let n = umbrella_names(
            "pub mod tutti::graph\npub use tutti::session::Graph\npub fn tutti::free(x: u8)\n\
             pub struct tutti::S<T>\n",
        );
        assert_eq!(
            n,
            BTreeSet::from(["Graph".into(), "free".into(), "S".into()])
        );
    }
}
