//! `unused-public --app`: of the items the umbrella `tutti` names, which does nothing
//! outside the library exercise?
//!
//! Mode 1 hides exactly these items; this lists them, in three groups. The references
//! of an item (from the same index, minus the definition, an `impl` header's mention
//! and a `pub use` statement) are told apart by where they sit:
//!
//! - **app**: a file under `crates/tutti/`, under `testing/tutti-book/` (the book's
//!   programs; the chapters in `book/` are Markdown the index does not read), or under
//!   any `examples/` directory;
//! - **own tests** and **own production**: the item's crate, test code or not;
//! - **other tests**: another crate's test code, or `testing/` outside the book;
//! - **sibling production**: another crate's non-test code under `crates/`.

use super::named::{Corpus, Hit};
use super::{Gathered, Item, Kind, Reach, crate_of, gather};
use crate::gates::Tree;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Where a reference sits, relative to the item's crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Use {
    App,
    OwnTest,
    OtherTest,
    Sibling,
    OwnProduction,
}

/// The kind of reference `path` is to an item of crate `own`.
pub(super) fn use_of(path: &str, own: &str, test: bool) -> Use {
    let app = path.starts_with("crates/tutti/")
        || path.starts_with("testing/tutti-book/")
        || path.split('/').any(|c| c == "examples");
    match (app, crate_of(path) == Some(own), test) {
        (true, ..) => Use::App,
        (_, true, true) => Use::OwnTest,
        (_, true, false) => Use::OwnProduction,
        (_, false, true) => Use::OtherTest,
        (_, false, false) if crate_of(path).is_some() => Use::Sibling,
        _ => Use::OtherTest,
    }
}

/// How an item no app exercises is exercised, if at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Group {
    Nothing,
    Documented,
    OnlyTests,
    OwnProduction,
    Sibling,
}

impl Group {
    fn word(self) -> &'static str {
        match self {
            Group::Nothing => "exercised-by-nothing",
            Group::Documented => "documented-and-otherwise-unexercised",
            Group::OnlyTests => "exercised-only-by-tests",
            Group::OwnProduction => "used-only-inside-its-crate",
            Group::Sibling => "public-for-a-sibling",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) own_tests: usize,
    pub(super) other_tests: usize,
    pub(super) sibling: usize,
    pub(super) own_production: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    pub(super) krate: String,
    pub(super) file: String,
    pub(super) line: usize,
    pub(super) kind: Kind,
    pub(super) owner: Option<String>,
    pub(super) name: String,
    pub(super) group: Group,
    pub(super) counts: Counts,
    pub(super) doc: Option<Hit>,
    pub(super) macro_named: bool,
}

/// The items of `items` no app exercises, each with its group; an item with any
/// app-side reference is left out.
pub(super) fn classify(
    items: &[Item],
    is_test: &dyn Fn(&str, usize) -> bool,
    is_reexport: &dyn Fn(&str, usize) -> bool,
    corpus: &Corpus,
) -> Vec<Row> {
    let mut rows = Vec::new();
    'items: for it in items {
        let mut c = Counts::default();
        for (f, l) in it.refs.iter().filter(|(f, l)| !is_reexport(f, *l)) {
            match use_of(f, &it.krate, is_test(f, *l)) {
                Use::App => continue 'items,
                Use::OwnTest => c.own_tests += 1,
                Use::OtherTest => c.other_tests += 1,
                Use::Sibling => c.sibling += 1,
                Use::OwnProduction => c.own_production += 1,
            }
        }
        let doc = corpus.doc_example(it);
        let unused = c == Counts::default();
        let group = if c.sibling > 0 {
            Group::Sibling
        } else if unused && doc.is_some() {
            Group::Documented
        } else if unused {
            Group::Nothing
        } else if c.own_production > 0 {
            Group::OwnProduction
        } else {
            Group::OnlyTests
        };
        rows.push(Row {
            krate: it.krate.clone(),
            file: it.file.clone(),
            line: it.line,
            kind: it.kind,
            owner: it.owner.clone(),
            name: it.name.clone(),
            group,
            counts: c,
            macro_named: unused && corpus.in_macro(it),
            doc,
        });
    }
    rows.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    rows
}

