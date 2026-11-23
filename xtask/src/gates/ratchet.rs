//! What the baseline-backed gates share.
//!
//! A counting gate finds sites in each file. A baseline file holds one
//! `<count>.. <path>` line per file that held sites when the gate began (one
//! count per kind of site; a file not listed has 0). A file fails when it holds
//! more sites of a kind than its baseline; a file below its baseline passes and
//! the report says how far the baseline can be lowered. A missing baseline
//! fails.
//!
//! A row-list gate (`<key>  <reason>` rows that cover violations one for one)
//! reads its file through [`read_rows`]: comments and blank lines skipped, a
//! missing or malformed file a failing report.

use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// A file's baseline: path to one count per kind of site.
pub(super) type Baseline<const N: usize> = BTreeMap<String, [usize; N]>;

/// The sites one file holds, each a printable line, per kind.
pub(super) struct Found<const N: usize> {
    pub(super) path: String,
    pub(super) sites: [Vec<String>; N],
}

/// What one gate prints around its ratchet.
pub(super) struct Wording<'a, const N: usize> {
    /// The gate's recipe name, the prefix of every line.
    pub(super) gate: &'a str,
    /// The baseline file's repo path.
    pub(super) baseline: &'a str,
    /// What each kind of site is, plural, in the per-file line (`unresolved names`).
    pub(super) nouns: [&'a str; N],
    /// The failure headline's remainder, after `FAIL (`.
    pub(super) headline: &'a str,
    /// Whether a failure or slack prints the baseline lines of the current counts.
    pub(super) paste: bool,
}

fn failing(gate: &str, why: String) -> Report {
    let mut r = Report::default();
    r.fail(format!("{gate}: FAIL: {why}"));
    r
}

/// The non-blank, non-`#` lines of `text`, trimmed, with their 1-based line numbers.
pub(super) fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(n, l)| (n + 1, l.trim()))
        .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
}

/// One row per line of `text` through `row` (given the line number, the line
/// and the error list a refused line adds to); any error fails the list.
pub(super) fn parse_rows<T>(
    text: &str,
    mut row: impl FnMut(usize, &str, &mut Vec<String>) -> Option<T>,
) -> Result<Vec<T>, Vec<String>> {
    let (mut rows, mut errors) = (Vec::new(), Vec::new());
    for (at, line) in lines(text) {
        rows.extend(row(at, line, &mut errors));
    }
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

/// The text of the list at `path`, or a failing report: the file is missing.
pub(super) fn text(tree: &dyn Tree, gate: &str, path: &str) -> Result<String, Report> {
    tree.read(path)
        .ok_or_else(|| failing(gate, format!("{path} is missing")))
}

/// The failing report for a list at `path` whose lines `errors` name.
pub(super) fn malformed(gate: &str, path: &str, errors: Vec<String>) -> Report {
    let mut r = Report::default();
    r.fail(format!("{gate}: FAIL ({path} is malformed):"));
    errors.into_iter().for_each(|e| r.fail(e));
    r
}

/// The rows of the list at `path`, or a failing report: the file is missing, or
/// `row` refuses some line.
pub(super) fn read_rows<T>(
    tree: &dyn Tree,
    gate: &str,
    path: &str,
    row: impl FnMut(usize, &str, &mut Vec<String>) -> Option<T>,
) -> Result<Vec<T>, Report> {
    parse_rows(&text(tree, gate, path)?, row).map_err(|errors| malformed(gate, path, errors))
}

/// `<count>.. <path>` lines: `N` counts, then the path (which may hold spaces).
pub(super) fn parse<const N: usize>(path: &str, text: &str) -> Result<Baseline<N>, String> {
    let mut map = Baseline::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let bad = || {
            format!(
                "{path}:{}: not `{}<path>`: {line}",
                n + 1,
                "<count> ".repeat(N)
            )
        };
        let mut it = line.splitn(N + 1, ' ');
        let mut counts = [0; N];
        for c in &mut counts {
            *c = it.next().and_then(|c| c.parse().ok()).ok_or_else(bad)?;
        }
        let file = it.next().map(str::trim).filter(|p| !p.is_empty());
        map.insert(file.ok_or_else(bad)?.to_owned(), counts);
    }
    Ok(map)
}

