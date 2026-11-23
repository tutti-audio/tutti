//! The scan: one file read into its violations.

use super::super::Tree;
use super::super::buffers::{blocks, item_end, split_top, squash};
use super::super::lex::{
    blank, find_word_from, ident_at, matching, skipped_library, strip_attrs, test_ranges,
};
use super::Rule;
use super::crate_of;
use std::collections::{BTreeMap, BTreeSet};

/// A violation: its key and rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Site {
    pub(super) key: String,
    pub(super) owner: String,
    pub(super) rule: Rule,
}

pub(super) fn source_files(tree: &dyn Tree) -> Vec<String> {
    tree.files("crates", &skipped_library)
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
    // A wrapper is named by its last path segment: `io::Result<Self>` is a `Result`.
    let wrapper = r.find('<').and_then(|lt| {
        let seg = r[..lt].rsplit("::").next().unwrap_or("");
        match seg {
            "Result" => Some((Wrap::Result, &r[lt + 1..])),
            "Option" => Some((Wrap::Option, &r[lt + 1..])),
            _ => None,
        }
    });
    let (wrap, inner) = wrapper.unwrap_or((Wrap::Plain, r.as_str()));
    let ty = bare(split_top(inner)[0]).trim_end_matches('>');
    (ty == "Self" || ty == owner).then_some(wrap)
}

/// Whether a parameter `pat: ty` names a width: a `ChannelLayout` or `ChannelTopology`,
/// directly, behind `impl Into<_>`, or as a generic with that `Into` bound.
fn is_width(param: &str, bounds: &str) -> bool {
    let p = squash(strip_attrs(param));
    let Some((_, ty)) = p.split_once(':') else {
        return false;
    };
    let ty = ty.trim();
    let wide = |t: &str| matches!(bare(t), "ChannelLayout" | "ChannelTopology");
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
    for w in ["ChannelLayout", "ChannelTopology"] {
        let pat = format!("{ty}:Into<{w}>");
        if b.contains(&pat) || b.contains(&format!("{ty}: Into<{w}>")) {
            return true;
        }
    }
    false
}

/// Whether a parameter `pat: ty` is an `Option<..>` (any path to it), an
/// `impl Into<Option<..>>`, or a generic bounded `Into<Option<..>>` in `bounds`
/// (the generics and the `where` clause).
fn takes_option(param: &str, bounds: &str) -> bool {
    let p = squash(strip_attrs(param));
    let Some((_, ty)) = p.split_once(':') else {
        return false;
    };
    let ty = ty.trim();
    if let Some(t) = ty.strip_prefix("impl ") {
        return t
            .trim()
            .strip_prefix("Into<")
            .is_some_and(|r| bare(r) == "Option");
    }
    if bare(ty) == "Option" && ty.contains('<') {
        return true;
    }
    let b = squash(bounds);
    b.contains(&format!("{ty}:Into<Option<")) || b.contains(&format!("{ty}: Into<Option<"))
}

/// The type path a parameter `pat: ty` takes, without generics, with `&`, `&mut`, a
/// lifetime, `impl Into<_>` and a generic's `Into<_>` bound peeled.
fn param_path(param: &str, bounds: &str) -> Option<String> {
    let p = squash(strip_attrs(param));
    let (_, ty) = p.split_once(':')?;
    let peel = |t: &str| -> String {
        let mut t = t.trim().trim_start_matches('&').trim_start();
        if let Some(r) = t.strip_prefix('\'') {
            t = r
                .trim_start_matches(|c: char| c.is_alphanumeric() || c == '_')
                .trim_start();
        }
        t.strip_prefix("mut ").unwrap_or(t).trim().to_owned()
    };
    let inner = |t: &str| -> Option<String> {
        let r = t.strip_prefix("Into<")?.strip_suffix('>')?;
        Some(peel(r))
    };
    let ty = peel(ty);
    let ty = if let Some(t) = ty.strip_prefix("impl ") {
        inner(t.trim())?
    } else {
        let b = squash(bounds);
        let at = b
            .find(&format!("{ty}:Into<"))
            .or_else(|| b.find(&format!("{ty}: Into<")));
        match at {
            Some(i) => {
                let rest = &b[i + ty.len()..];
                let rest = rest.trim_start_matches(':').trim_start();
                let close = matching(rest, 4, b'<', b'>')?;
                inner(&rest[..=close]).unwrap_or(ty)
            }
            None => ty,
        }
    };
    let path = ty[..ty.find('<').unwrap_or(ty.len())]
        .trim()
        .trim_start_matches("::");
    (!path.is_empty()
        && path
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':'))
    .then(|| path.to_owned())
}