/// The groups, in report order.
const GROUPS: [Group; 5] = [
    Group::Nothing,
    Group::Documented,
    Group::OnlyTests,
    Group::OwnProduction,
    Group::Sibling,
];

/// The heading of a type: `Type  (N public methods, k listed: a / b / c / d / e)`, the
/// five figures being the listed methods in each group, in report order.
pub(super) fn heading(owner: &str, methods: usize, listed: &[Group]) -> String {
    let per: Vec<String> = GROUPS
        .iter()
        .map(|g| listed.iter().filter(|x| *x == g).count().to_string())
        .collect();
    format!(
        "{owner}  ({methods} public methods, {} listed: {})",
        listed.len(),
        per.join(" / ")
    )
}

fn row_line(r: &Row, typed: bool) -> String {
    let c = r.counts;
    let name = if typed {
        r.name.clone()
    } else {
        format!("{} {}", r.kind.word(), r.name)
    };
    let mut note = String::new();
    if r.macro_named {
        note.push_str("  named in a macro");
    }
    if let Some((f, l)) = &r.doc {
        note.push_str(&format!("  named in a doc example ({f}:{l})"));
    }
    format!(
        "{}:{}  {name}  {}  refs: own-tests {}, other-tests {}, sibling-production {}, own-production {}{note}",
        r.file,
        r.line,
        r.group.word(),
        c.own_tests,
        c.other_tests,
        c.sibling,
        c.own_production
    )
}

/// One crate's section: its types by listed methods, descending, then the rest.
pub(super) fn section(
    krate: &str,
    rows: &[Row],
    methods: &BTreeMap<(String, String), usize>,
) -> String {
    let mut out = String::new();
    let mut by_type: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    let mut free: Vec<&Row> = Vec::new();
    for r in rows {
        match &r.owner {
            Some(t) => by_type.entry(t).or_default().push(r),
            None => free.push(r),
        }
    }
    let mut types: Vec<_> = by_type.into_iter().collect();
    types.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    let _ = writeln!(out, "{krate}: {} items", rows.len());
    for (t, rs) in &types {
        let n = methods
            .get(&(krate.to_owned(), (*t).to_owned()))
            .copied()
            .unwrap_or(0);
        let _ = writeln!(
            out,
            "{}",
            heading(t, n, &rs.iter().map(|r| r.group).collect::<Vec<_>>())
        );
        for r in rs {
            let _ = writeln!(out, "  {}", row_line(r, true));
        }
    }
    if !free.is_empty() {
        let _ = writeln!(out, "free functions and types");
        for r in &free {
            let _ = writeln!(out, "  {}", row_line(r, false));
        }
    }
    let count = |g: Group| rows.iter().filter(|r| r.group == g).count();
    let _ = writeln!(
        out,
        "{krate}: {} listed ({} exercised by nothing, {} documented and otherwise unexercised, {} exercised only by tests, {} used only inside its crate, {} public for a sibling)\n",
        rows.len(),
        count(Group::Nothing),
        count(Group::Documented),
        count(Group::OnlyTests),
        count(Group::OwnProduction),
        count(Group::Sibling)
    );
    out
}

