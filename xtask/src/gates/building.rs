//! `check-building`: a configured thing is built one way, and what is not yet is a
//! row of [`LIST`].
//!
//! A node, a source, a spec, a request or an option is written as
//! `Type::new(required..)` and then chained options (`self`, a bare name, `-> Self`);
//! a running thing changes through its knob. The gate holds that over every shipped
//! crate and counts what is not yet so: a violation without a row of [`LIST`] fails,
//! and so does a row without a violation, so the list only shrinks.
//!
//! **Where it reads.** Every `crates/*/src/**/*.rs` (the `*-ffi` crates too, which
//! [`ALLOW`] excludes), comments and literals blanked, except `tests` and `benches`
//! and `examples` directories, `tests.rs` files and `#[cfg(test)]` items. `testing/`,
//! `xtask/` and `vendor/` are not read.
//!
//! **What it sees.** A `pub fn` (a `pub(..)` item is not public) in an inherent `impl`
//! (a trait impl's methods carry no `pub`), outside the body of a `macro_rules!`. Its
//! *receiver* is by value (`self`, `mut self`), by reference (`&self`, `&mut self`,
//! `&'a self`) or absent. Its *owner* is the last path segment of the `impl`'s type.
//! Its *built type* is the return type with one `Result<..>` or `Option<..>` wrapper
//! peeled, generics dropped, when that is `Self` or the owner's name; a return type
//! that is anything else (`Param<U>`, a tuple, a trait object) has none. A *building
//! owner* is an owner named `*Node`, `*Source`, `*Spec`, `*Config` or `*Options`.
//!
//! **The rules** (a row's rule names one):
//!
//! - `with-prefix`: a receiver by value and a name that starts with `with_`.
//! - `chain-result`: a receiver by value and a return type `Result<Self, ..>` or
//!   `Result<Owner, ..>`. A terminal `build(self) -> Result<Product>` is not one: its
//!   built type is not its owner.
//! - `setter`: a receiver by reference and a name that starts with `set_`, on a
//!   building owner.
//! - `node-ctor`: no receiver, a built type, on a `*Node` owner (a `Result` or
//!   `Option` wrapper counts).
//! - `config-width`: no receiver, return type `Self` or the owner (no wrapper), on a
//!   `*Config` owner, whose first parameter is not a `ChannelLayout` or `BusTopology`
//!   (the last path segment of a type, or of an `impl Into<_>` or a `T: Into<_>` bound
//!   named in the generics or the `where` clause). A function with no parameter has no
//!   first one.
//! - `ctor-result`: no receiver, a built type wrapped in `Result`, on a building owner.
//! - `range-macro`: a `macro_rules!` outside `tutti-types` whose body holds a
//!   `fn new` of one parameter returning `Option<..>`, the `new(raw) -> Option`
//!   signature of a range-checked type.
//!
//! **Blind spots.** The scan is lexical: it does not see an item a macro writes (the
//! `with-prefix` of a macro-made method), an owner reached through a type alias, a
//! re-export, whether a `pub` type is reachable from the crate root, or what a type
//! is beyond its name's suffix. A `set_*` or a constructor on an owner with another
//! suffix is outside the rules.
//!
//! **Keys.** `<file>::<Owner>::<fn>`, or `<file>::<macro>` for `range-macro`. No line
//! numbers; items that share a key and a rule (an `impl Foo<f32>` and an
//! `impl Foo<f64>`) share one row.
//!
//! **Lists.** [`LIST`] holds `<crate>  <rule>  <key>` lines (`#` lines and blank lines
//! aside), today's violations by crate; its crate must be the key's. [`ALLOW`] holds
//! the builders of another kind, each with its reason; those are permanent, a
//! violation they cover is not counted, and a row that covers none stays.

use super::buffers::{blocks, ident_at, item_end, split_top, squash, strip_attrs};
use super::lex::{blank, find_word_from, matching, test_ranges};
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// The baseline: today's violations, `<crate>  <rule>  <key>` per line.
pub(crate) const LIST: &str = "xtask/building-baseline.txt";