/// The baseline `w.baseline` holds, or a failing report that says why not.
pub(super) fn load<const N: usize>(tree: &dyn Tree, w: &Wording<N>) -> Result<Baseline<N>, Report> {
    let text = text(tree, w.gate, w.baseline)?;
    parse(w.baseline, &text).map_err(|e| failing(w.gate, e))
}

/// Settles `found` (one entry per file, in path order) against `baseline`.
pub(super) fn settle<const N: usize>(
    w: &Wording<N>,
    baseline: &Baseline<N>,
    found: Vec<Found<N>>,
) -> Report {
    let mut r = Report::default();
    let mut over = Vec::new();
    let mut slack = 0;
    let mut seen = BTreeSet::new();
    let mut total = [0; N];
    let mut current = Vec::new();
    for f in found {
        let allowed = baseline.get(&f.path).copied().unwrap_or([0; N]);
        let counts: [usize; N] = std::array::from_fn(|k| f.sites[k].len());
        seen.insert(f.path.clone());
        for (k, sites) in f.sites.into_iter().enumerate() {
            total[k] += counts[k];
            if counts[k] > allowed[k] {
                over.push(format!(
                    "  {}: {} {}, baseline {}",
                    f.path, counts[k], w.nouns[k], allowed[k]
                ));
                over.extend(sites);
            } else {
                slack += allowed[k] - counts[k];
            }
        }
        if counts != [0; N] {
            current.push((f.path, counts));
        }
    }
    let gone: Vec<(&String, &[usize; N])> = baseline
        .iter()
        .filter(|(p, _)| !seen.contains(*p))
        .collect();
    slack += gone.iter().flat_map(|(_, n)| n.iter()).sum::<usize>();
    if over.is_empty() {
        r.say(format!("{}: OK", w.gate));
    } else {
        r.fail(format!("{}: FAIL ({}):", w.gate, w.headline));
        for line in over {
            r.fail(line);
        }
    }
    for (p, _) in &gone {
        r.say(format!(
            "{}: {} names {p}, which no longer exists; remove the row",
            w.gate, w.baseline
        ));
    }
    let base: [usize; N] = std::array::from_fn(|k| baseline.values().map(|c| c[k]).sum());
    let say = |counts: &[usize; N], nouns: bool| -> String {
        counts
            .iter()
            .zip(&w.nouns)
            .map(|(c, noun)| {
                if nouns {
                    format!("{c} {noun}")
                } else {
                    c.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" and ")
    };
    r.say(format!(
        "{}: {} in {} baseline files ({} total in {})",
        w.gate,
        say(&total, true),
        baseline.len(),
        say(&base, false),
        w.baseline
    ));
    if slack > 0 {
        r.say(format!(
            "{}: {} can drop by {slack}; lower it to the current counts",
            w.gate, w.baseline
        ));
    }
    if w.paste && (slack > 0 || !r.passed()) {
        r.say(format!(
            "{}: {} for the current counts:",
            w.gate, w.baseline
        ));
        for (path, c) in current {
            let counts: Vec<String> = c.iter().map(usize::to_string).collect();
            r.say(format!("{} {path}", counts.join(" ")));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const W: Wording<1> = Wording {
        gate: "check-x",
        baseline: "xtask/x.txt",
        nouns: ["sites"],
        headline: "a file gained a site",
        paste: false,
    };

    const W2: Wording<2> = Wording {
        gate: "check-y",
        baseline: "xtask/y.txt",
        nouns: ["compares", "sleeps"],
        headline: "a file gained a site",
        paste: true,
    };

    fn found(path: &str, n: usize) -> Found<1> {
        Found {
            path: path.to_owned(),
            sites: [(1..=n).map(|i| format!("    {path}:{i}: site")).collect()],
        }
    }

    fn found2(path: &str, a: usize, b: usize) -> Found<2> {
        Found {
            path: path.to_owned(),
            sites: [
                (1..=a)
                    .map(|i| format!("    {path}:{i}: compare"))
                    .collect(),
                (1..=b).map(|i| format!("    {path}:{i}: sleep")).collect(),
            ],
        }
    }

    #[test]
    fn a_file_over_its_baseline_fails_and_lists_its_sites() {
        let base = parse("b", "1 a.rs\n").unwrap();
        let r = settle(&W, &base, vec![found("a.rs", 2), found("b.rs", 0)]);
        assert_eq!(
            r.findings,
            [
                "check-x: FAIL (a file gained a site):",
                "  a.rs: 2 sites, baseline 1",
                "    a.rs:1: site",
                "    a.rs:2: site",
            ]
        );
    }

    #[test]
    fn a_file_under_its_baseline_passes_and_the_slack_is_reported() {
        let base = parse("b", "3 a.rs\n2 gone.rs\n").unwrap();
        let r = settle(&W, &base, vec![found("a.rs", 1)]);
        assert!(r.passed());
        assert_eq!(
            r.out,
            [
                "check-x: OK",
                "check-x: xtask/x.txt names gone.rs, which no longer exists; remove the row",
                "check-x: 1 sites in 2 baseline files (5 total in xtask/x.txt)",
                "check-x: xtask/x.txt can drop by 4; lower it to the current counts",
            ]
        );
    }

    #[test]
    fn a_file_missing_from_the_baseline_has_a_baseline_of_zero() {
        let r = settle(&W, &Baseline::new(), vec![found("new.rs", 1)]);
        assert!(!r.passed());
        assert_eq!(r.findings[1], "  new.rs: 1 sites, baseline 0");
    }

    #[test]
    fn a_missing_baseline_fails_naming_the_file() {
        let r = load(&Mem::default(), &W).unwrap_err();
        assert_eq!(r.findings, ["check-x: FAIL: xtask/x.txt is missing"]);
    }

    #[test]
    fn a_malformed_baseline_line_is_an_error() {
        assert!(parse::<1>("b", "many a.rs\n").is_err());
        assert!(parse::<1>("b", "3\n").is_err());
        assert!(parse::<2>("b", "3 a.rs\n").is_err());
        assert_eq!(parse::<1>("b", "\n3 a b.rs\n").unwrap()["a b.rs"], [3]);
        let mem = Mem::with(&[("xtask/x.txt", "many a.rs\n")]);
        assert!(!load(&mem, &W).unwrap_err().passed());
    }

    #[test]
    fn each_kind_of_site_is_held_to_its_own_column() {
        let base = parse("b", "1 2 a.rs\n").unwrap();
        assert_eq!(base["a.rs"], [1, 2]);
        let r = settle(&W2, &base, vec![found2("a.rs", 0, 3)]);
        assert_eq!(
            r.findings,
            [
                "check-y: FAIL (a file gained a site):",
                "  a.rs: 3 sleeps, baseline 2",
                "    a.rs:1: sleep",
                "    a.rs:2: sleep",
                "    a.rs:3: sleep",
            ]
        );
        let r = settle(&W2, &base, vec![found2("a.rs", 1, 1)]);
        assert!(r.passed());
        assert_eq!(
            r.out[1..],
            [
                "check-y: 1 compares and 1 sleeps in 1 baseline files (1 and 2 total in xtask/y.txt)",
                "check-y: xtask/y.txt can drop by 1; lower it to the current counts",
                "check-y: xtask/y.txt for the current counts:",
                "1 1 a.rs",
            ]
        );
    }

    #[test]
    fn a_row_list_skips_comments_and_names_every_refused_line() {
        let text = "# header\n\nok  one\nbad\nok  two\nbad2\n";
        let row = |at: usize, l: &str, errors: &mut Vec<String>| {
            let ok = l.strip_prefix("ok  ").map(str::to_owned);
            if ok.is_none() {
                errors.push(format!("  list:{at}: `{l}`"));
            }
            ok
        };
        assert_eq!(
            parse_rows(text, row).unwrap_err(),
            ["  list:4: `bad`", "  list:6: `bad2`"]
        );
        assert_eq!(parse_rows("# x\nok  a\n", row).unwrap(), ["a"]);
        let r = read_rows(&Mem::default(), "check-z", "xtask/z.txt", row).unwrap_err();
        assert_eq!(r.findings, ["check-z: FAIL: xtask/z.txt is missing"]);
        let mem = Mem::with(&[("xtask/z.txt", text)]);
        let r = read_rows(&mem, "check-z", "xtask/z.txt", row).unwrap_err();
        assert_eq!(r.findings[0], "check-z: FAIL (xtask/z.txt is malformed):");
        assert_eq!(r.findings.len(), 3);
    }
}