/// The `--app` report over `index` for `crates`.
pub(crate) fn app_report(
    tree: &dyn Tree,
    index: &[u8],
    crates: &[String],
    umbrella: Option<&str>,
) -> Result<String, String> {
    let Gathered {
        items,
        reach,
        sources,
        methods,
        ..
    } = gather(tree, index, crates, umbrella)?;
    let corpus = Corpus::read(tree);
    let Some(reach) = reach else {
        return Err("`--app` needs the umbrella's public API: cargo-public-api did not run".into());
    };
    let named = |i: &Item| Reach::names(&reach, &i.krate, i.owner.as_deref().unwrap_or(&i.name));
    let population: Vec<Item> = items.into_iter().filter(named).collect();
    let is_test = |f: &str, l: usize| sources.is_test(f, l);
    let is_reexport = |f: &str, l: usize| sources.is_reexport(f, l);
    let mut out = String::from(HEADER);
    out.push('\n');
    let mut totals = [0usize; 5];
    for krate in crates {
        let mine: Vec<Item> = population
            .iter()
            .filter(|i| &i.krate == krate)
            .cloned()
            .collect();
        let rows = classify(&mine, &is_test, &is_reexport, &corpus);
        for r in &rows {
            totals[r.group as usize] += 1;
        }
        out.push_str(&section(krate, &rows, &methods));
    }
    let _ = writeln!(
        out,
        "total: {} listed ({} exercised by nothing, {} documented and otherwise unexercised, {} exercised only by tests, {} used only inside its crate, {} public for a sibling)",
        totals.iter().sum::<usize>(),
        totals[0],
        totals[1],
        totals[2],
        totals[3],
        totals[4]
    );
    Ok(out)
}