/// The fewest violations a real run finds; fewer means the walk is broken.
pub(super) const MIN_SITES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Rule {
    WithPrefix,
    ChainResult,
    Setter,
    NodeCtor,
    ConfigWidth,
    CtorResult,
    RangeMacro,
}

const RULES: [(Rule, &str); 7] = [
    (Rule::WithPrefix, "with-prefix"),
    (Rule::ChainResult, "chain-result"),
    (Rule::Setter, "setter"),
    (Rule::NodeCtor, "node-ctor"),
    (Rule::ConfigWidth, "config-width"),
    (Rule::CtorResult, "ctor-result"),
    (Rule::RangeMacro, "range-macro"),
];

impl Rule {
    fn name(self) -> &'static str {
        RULES.iter().find(|(r, _)| *r == self).map_or("", |r| r.1)
    }

    fn parse(name: &str) -> Option<Rule> {
        RULES.iter().find(|(_, n)| *n == name).map(|r| r.0)
    }
}

/// A builder of another kind: the violations of `rules` (all when empty) of items
/// under `path`, owned by `owner` (any when empty), are not counted.
pub(super) struct Allow {
    /// A repo path prefix: a crate's directory, or a file.
    pub(super) path: &'static str,
    pub(super) owner: &'static str,
    pub(super) rules: &'static [Rule],
    pub(super) reason: &'static str,
}

/// The path of every `*-ffi` crate.
const FFI: &str = "crates/*-ffi/";

const GRAPH: &str = "crates/tutti-graph/";

const NODE_AUTHOR: &str = "tutti-graph's node-author API: what a node declares and what a \
    node is prepared with, not what an app configures";

const NOT_A_NODE: &str = "not a node's config: it holds no width to choose";

/// The allow-list. A row is permanent policy: it covers the violations it names and
/// stays when none exist.
pub(super) const ALLOW: &[Allow] = &[
    Allow {
        path: GRAPH,
        owner: "Shape",
        rules: &[],
        reason: NODE_AUTHOR,
    },
    Allow {
        path: GRAPH,
        owner: "Prepare",
        rules: &[],
        reason: NODE_AUTHOR,
    },
    Allow {
        path: GRAPH,
        owner: "Forked",
        rules: &[],
        reason: NODE_AUTHOR,
    },
    Allow {
        path: GRAPH,
        owner: "Row",
        rules: &[],
        reason: "the contract harness a node crate writes rows with: node-author test support",
    },
    Allow {
        path: GRAPH,
        owner: "Pulse",
        rules: &[],
        reason: "the contract harness a node crate writes rows with: node-author test support",
    },
    Allow {
        path: "crates/tutti-graph/src/testing.rs",
        owner: "",
        rules: &[],
        reason: "tutti-graph's test nodes, for node authors' tests",
    },
    Allow {
        path: "crates/tutti-nodes/src/testing.rs",
        owner: "",
        rules: &[],
        reason: "tutti-nodes' test oscillator, for node authors' tests",
    },
    Allow {
        path: "crates/tutti-graph/src/pool.rs",
        owner: "WorkerPoolBuilder",
        rules: &[],
        reason: "the pool follows rayon: rayon's names, builder shape and fallible build",
    },
    Allow {
        path: FFI,
        owner: "",
        rules: &[],
        reason: "an FFI crate's raw types mirror a C ABI or an OS call, not an app's configuration",
    },
    Allow {
        path: "crates/tutti-midi-hardware/src/virtual_endpoint.rs",
        owner: "VirtualSource",
        rules: &[Rule::CtorResult],
        reason: "opening an OS endpoint: the OS refuses, not a value a type could forbid",
    },
    Allow {
        path: "crates/tutti-measure/",
        owner: "",
        rules: &[Rule::ConfigWidth],
        reason: NOT_A_NODE,
    },
    Allow {
        path: "crates/tutti-au-host/",
        owner: "",
        rules: &[Rule::ConfigWidth],
        reason: NOT_A_NODE,
    },
    Allow {
        path: "crates/tutti-midi-network/",
        owner: "",
        rules: &[Rule::ConfigWidth],
        reason: NOT_A_NODE,
    },
    Allow {
        path: "crates/tutti-midi-protocol/",
        owner: "",
        rules: &[Rule::ConfigWidth],
        reason: NOT_A_NODE,
    },
    Allow {
        path: "crates/tutti-plugin-ipc/",
        owner: "",
        rules: &[Rule::ConfigWidth],
        reason: NOT_A_NODE,
    },
    Allow {
        path: "crates/tutti-session/src/pool/",
        owner: "",
        rules: &[Rule::ConfigWidth],
        reason: NOT_A_NODE,
    },
];