/// The crate directory name a path's first segment names: `tutti_x` is `tutti-x`,
/// `crate`, `self`, `super` and a local module are `own`; `None` for `std`, `core`
/// and `alloc`.
fn first_crate(seg: &str, own: &str) -> Option<String> {
    match seg {
        "std" | "core" | "alloc" => None,
        s if s.starts_with("tutti_") => Some(s.replace('_', "-")),
        _ => Some(own.to_owned()),
    }
}

/// What a file's `use` items bring in: each local name with the crate it comes from,
/// and the crates of its glob imports.
#[derive(Default)]
struct Imports {
    /// A local name: the crate it comes from and the name it has there.
    names: BTreeMap<String, (Option<String>, String)>,
    globs: BTreeSet<String>,
}

impl Imports {
    fn add(&mut self, tree: &str, prefix: &[String], own: &str) {
        let t = tree.trim();
        if let Some(b) = t.find('{') {
            let Some(close) = matching(t, b, b'{', b'}') else {
                return;
            };
            let mut pre = prefix.to_vec();
            pre.extend(segments(&t[..b]));
            for part in split_top(&t[b + 1..close]) {
                self.add(part, &pre, own);
            }
            return;
        }
        let (path, alias) = match find_word_from(t, "as", 0) {
            Some(a) => (&t[..a], Some(t[a + 2..].trim())),
            None => (t, None),
        };
        let mut full = prefix.to_vec();
        full.extend(segments(path));
        let Some(first) = full.first() else { return };
        let krate = first_crate(first, own);
        match full.last().map(String::as_str) {
            Some("*") => {
                self.globs.extend(krate);
            }
            Some("self") if full.len() >= 2 => {
                let local = full[full.len() - 2].clone();
                let orig = local.clone();
                self.names.insert(local, (krate, orig));
            }
            Some(last) => {
                let local = alias.unwrap_or(last).to_owned();
                self.names.insert(local, (krate, last.to_owned()));
            }
            None => {}
        }
    }

    /// The `(crate, name)` a type path may name, in a file of crate `own`.
    fn resolve(&self, path: &str, own: &str) -> Vec<(String, String)> {
        let segs = segments(path);
        let Some(last) = segs.last() else {
            return Vec::new();
        };
        let pair = |c: &String| (c.clone(), last.clone());
        match segs.as_slice() {
            [name] => match self.names.get(name) {
                Some((c, orig)) => c.iter().map(|c| (c.clone(), orig.clone())).collect(),
                None => std::iter::once(own.to_owned())
                    .chain(self.globs.iter().cloned())
                    .map(|c| (c, last.clone()))
                    .collect(),
            },
            [first, ..] => match self.names.get(first) {
                Some((c, _)) => c.iter().map(pair).collect(),
                None => first_crate(first, own).iter().map(pair).collect(),
            },
            [] => Vec::new(),
        }
    }
}

fn segments(path: &str) -> Vec<String> {
    path.split("::")
        .map(|x| x.trim().to_owned())
        .filter(|x| !x.is_empty())
        .collect()
}