const HEADER: &str = "unused-public --app: items the umbrella `tutti` names that nothing outside the library exercises\n\
A list to cut from, for the owner to decide item by item; not a proof. The population is every pub fn,\n\
method, struct, enum, trait, type, const or static the umbrella names (trait members and trait impls are\n\
not listed). References come from rust-analyzer's SCIP index (every cargo feature on), minus the\n\
definition, an `impl` header's mention of its type and a `pub use` statement. An item with a reference\n\
from crates/tutti/, testing/tutti-book/ or any examples/ directory is not listed.\n\
Group exercised-by-nothing: no reference of any other kind (a row may carry `named in a macro`, see below).\n\
Group documented-and-otherwise-unexercised: no reference, but a doc example names it (by name).\n\
Group exercised-only-by-tests: at least one test reference (its own crate's or another's), no production\n\
  reference from its own crate or a sibling: public items only tests call.\n\
Group used-only-inside-its-crate: its own crate's production code references it (tests or not), no sibling\n\
  crate's production code does: works for the library, but no app-side code, example or book example calls it.\n\
Group public-for-a-sibling: referenced by another crate's production code and by no app-side code\n\
  (a candidate for a narrower door, not for deletion).\n\
Types sort by listed methods, descending; `N public methods` is the scan's count of the type's pub methods,\n\
merged by type name within a crate; `k listed: a / b / c / d / e` is its listed methods in each group, in the\n\
order above (nothing / documented / only tests / inside its crate / sibling).\n\
Doc examples (``` blocks in /// and //! comments) and the book's Markdown chapters are not in the index; they\n\
are searched by name: `.name(` or `Type::name`, `name(`, the bare name; it can over-label. A `macro_rules!`\n\
body naming an unreferenced item labels it `named in a macro`. A row is not safe to delete until\n\
`cargo test --doc --workspace` passes without the item.\n\
It cannot see: an item called only from a macro expansion (`Sweep::new` in `declare_params!`; see the label);\n\
an app outside this repository; use through a trait object or a macro expansion;\n\
cfg'd-out platforms (macOS and Windows code is not indexed on Linux); the Reach match is by crate\n\
and name, so a name re-exported through another crate under a disabled feature may be missed.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, owner: Option<&str>, refs: &[&str]) -> Item {
        Item {
            krate: "c".to_owned(),
            file: "crates/c/src/lib.rs".to_owned(),
            line: 1,
            kind: Kind::Fn,
            owner: owner.map(str::to_owned),
            name: name.to_owned(),
            in_macro: false,
            refs: refs.iter().map(|f| ((*f).to_owned(), 1)).collect(),
        }
    }

    fn run(items: &[Item]) -> Vec<Group> {
        let test = |f: &str, _: usize| f.contains("/tests/");
        classify(items, &test, &|_, _| false, &Corpus::default())
            .iter()
            .map(|r| r.group)
            .collect()
    }

    #[test]
    fn each_group_is_decided_by_where_the_references_sit() {
        let nothing = item("a", None, &[]);
        let tests = item("b", None, &["crates/c/tests/t.rs", "crates/d/tests/t.rs"]);
        let own = item("e", None, &["crates/c/tests/t.rs", "crates/c/src/x.rs"]);
        let sibling = item("c", None, &["crates/d/src/x.rs", "crates/c/tests/t.rs"]);
        assert_eq!(
            run(&[nothing, tests, own, sibling]),
            [
                Group::Nothing,
                Group::OnlyTests,
                Group::OwnProduction,
                Group::Sibling
            ]
        );
    }

    #[test]
    fn an_item_only_a_doc_example_uses_lands_in_the_documented_group() {
        let corpus = Corpus::from_texts(&[(
            "crates/c/src/lib.rs",
            "//! ```\n//! let r = s.a();\n//! ```\n",
        )]);
        let it = item("a", Some("S"), &[]);
        let rows = classify(&[it], &|_, _| false, &|_, _| false, &corpus);
        assert_eq!(rows[0].group, Group::Documented);
        assert_eq!(rows[0].doc, Some(("crates/c/src/lib.rs".to_owned(), 2)));
    }

    #[test]
    fn an_app_side_reference_keeps_an_item_off_the_list() {
        for app in [
            "crates/tutti/tests/a.rs",
            "testing/tutti-book/examples/b.rs",
            "crates/d/examples/c.rs",
        ] {
            assert!(run(&[item("a", None, &[app, "crates/d/tests/t.rs"])]).is_empty());
        }
    }

    #[test]
    fn another_crates_tests_are_not_a_sibling_use_and_a_reexport_is_no_use() {
        let it = item(
            "a",
            None,
            &["crates/d/tests/t.rs", "crates/tutti/src/lib.rs"],
        );
        let test = |f: &str, _: usize| f.contains("/tests/");
        let rows = classify(
            &[it],
            &test,
            &|f, _| f.ends_with("lib.rs"),
            &Corpus::default(),
        );
        assert_eq!(rows[0].group, Group::OnlyTests);
        assert_eq!(rows[0].counts.other_tests, 1);
    }

    #[test]
    fn a_type_heading_counts_its_methods_and_its_listed_ones_and_sorts_by_listed() {
        assert_eq!(
            heading(
                "Editor",
                40,
                &[Group::Nothing, Group::OnlyTests, Group::OnlyTests]
            ),
            "Editor  (40 public methods, 3 listed: 1 / 0 / 2 / 0 / 0)"
        );
        let rows = classify(
            &[
                item("m", Some("Small"), &[]),
                item("x", Some("Big"), &[]),
                item("y", Some("Big"), &[]),
            ],
            &|_, _| false,
            &|_, _| false,
            &Corpus::default(),
        );
        let methods = BTreeMap::from([(("c".to_owned(), "Big".to_owned()), 40)]);
        let text = section("c", &rows, &methods);
        let big = text
            .find("Big  (40 public methods, 2 listed: 2 / 0 / 0 / 0 / 0)")
            .unwrap();
        let small = text
            .find("Small  (0 public methods, 1 listed: 1 / 0 / 0 / 0 / 0)")
            .unwrap();
        assert!(big < small);
    }

    #[test]
    fn use_of_tells_the_crates_apart() {
        assert_eq!(use_of("crates/c/src/a.rs", "c", false), Use::OwnProduction);
        assert_eq!(use_of("crates/d/src/a.rs", "c", false), Use::Sibling);
        assert_eq!(
            use_of("testing/tutti-fixtures/src/a.rs", "c", true),
            Use::OtherTest
        );
    }
}