/// A violation: its key and rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Site {
    pub(super) key: String,
    pub(super) owner: String,
    pub(super) rule: Rule,
}

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git" | "tests" | "benches" | "examples")
}

fn source_files(tree: &dyn Tree) -> Vec<String> {
    tree.files("crates", &skipped)
        .into_iter()
        .filter(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            p.ends_with(".rs")
                && parts.get(2) == Some(&"src")
                && parts.last() != Some(&"tests.rs")
                && !parts.contains(&"tests")
        })
        .collect()
}

const BUILDING: [&str; 5] = ["Node", "Source", "Spec", "Config", "Options"];

fn building_owner(owner: &str) -> bool {
    BUILDING
        .iter()
        .any(|s| owner.ends_with(s) && owner.len() > s.len())
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Recv {
    Value,
    Ref,
    None,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Wrap {
    Plain,
    Result,
    Option,
}

fn receiver(first: Option<&str>) -> Recv {
    let Some(first) = first else {
        return Recv::None;
    };
    let t = squash(strip_attrs(first));
    if t.starts_with('&') {
        // `&self`, `&mut self`, `&'a self`, `&'a mut self`.
        let rest = t.trim_start_matches('&');
        let rest = match rest.strip_prefix('\'') {
            Some(l) => l.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_'),
            None => rest,
        };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix("mut ").unwrap_or(rest).trim();
        return if rest == "self" {
            Recv::Ref
        } else {
            Recv::None
        };
    }
    let rest = t.strip_prefix("mut ").unwrap_or(&t);
    if rest == "self" {
        return Recv::Value;
    }
    match rest.strip_prefix("self:") {
        Some(ty) if ty.trim_start().starts_with('&') => Recv::Ref,
        Some(_) => Recv::Value,
        None => Recv::None,
    }
}

/// The last path segment of `ty` without its generics.
fn bare(ty: &str) -> &str {
    let ty = ty.trim();
    let end = ty.find('<').unwrap_or(ty.len());
    ty[..end].rsplit("::").next().unwrap_or("").trim()
}

/// The wrapper of a return type and whether what it wraps is `Self` or `owner`.
fn built(ret: &str, owner: &str) -> Option<Wrap> {
    let r = squash(ret);
    let (wrap, inner) = if let Some(a) = r.strip_prefix("Result<") {
        (Wrap::Result, a)
    } else if let Some(a) = r.strip_prefix("Option<") {
        (Wrap::Option, a)
    } else {
        (Wrap::Plain, r.as_str())
    };
    let ty = bare(split_top(inner)[0]).trim_end_matches('>');
    (ty == "Self" || ty == owner).then_some(wrap)
}

/// Whether a parameter `pat: ty` names a width: a `ChannelLayout` or `BusTopology`,
/// directly, behind `impl Into<_>`, or as a generic with that `Into` bound.
fn is_width(param: &str, bounds: &str) -> bool {
    let p = squash(strip_attrs(param));
    let Some((_, ty)) = p.split_once(':') else {
        return false;
    };
    let ty = ty.trim();
    let wide = |t: &str| matches!(bare(t), "ChannelLayout" | "BusTopology");
    if wide(ty) {
        return true;
    }
    let into = |t: &str| {
        t.strip_prefix("Into<")
            .and_then(|r| r.strip_suffix('>'))
            .is_some_and(wide)
    };
    if let Some(t) = ty.strip_prefix("impl ") {
        return into(t.trim());
    }
    // A generic `L` bounded `L: Into<ChannelLayout>` in the generics or `where`.
    let b = squash(bounds);
    for w in ["ChannelLayout", "BusTopology"] {
        let pat = format!("{ty}:Into<{w}>");
        if b.contains(&pat) || b.contains(&format!("{ty}: Into<{w}>")) {
            return true;
        }
    }
    false
}

fn macro_bodies(code: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "macro_rules", from) {
        from = at + 11;
        let rest = code[from..].trim_start();
        let Some(rest) = rest.strip_prefix('!') else {
            continue;
        };
        let (name, after) = ident_at(code, code.len() - rest.len());
        let name = name.to_owned();
        let Some(open) = code[after..].find(['{', '(', '[']).map(|i| after + i) else {
            continue;
        };
        let (o, c) = match code.as_bytes()[open] {
            b'{' => (b'{', b'}'),
            b'(' => (b'(', b')'),
            _ => (b'[', b']'),
        };
        if let Some(close) = matching(code, open, o, c) {
            out.push((open, close, name));
            from = close;
        }
    }
    out
}

/// A `fn new` of one parameter returning `Option<..>` inside `body`.
fn writes_range_type(body: &str) -> bool {
    let mut from = 0;
    while let Some(at) = find_word_from(body, "new", from) {
        from = at + 3;
        if !body[..at].trim_end().ends_with("fn") {
            continue;
        }
        let rest = body[from..].trim_start();
        let Some(rest) = rest.strip_prefix('(') else {
            continue;
        };
        let off = body.len() - rest.len() - 1;
        let Some(close) = matching(body, off, b'(', b')') else {
            continue;
        };
        let params = &body[off + 1..close];
        let ret = squash(
            body[close + 1..]
                .trim_start()
                .split(['{', ';'])
                .next()
                .unwrap_or(""),
        );
        let one = split_top(params)
            .iter()
            .filter(|p| !p.trim().is_empty())
            .count()
            == 1;
        if one && ret.starts_with("->Option<") {
            return true;
        }
    }
    false
}

/// The violations of one file's text.
pub(super) fn scan(path: &str, src: &str) -> Vec<Site> {
    let code = blank(src);
    let tests = test_ranges(&code);
    let in_test = |p: usize| tests.iter().any(|&(a, z)| a <= p && p <= z);
    let macros = macro_bodies(&code);
    let in_macro = |p: usize| macros.iter().any(|(a, z, _)| *a < p && p < *z);
    let blocks = blocks(&code);
    let owner_of = |p: usize| {
        blocks
            .iter()
            .filter(|b| b.open < p && p < b.close)
            .max_by_key(|b| b.open)
    };
    let in_types = path.starts_with("crates/tutti-types/");
    let mut sites = BTreeSet::new();
    if !in_types {
        for (open, close, name) in &macros {
            if writes_range_type(&code[open + 1..*close]) {
                sites.insert(Site {
                    key: format!("{path}::{name}"),
                    owner: String::new(),
                    rule: Rule::RangeMacro,
                });
            }
        }
    }
    let mut from = 0;
    while let Some(p) = find_word_from(&code, "pub", from) {
        from = p + 3;
        if in_test(p) || in_macro(p) || code[from..].trim_start().starts_with('(') {
            continue;
        }
        let mut at = from;
        let (w, end) = loop {
            let (w, end) = ident_at(&code, at);
            match w {
                "const" | "async" | "unsafe" | "default" => at = end,
                "extern" => {
                    let rest = code[end..].trim_start();
                    at = end;
                    if rest.starts_with('"') {
                        let open = code[end..].find('"').map_or(end, |q| end + q);
                        at = code[open + 1..].find('"').map_or(open, |q| open + 1 + q) + 1;
                    }
                }
                _ => break (w, end),
            }
        };
        if w != "fn" {
            continue;
        }
        let Some(block) = owner_of(p) else { continue };
        let (name, mut cur) = ident_at(&code, end);
        let mut bounds = String::new();
        if code[cur..].trim_start().starts_with('<') {
            let lt = cur + (code[cur..].len() - code[cur..].trim_start().len());
            let Some(gt) = matching(&code, lt, b'<', b'>') else {
                continue;
            };
            bounds.push_str(&code[lt..=gt]);
            cur = gt + 1;
        }
        let rest = code[cur..].trim_start();
        if !rest.starts_with('(') {
            continue;
        }
        let open = code.len() - rest.len();
        let Some(close) = matching(&code, open, b'(', b')') else {
            continue;
        };
        let params: Vec<&str> = split_top(&code[open + 1..close])
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        let stop = item_end(&code, close + 1);
        let tail = &code[close + 1..stop];
        let (ret, wh) = match find_word_from(tail, "where", 0) {
            Some(w) => (&tail[..w], &tail[w..]),
            None => (tail, ""),
        };
        bounds.push_str(wh);
        let ret = ret.trim().strip_prefix("->").unwrap_or("").trim();
        let owner = block.owner.as_str();
        let recv = receiver(params.first().copied());
        let ty = built(ret, owner);
        let key = format!("{path}::{owner}::{name}");
        let mut hit = |rule| {
            sites.insert(Site {
                key: key.clone(),
                owner: owner.to_owned(),
                rule,
            });
        };
        match recv {
            Recv::Value => {
                if name.starts_with("with_") {
                    hit(Rule::WithPrefix);
                }
                if ty == Some(Wrap::Result) {
                    hit(Rule::ChainResult);
                }
            }
            Recv::Ref => {
                if name.starts_with("set_") && building_owner(owner) {
                    hit(Rule::Setter);
                }
            }
            Recv::None => {
                if let Some(wrap) = ty {
                    if owner.ends_with("Node") && owner.len() > 4 {
                        hit(Rule::NodeCtor);
                    }
                    if wrap == Wrap::Plain
                        && owner.ends_with("Config")
                        && owner.len() > 6
                        && !params.first().is_some_and(|f| is_width(f, &bounds))
                    {
                        hit(Rule::ConfigWidth);
                    }
                    if wrap == Wrap::Result && building_owner(owner) {
                        hit(Rule::CtorResult);
                    }
                }
            }
        }
        from = close;
    }
    sites.into_iter().collect()
}

fn crate_of(path: &str) -> &str {
    path.split('/').nth(1).unwrap_or("")
}

fn allowed(path: &str, s: &Site, a: &Allow) -> bool {
    let under = if a.path == FFI {
        crate_of(path).ends_with("-ffi")
    } else {
        path.starts_with(a.path)
    };
    under
        && (a.owner.is_empty() || a.owner == s.owner)
        && (a.rules.is_empty() || a.rules.contains(&s.rule))
}

/// A baseline row: crate, rule, key.
type Row = (String, Rule, String);

fn parse(text: &str) -> Result<BTreeSet<Row>, Vec<String>> {
    let mut rows = BTreeSet::new();
    let mut errors = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = n + 1;
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [krate, rule, key] = parts[..] else {
            errors.push(format!(
                "  {LIST}:{at}: not `<crate>  <rule>  <key>`: {line}"
            ));
            continue;
        };
        let Some(rule) = Rule::parse(rule) else {
            errors.push(format!("  {LIST}:{at}: `{rule}` is not a rule"));
            continue;
        };
        if key.split('/').nth(1) != Some(krate) {
            errors.push(format!("  {LIST}:{at}: `{key}` is not in crate `{krate}`"));
        }
        if !rows.insert((krate.to_owned(), rule, key.to_owned())) {
            errors.push(format!("  {LIST}:{at}: `{line}` is listed twice"));
        }
    }
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

/// The violations of the tree that `allow` does not cover.
fn found(tree: &dyn Tree, allow: &[Allow]) -> BTreeSet<Row> {
    let mut sites = BTreeSet::new();
    for path in source_files(tree) {
        let Some(src) = tree.read(&path) else {
            continue;
        };
        for s in scan(&path, &src) {
            if !allow.iter().any(|a| allowed(&path, &s, a)) {
                sites.insert((crate_of(&path).to_owned(), s.rule, s.key));
            }
        }
    }
    sites
}

pub(super) fn check(tree: &dyn Tree, allow: &[Allow], min_sites: usize) -> Report {
    let mut r = Report::default();
    let Some(text) = tree.read(LIST) else {
        r.fail(format!("check-building: FAIL: {LIST} is missing"));
        return r;
    };
    let rows = match parse(&text) {
        Ok(rows) => rows,
        Err(errors) => {
            r.fail(format!("check-building: FAIL ({LIST} is malformed):"));
            errors.into_iter().for_each(|e| r.fail(e));
            return r;
        }
    };
    let sites = found(tree, allow);
    if sites.len() < min_sites {
        r.fail(format!(
            "check-building: FAIL: {} violations found, at least {min_sites} expected; the walk is broken",
            sites.len()
        ));
        return r;
    }
    let fmt = |(c, rule, k): &Row| format!("{c}  {}  {k}", rule.name());
    let new: Vec<_> = sites.difference(&rows).collect();
    let stale: Vec<_> = rows.difference(&sites).collect();
    if !new.is_empty() {
        r.fail(
            "check-building: FAIL (a configured thing is built another way than the one shape; \
             build it the one way, or an allow row in the gate says why it is another kind):",
        );
        new.into_iter()
            .for_each(|row| r.fail(format!("  {}", fmt(row))));
        r.say(
            "check-building: the builders of another kind (ALLOW in xtask/src/gates/building.rs):",
        );
        for a in allow {
            let owner = if a.owner.is_empty() { "*" } else { a.owner };
            r.say(format!("  {} {owner}: {}", a.path, a.reason));
        }
    }
    if !stale.is_empty() {
        r.fail(format!(
            "check-building: FAIL (a row of {LIST} names no violation; delete it):"
        ));
        stale
            .into_iter()
            .for_each(|row| r.fail(format!("  {}", fmt(row))));
    }
    if r.passed() {
        r.say("check-building: OK");
    }
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    for (c, _, _) in &sites {
        *by.entry(c).or_default() += 1;
    }
    let counts: Vec<String> = by.iter().map(|(c, n)| format!("{c} {n}")).collect();
    r.say(format!(
        "check-building: {} violations, {} rows in {LIST} ({})",
        sites.len(),
        rows.len(),
        counts.join(", ")
    ));
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const F: &str = "crates/a/src/lib.rs";

    fn run(list: &str, src: &str, allow: &[Allow]) -> Report {
        check(&Mem::with(&[(LIST, list), (F, src)]), allow, 0)
    }

    fn rules(src: &str) -> Vec<(String, &'static str)> {
        scan(F, src)
            .into_iter()
            .map(|s| (s.key.rsplit("::").next().unwrap().to_owned(), s.rule.name()))
            .collect()
    }

    fn one(name: &str, rule: &'static str) -> Vec<(String, &'static str)> {
        vec![(name.to_owned(), rule)]
    }

    #[test]
    fn a_chained_option_named_with_is_a_violation_and_a_bare_name_is_not() {
        let bad = "pub struct T; impl T { pub fn with_gain(mut self, g: f32) -> Self { self } }";
        assert_eq!(rules(bad), one("with_gain", "with-prefix"));
        let ok = "pub struct T; impl T { pub fn gain(mut self, g: f32) -> Self { self }\n\
                  pub fn with_view(&self) -> u8 { 0 } }";
        assert!(rules(ok).is_empty());
    }

    #[test]
    fn a_chained_option_returning_result_self_is_a_violation_and_a_terminal_build_is_not() {
        let bad =
            "pub struct T; impl T { pub fn depth(self, d: u8) -> Result<Self, E> { Ok(self) } }";
        assert_eq!(rules(bad), one("depth", "chain-result"));
        let bare = "pub struct T; impl T { pub fn depth(self) -> Result<T> { Ok(self) } }";
        assert_eq!(rules(bare), one("depth", "chain-result"));
        let ok = "pub struct B; impl B { pub fn build(self) -> Result<Product, E> { todo!() }\n\
                  pub fn depth(self, d: u8) -> Self { self } }";
        assert!(rules(ok).is_empty());
    }

    #[test]
    fn a_setter_on_a_node_source_or_spec_is_a_violation_and_a_setter_on_a_handle_is_not() {
        for owner in [
            "GainNode",
            "FileSource",
            "RenderSpec",
            "MixConfig",
            "LoadOptions",
        ] {
            let src = format!(
                "impl {owner} {{ pub fn set_gain(&self, g: f32) {{}}\n\
                               pub fn set_mode(&mut self, m: u8) {{}} }}"
            );
            assert_eq!(
                rules(&src),
                [
                    ("set_gain".to_owned(), "setter"),
                    ("set_mode".to_owned(), "setter")
                ],
                "{owner}"
            );
        }
        let ok = "impl GainHandle { pub fn set_gain(&self, g: f32) {} }\n\
                  impl Node { pub fn set_gain(&self, g: f32) {} }\n\
                  impl GainNode { pub fn set_gain(self, g: f32) -> Self { self } }";
        assert!(rules(ok).is_empty());
    }

    #[test]
    fn a_public_constructor_on_a_node_is_a_violation_and_one_on_its_config_is_not() {
        let bad = "impl GainNode { pub fn new(c: GainConfig) -> Self { todo!() }\n\
                   pub fn open(p: &str) -> Result<Self, E> { todo!() }\n\
                   pub fn maybe() -> Option<GainNode> { None } }";
        let got = rules(bad);
        assert!(got.contains(&("new".to_owned(), "node-ctor")));
        assert!(got.contains(&("open".to_owned(), "node-ctor")));
        assert!(got.contains(&("maybe".to_owned(), "node-ctor")));
        let ok = "impl GainNode { pub fn gain(&self) -> Param<Db> { todo!() }\n\
                  pub(crate) fn new() -> Self { todo!() } }\n\
                  impl Other { pub fn new() -> Self { todo!() } }";
        assert!(rules(ok).is_empty());
    }

    #[test]
    fn a_config_constructor_must_take_a_width_first() {
        let ok = "impl AConfig { pub fn new(l: ChannelLayout, r: u8) -> Self { todo!() } }\n\
                  impl BConfig { pub fn new(t: tutti_types::BusTopology) -> Self { todo!() } }\n\
                  impl CConfig { pub fn new(l: impl Into<ChannelLayout>) -> Self { todo!() } }\n\
                  impl DConfig { pub fn new<L: Into<BusTopology>>(l: L) -> Self { todo!() } }\n\
                  impl EConfig { pub fn new<L>(l: L) -> Self where L: Into<ChannelLayout> { todo!() } }\n\
                  impl FConfig { pub fn gain(&self) -> Self { todo!() } }";
        assert!(rules(ok).is_empty());
        for bad in [
            "impl XConfig { pub fn new(rate: u32) -> Self { todo!() } }",
            "impl XConfig { pub fn new() -> Self { todo!() } }",
            "impl XConfig { pub fn new(r: u32, l: ChannelLayout) -> Self { todo!() } }",
            "impl XConfig { pub fn new<L: Into<u8>>(l: L) -> Self { todo!() } }",
        ] {
            assert_eq!(rules(bad), one("new", "config-width"), "{bad}");
        }
    }

    #[test]
    fn a_result_constructor_on_a_building_owner_is_a_violation_and_a_plain_one_is_not() {
        let bad = "impl PhaserConfig { pub fn new(r: f32) -> Result<Self, E> { todo!() } }";
        assert!(rules(bad).contains(&("new".to_owned(), "ctor-result")));
        let ok = "impl Wave { pub fn open(p: &str) -> Result<Self, E> { todo!() } }\n\
                  impl XSource { pub fn probe(p: &str) -> Result<Report, E> { todo!() } }";
        assert!(rules(ok).is_empty());
    }

    #[test]
    fn a_macro_writing_a_range_type_is_a_violation_outside_tutti_types_only() {
        let src = "macro_rules! ranged { ($n:ident) => { impl $n { pub fn new(raw: u8) -> Option<Self> { todo!() } } }; }\n\
                   macro_rules! plain { ($n:ident) => { impl $n { pub fn new(a: u8, b: u8) -> Option<Self> { todo!() } } }; }";
        assert_eq!(rules(src), one("ranged", "range-macro"));
        assert!(scan("crates/tutti-types/src/lib.rs", src).is_empty());
    }

    #[test]
    fn items_in_tests_comments_strings_and_non_public_fns_are_not_read() {
        let src = "// pub fn with_a(self) -> Self\nconst S: &str = \"impl XNode { pub fn new() -> Self }\";\n\
                   impl T { pub(crate) fn with_b(self) -> Self { self } }\n\
                   #[cfg(test)] mod m { impl T { pub fn with_c(self) -> Self { self } } }\n\
                   trait Q { fn with_d(self) -> Self; }\n";
        assert!(rules(src).is_empty());
    }

    #[test]
    fn a_violation_without_a_row_fails_and_names_the_row_to_write() {
        let src = "impl T { pub fn with_x(self) -> Self { self } }";
        let r = run("", src, &[]);
        assert!(!r.passed());
        assert_eq!(
            r.findings[1],
            "  a  with-prefix  crates/a/src/lib.rs::T::with_x"
        );
    }

    #[test]
    fn a_baselined_violation_passes_with_its_count() {
        let src = "impl T { pub fn with_x(self) -> Self { self } }";
        let r = run("a  with-prefix  crates/a/src/lib.rs::T::with_x\n", src, &[]);
        assert!(r.passed());
        assert_eq!(
            r.out[1],
            "check-building: 1 violations, 1 rows in xtask/building-baseline.txt (a 1)"
        );
    }

    #[test]
    fn a_row_with_no_violation_is_stale_and_fails() {
        let r = run(
            "a  with-prefix  crates/a/src/lib.rs::T::with_x\n",
            "pub fn f() {}",
            &[],
        );
        assert!(!r.passed());
        assert_eq!(
            r.findings[1],
            "  a  with-prefix  crates/a/src/lib.rs::T::with_x"
        );
    }

    #[test]
    fn a_clean_tree_passes() {
        let r = run("", "impl T { pub fn x(self) -> Self { self } }", &[]);
        assert!(r.passed());
        assert_eq!(r.out[0], "check-building: OK");
    }

    #[test]
    fn an_allowed_item_passes_without_a_row_and_a_row_for_it_is_stale() {
        let src = "impl Shape { pub fn with_cost(self, c: u8) -> Self { self } }";
        let allow = [Allow {
            path: "crates/a/",
            owner: "Shape",
            rules: &[],
            reason: "r",
        }];
        assert!(run("", src, &allow).passed());
        let r = run(
            "a  with-prefix  crates/a/src/lib.rs::Shape::with_cost\n",
            src,
            &allow,
        );
        assert!(!r.passed());
        let other = "impl Other { pub fn with_cost(self, c: u8) -> Self { self } }";
        assert!(!run("", other, &allow).passed());
    }

    #[test]
    fn an_allow_row_covers_only_its_rules_and_the_ffi_crates_by_name() {
        let src = "impl XConfig { pub fn new(r: u8) -> Result<Self, E> { todo!() } }";
        let one_rule = [Allow {
            path: "crates/a/",
            owner: "",
            rules: &[Rule::ConfigWidth],
            reason: "r",
        }];
        let r = run("", src, &one_rule);
        assert_eq!(
            r.findings[1],
            "  a  ctor-result  crates/a/src/lib.rs::XConfig::new"
        );
        let ffi = [Allow {
            path: FFI,
            owner: "",
            rules: &[],
            reason: "r",
        }];
        let tree = Mem::with(&[
            (LIST, ""),
            (
                "crates/a-ffi/src/lib.rs",
                "impl T { pub fn with_x(self) -> Self { self } }",
            ),
            (
                "crates/b/src/lib.rs",
                "impl T { pub fn with_x(self) -> Self { self } }",
            ),
        ]);
        let r = check(&tree, &ffi, 0);
        assert_eq!(
            r.findings[1],
            "  b  with-prefix  crates/b/src/lib.rs::T::with_x"
        );
    }

    #[test]
    fn a_malformed_or_misfiled_row_fails() {
        for list in [
            "a with-prefix\n",
            "a nonsense crates/a/src/lib.rs::T::f\n",
            "b with-prefix crates/a/src/lib.rs::T::f\n",
            "a with-prefix crates/a/src/lib.rs::T::f\na with-prefix crates/a/src/lib.rs::T::f\n",
        ] {
            let r = run(list, "", &[]);
            assert!(!r.passed(), "{list}");
            assert!(r.findings[0].contains("malformed"));
        }
    }

    #[test]
    fn a_walk_that_finds_too_few_violations_fails() {
        let tree = Mem::with(&[(LIST, ""), (F, "pub fn f() {}")]);
        assert!(check(&tree, &[], 1).findings[0].contains("the walk is broken"));
    }
}
