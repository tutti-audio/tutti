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
//! Its *built type* is the return type with one `Result<..>` or `Option<..>` wrapper (any path to it, as `io::Result<..>`)
//! peeled, generics dropped, when that is `Self` or the owner's name; a return type
//! that is anything else (`Param<U>`, a tuple, a trait object) has none.
//!
//! **The rules** (a row's rule names one):
//!
//! - `with-prefix`: a receiver by value and a name that starts with `with_`.
//! - `chain-result`: a receiver by value and a return type `Result<Self, ..>` or
//!   `Result<Owner, ..>`. A terminal `build(self) -> Result<Product>` is not one: its
//!   built type is not its owner.
//! - `setter`: a receiver by reference and a name that starts with `set_`, on any
//!   owner: wider than the design's node, source or spec, and the difference is carried
//!   by named rows of [`ALLOW`].
//! - `node-ctor`: no receiver, a built type, on a `*Node` owner (a `Result` or
//!   `Option` wrapper counts).
//! - `config-width`: no receiver, return type `Self` or the owner (no wrapper: an
//!   `Option` or `Result` return is not rule 2's "returning `Self`", and `ctor-result`
//!   counts the `Result`), on a *node config*, whose first parameter is not a `ChannelLayout` or `BusTopology`
//!   (the last path segment of a type, or of an `impl Into<_>` or a `T: Into<_>` bound
//!   named in the generics or the `where` clause). A function with no parameter has no
//!   first one. A node config is a `*Config` owner that the shipped tree converts into
//!   a node: an `impl From<Owner..> for X` or `impl TryFrom<Owner..> for X` (generics on
//!   either side, any path prefix) whose `X`'s last path segment ends in `Node` (a type
//!   alias such as `pub type CrossoverNode<F> = KernelNode<..>` is named so), or a
//!   `*Node` owner's no-receiver `pub fn` with a parameter whose type is the owner
//!   (by value, by reference or behind `impl Into<_>`: a constructor like
//!   `new(config: XConfig)`). The width is the first choice of a node; an analysis,
//!   device or transport config is no node config and is not read, whatever its name.
//! - `ctor-result`: no receiver, a built type wrapped in `Result`, on any owner.
//! - `range-macro`: a `macro_rules!` outside `tutti-units` and `tutti-rt`'s `id.rs` (the homes of
//!   `ranged!` and `id!`) whose body holds a
//!   `fn new` of one parameter returning `Option<..>`, the `new(raw) -> Option`
//!   signature of a range-checked type.
//! - `option-arg`: a receiver by value, a built type of `Self` or the owner (no wrapper:
//!   an option returns `Self`), and a parameter after the receiver whose type is an
//!   `Option<..>` (any path to it), an `impl Into<Option<..>>` or a generic bounded
//!   `Into<Option<..>>`: "none" is said by not
//!   calling the option. A terminal `build(self)` whose built type is not its owner is
//!   not one.
//! - `default-enum-arg`: a parameter (after the receiver, any when there is none) whose
//!   type, with `&`, `&mut`, `impl Into<_>` and a `T: Into<_>` bound peeled, names an
//!   enum that has a usual value, in the crate the name resolves to: a `pub enum` with
//!   `#[derive(.. Default ..)]` and a `#[default]` variant, or a `pub enum` with a
//!   hand `impl Default for` (any generics, any path prefix) in its crate. A name
//!   that resolves to a struct, to an enum with no usual value, or to a crate
//!   outside `crates/` is no hit. The usual
//!   value is the option's default, so it is said by not calling the option. Not
//!   one: a receiver by value whose built type (`Self`, the owner, any wrapper) is
//!   its owner, which is how a chained option takes its value, a function whose owner
//!   is the enum itself, a function with no other parameter beside the receiver
//!   (`set_mode(&mut self, mode)`), and a `set_*` function's last parameter (a keyed
//!   setter, `set_mode(&mut self, key, mode)`): those calls exist to pass that value.
//! - `pub-field-config`: a `pub struct` (a `pub(..)` item is not public) whose name ends
//!   in `Config`, `Settings` or `Options` (the bare name too) and that has at
//!   least one `pub` field (a `pub(..)` field is not public), named or tuple, outside a
//!   `macro_rules!` body. A configured thing is `Type::new(required..)` plus chained
//!   options; a struct of public fields filled with `..Default::default()` has no
//!   method for the other rules to see. Its key is `<file>::<Type>`. A struct that is
//!   plain data (a wire, IPC or FFI shape, or the protocol's own fields) is a named row
//!   of [`ALLOW`] with its reason. The rule's limit: it catches the config-by-name shape
//!   only. The method-shape rules above stay the real check, and a value type that is no
//!   configuration but has public fields is fine by the style guide's T5.
//!
//! **Blind spots.** The scan is lexical: it does not see an item a macro writes (the
//! `with-prefix` of a macro-made method), an owner reached through a type alias, a
//! re-export, whether a `pub` type is reachable from the crate root, or what a type
//! is beyond its name's suffix (`node-ctor` reads the suffix, and `config-width` reads
//! the suffix of the node a config converts into or is passed to, matched by name
//! across the tree). `default-enum-arg` resolves a parameter's type name to its crate
//! from the file's `use` items (any form, `as` renames by local name), a qualified
//! path's first segment, or else the file's own crate; a glob import (`use x::*`) or
//! a re-export chain resolves to the importing path's crate only, and a type alias is
//! not seen.
//!
//! **Keys.** `<file>::<Owner>::<fn>`, or `<file>::<macro>` for `range-macro`. No line
//! numbers; items that share a key and a rule (an `impl Foo<f32>` and an
//! `impl Foo<f64>`) share one row.
//!
//! **Lists.** [`LIST`] holds `<crate>  <rule>  <key>` lines (`#` lines and blank lines
//! aside), today's violations by crate; its crate must be the key's. [`ALLOW`] holds
//! the types of another kind, one row per type or per `Type::fn` (a whole-module row only for an FFI
//! crate or a `testing.rs`), each with its reason; a violation a row covers is not
//! counted, and a row that covers no violation fails like a stale baseline row.