/// The type an `impl Default for X` header (generics, a path prefix and a `where`
/// clause allowed) is for.
fn default_target(header: &str) -> Option<String> {
    let mut h = header.trim();
    if h.starts_with('<') {
        h = h[matching(h, 0, b'<', b'>')? + 1..].trim();
    }
    if let Some(w) = find_word_from(h, "where", 0) {
        h = h[..w].trim();
    }
    let f = find_word_from(h, "for", 0)?;
    (bare(&h[..f]) == "Default").then(|| bare(h[f + 3..].trim()).to_owned())
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

/// Whether one field's text (a named or tuple field) starts with a bare `pub`.
fn has_pub_field(field: &str) -> bool {
    let f = strip_attrs(field);
    let (w, end) = ident_at(f, 0);
    w == "pub" && !f[end..].trim_start().starts_with('(')
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
#[cfg(test)]
pub(super) fn scan(path: &str, src: &str) -> Vec<Site> {
    let parsed = parse_file(path, src);
    let nodes = parsed.nodes.clone();
    let usual = Usual::of(std::iter::once(&parsed));
    parsed.finish(&nodes, &usual)
}

/// One file's reading: its violations, its `config-width` candidates (kept apart until
/// the tree's node configs are known) and the `*Config` names it converts into a node.
pub(super) struct Parsed {
    krate: String,
    sites: BTreeSet<Site>,
    widths: Vec<Site>,
    pub(super) nodes: BTreeSet<String>,
    /// `default-enum-arg` candidates: a site and the `(crate, name)` each parameter's
    /// type may be.
    args: Vec<(Site, Vec<(String, String)>)>,
    /// The `pub enum`s that derive `Default`, and every `pub enum`.
    derived: BTreeSet<String>,
    pub_enums: BTreeSet<String>,
    /// The types an `impl Default for` names.
    hand: BTreeSet<String>,
}

/// The `(crate, name)` of every `pub enum` that has a usual value: a derived `Default`
/// with a `#[default]` variant, or a hand `impl Default for` in its crate.
#[derive(Default)]
pub(super) struct Usual {
    enums: BTreeSet<(String, String)>,
}

impl Usual {
    pub(super) fn of<'a>(files: impl Iterator<Item = &'a Parsed> + Clone) -> Usual {
        let mut u = Usual::default();
        let mut impls = BTreeSet::new();
        let mut enums = BTreeSet::new();
        for p in files {
            u.enums
                .extend(p.derived.iter().map(|n| (p.krate.clone(), n.clone())));
            impls.extend(p.hand.iter().map(|n| (p.krate.clone(), n.clone())));
            enums.extend(p.pub_enums.iter().map(|n| (p.krate.clone(), n.clone())));
        }
        u.enums.extend(impls.intersection(&enums).cloned());
        u
    }
}

impl Parsed {
    /// The violations, with a `config-width` candidate kept when its owner is in `nodes`
    /// and a `default-enum-arg` candidate kept when a parameter's type is in `usual`.
    pub(super) fn finish(self, nodes: &BTreeSet<String>, usual: &Usual) -> Vec<Site> {
        let mut sites = self.sites;
        sites.extend(self.widths.into_iter().filter(|s| nodes.contains(&s.owner)));
        sites.extend(self.args.into_iter().filter_map(|(s, names)| {
            names
                .iter()
                .any(|c| c.1 != s.owner && usual.enums.contains(c))
                .then_some(s)
        }));
        sites.into_iter().collect()
    }
}

/// The `*Config` an `impl From<Config..> for ..Node..` or `impl TryFrom<Config..> for
/// ..Node..` header converts into a node, if it is one.
fn converted_config(header: &str) -> Option<String> {
    let mut h = header.trim();
    if h.starts_with('<') {
        h = h[matching(h, 0, b'<', b'>')? + 1..].trim();
    }
    if let Some(w) = find_word_from(h, "where", 0) {
        h = h[..w].trim();
    }
    let f = find_word_from(h, "for", 0)?;
    let (tr, target) = (h[..f].trim(), h[f + 3..].trim());
    let lt = tr.find('<')?;
    if !matches!(bare(&tr[..lt]), "From" | "TryFrom") {
        return None;
    }
    let arg = split_top(tr[lt + 1..].trim_end().strip_suffix('>')?)[0];
    let config = bare(arg.trim().trim_start_matches('&'));
    let node = bare(target);
    (config.ends_with("Config") && config.len() > 6 && node.ends_with("Node") && node.len() > 4)
        .then(|| config.to_owned())
}

/// The `*Config` a parameter `pat: ty` takes, directly or behind `impl Into<_>`.
fn config_param(param: &str) -> Option<String> {
    let p = squash(strip_attrs(param));
    let (_, ty) = p.split_once(':')?;
    let ty = ty.trim().trim_start_matches('&');
    let ty = ty
        .strip_prefix("implInto<")
        .or_else(|| ty.strip_prefix("impl Into<"))
        .map_or(ty, |t| t.trim_end_matches('>'));
    let name = bare(ty);
    (name.ends_with("Config") && name.len() > 6).then(|| name.to_owned())
}

pub(super) fn parse_file(path: &str, src: &str) -> Parsed {
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
    let in_macro_home =
        path.starts_with("crates/tutti-units/") || path == "crates/tutti-rt/src/id.rs";
    let mut sites = BTreeSet::new();
    let mut widths = Vec::new();
    let mut nodes = BTreeSet::new();
    let own = crate_of(path);
    let mut imports = Imports::default();
    let mut u = 0;
    while let Some(p) = find_word_from(&code, "use", u) {
        u = p + 3;
        if in_test(p) || in_macro(p) {
            continue;
        }
        let end = code[u..].find(';').map_or(code.len(), |i| u + i);
        imports.add(&code[u..end], &[], own);
    }
    let mut args = Vec::new();
    let mut derived = BTreeSet::new();
    let mut pub_enums = BTreeSet::new();
    let mut hand = BTreeSet::new();
    let mut at = 0;
    while let Some(p) = find_word_from(&code, "impl", at) {
        at = p + 4;
        let head = code[..p].trim_end();
        let head_ok = head.is_empty()
            || head.ends_with(['}', ';', '{', ']'])
            || head.ends_with("unsafe")
            || head.ends_with("default");
        if !head_ok || in_test(p) {
            continue;
        }
        let end = code[at..].find(['{', ';']).map_or(code.len(), |i| at + i);
        nodes.extend(converted_config(&code[at..end]));
        hand.extend(default_target(&code[at..end]));
    }
    let mut at = 0;
    while let Some(e) = find_word_from(&code, "enum", at) {
        at = e + 4;
        let before = code[..e].trim_end();
        let Some(pub_at) = before.len().checked_sub(3) else {
            continue;
        };
        if !before.ends_with("pub")
            || in_test(e)
            || before[..pub_at].ends_with(|c: char| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let (name, after) = ident_at(&code, e + 4);
        if name.is_empty() {
            continue;
        }
        pub_enums.insert(name.to_owned());
        let start = code[..pub_at].rfind(['}', ';', '{']).map_or(0, |i| i + 1);
        let attrs = &code[start..pub_at];
        let Some(open) = code[after..].find('{').map(|i| after + i) else {
            continue;
        };
        let Some(close) = matching(&code, open, b'{', b'}') else {
            continue;
        };
        if attrs.contains("derive")
            && find_word_from(attrs, "Default", 0).is_some()
            && squash(&code[open..close]).contains("#[default]")
        {
            derived.insert(name.to_owned());
        }
    }
    if !in_macro_home {
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
    let mut at = 0;
    while let Some(e) = find_word_from(&code, "struct", at) {
        at = e + 6;
        let before = code[..e].trim_end();
        let Some(pub_at) = before.len().checked_sub(3) else {
            continue;
        };
        if !before.ends_with("pub")
            || in_test(e)
            || in_macro(e)
            || before[..pub_at].ends_with(|c: char| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let (name, mut cur) = ident_at(&code, e + 6);
        let suffixed = ["Config", "Settings", "Options"]
            .iter()
            .any(|x| name.ends_with(x));
        if !suffixed {
            continue;
        }
        if code[cur..].trim_start().starts_with('<') {
            let lt = cur + (code[cur..].len() - code[cur..].trim_start().len());
            let Some(gt) = matching(&code, lt, b'<', b'>') else {
                continue;
            };
            cur = gt + 1;
        }
        let rest = code[cur..].trim_start();
        let (open, close) = if rest.starts_with('(') {
            let o = code.len() - rest.len();
            (o, matching(&code, o, b'(', b')'))
        } else {
            let o = item_end(&code, cur);
            (o, matching(&code, o, b'{', b'}'))
        };
        let Some(close) = close else { continue };
        if split_top(&code[open + 1..close])
            .into_iter()
            .any(has_pub_field)
        {
            sites.insert(Site {
                key: format!("{path}::{name}"),
                owner: name.to_owned(),
                rule: Rule::PubFieldConfig,
            });
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
        let chained = recv == Recv::Value && ty.is_some();
        let skip = usize::from(recv != Recv::None);
        if !chained && params.len() > skip + 1 {
            // A keyed setter's last parameter is the value it sets.
            let set = params.len() - usize::from(name.starts_with("set_"));
            let names: Vec<(String, String)> = params[..set]
                .iter()
                .skip(skip)
                .filter_map(|p| param_path(p, &bounds))
                .flat_map(|path| imports.resolve(&path, own))
                .collect();
            if !names.is_empty() {
                args.push((
                    Site {
                        key: key.clone(),
                        owner: owner.to_owned(),
                        rule: Rule::DefaultEnumArg,
                    },
                    names,
                ));
            }
        }
        match recv {
            Recv::Value => {
                if name.starts_with("with_") {
                    hit(Rule::WithPrefix);
                }
                if ty == Some(Wrap::Result) {
                    hit(Rule::ChainResult);
                }
                if ty == Some(Wrap::Plain)
                    && params.iter().skip(1).any(|p| takes_option(p, &bounds))
                {
                    hit(Rule::OptionArg);
                }
            }
            Recv::Ref => {
                if name.starts_with("set_") {
                    hit(Rule::Setter);
                }
            }
            Recv::None => {
                if let Some(wrap) = ty {
                    if owner.ends_with("Node") && owner.len() > 4 {
                        hit(Rule::NodeCtor);
                    }
                    if wrap == Wrap::Result {
                        hit(Rule::CtorResult);
                    }
                }
                if owner.ends_with("Node") && owner.len() > 4 {
                    nodes.extend(params.iter().find_map(|p| config_param(p)));
                }
                if ty == Some(Wrap::Plain)
                    && owner.ends_with("Config")
                    && owner.len() > 6
                    && !params.first().is_some_and(|f| is_width(f, &bounds))
                {
                    widths.push(Site {
                        key: key.clone(),
                        owner: owner.to_owned(),
                        rule: Rule::ConfigWidth,
                    });
                }
            }
        }
        from = close;
    }
    Parsed {
        krate: crate_of(path).to_owned(),
        sites,
        widths,
        nodes,
        args,
        derived,
        pub_enums,
        hand,
    }
}