mod allow;
mod baseline;
mod scan;
#[cfg(test)]
mod tests;

use super::{Report, Tree};
use allow::allowed;
pub(super) use allow::{ALLOW, Allow};
pub(super) use baseline::pruned;
use baseline::{Row, read};
#[cfg(test)]
use scan::scan;
use scan::{Usual, parse_file, source_files};
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
    OptionArg,
    DefaultEnumArg,
    PubFieldConfig,
}

const RULES: [(Rule, &str); 10] = [
    (Rule::WithPrefix, "with-prefix"),
    (Rule::ChainResult, "chain-result"),
    (Rule::Setter, "setter"),
    (Rule::NodeCtor, "node-ctor"),
    (Rule::ConfigWidth, "config-width"),
    (Rule::CtorResult, "ctor-result"),
    (Rule::RangeMacro, "range-macro"),
    (Rule::OptionArg, "option-arg"),
    (Rule::DefaultEnumArg, "default-enum-arg"),
    (Rule::PubFieldConfig, "pub-field-config"),
];

impl Rule {
    fn name(self) -> &'static str {
        // `RULES` lists the variants in declaration order.
        RULES[self as usize].1
    }

    fn parse(name: &str) -> Option<Rule> {
        RULES.iter().find(|(_, n)| *n == name).map(|r| r.0)
    }
}

fn crate_of(path: &str) -> &str {
    path.split('/').nth(1).unwrap_or("")
}

/// The violations of the tree that `allow` does not cover.
fn found(tree: &dyn Tree, allow: &[Allow]) -> (BTreeSet<Row>, BTreeSet<usize>) {
    let mut sites = BTreeSet::new();
    let mut used = BTreeSet::new();
    let mut parsed = Vec::new();
    let mut nodes = BTreeSet::new();
    for path in source_files(tree) {
        let Some(src) = tree.read(&path) else {
            continue;
        };
        let p = parse_file(&path, &src);
        nodes.extend(p.nodes.iter().cloned());
        parsed.push((path, p));
    }
    let usual = Usual::of(parsed.iter().map(|(_, p)| p));
    for (path, p) in parsed {
        for s in p.finish(&nodes, &usual) {
            let covered: Vec<usize> = (0..allow.len())
                .filter(|&i| allowed(&path, &s, &allow[i]))
                .collect();
            if covered.is_empty() {
                sites.insert((crate_of(&path).to_owned(), s.rule, s.key));
            }
            used.extend(covered);
        }
    }
    (sites, used)
}

pub(super) fn check(tree: &dyn Tree, allow: &[Allow], min_sites: usize) -> Report {
    let mut r = Report::default();
    let rows = match read(tree) {
        Ok(rows) => rows,
        Err(r) => return r,
    };
    let (sites, used) = found(tree, allow);
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
            "check-building: the builders of another kind (ALLOW in xtask/src/gates/building/allow.rs):",
        );
        for a in allow {
            let owner = if a.owner.is_empty() { "*" } else { a.owner };
            r.say(format!("  {} {owner}: {}", a.path, a.reason));
        }
    }
    if !stale.is_empty() {
        r.fail(format!(
            "check-building: FAIL (a row of {LIST} names no violation; delete it, or run `cargo xtask prune-building`):"
        ));
        stale
            .into_iter()
            .for_each(|row| r.fail(format!("  {}", fmt(row))));
    }
    let idle: Vec<_> = (0..allow.len()).filter(|i| !used.contains(i)).collect();
    if !idle.is_empty() {
        r.fail("check-building: FAIL (a row of ALLOW covers no violation; delete it):");
        for i in idle {
            let a = &allow[i];
            let owner = if a.owner.is_empty() { "*" } else { a.owner };
            r.fail(format!("  {} {owner}: {}", a.path, a.reason));
        }
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
