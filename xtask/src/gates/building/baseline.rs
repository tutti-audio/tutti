//! The baseline list: its rows, and `prune-building`'s rewrite of it.

use super::super::ratchet;
use super::super::{Report, Tree};
use super::{Allow, LIST, Rule, found};
use std::collections::BTreeSet;

/// A baseline row: crate, rule, key.
pub(super) type Row = (String, Rule, String);

/// The baseline's rows, or a failing report.
pub(super) fn read(tree: &dyn Tree) -> Result<BTreeSet<Row>, Report> {
    let text = ratchet::text(tree, "check-building", LIST)?;
    parse(&text).map_err(|errors| ratchet::malformed("check-building", LIST, errors))
}

fn parse(text: &str) -> Result<BTreeSet<Row>, Vec<String>> {
    let mut rows = BTreeSet::new();
    ratchet::parse_rows(text, |at, line, errors| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [krate, rule, key] = parts[..] else {
            errors.push(format!(
                "  {LIST}:{at}: not `<crate>  <rule>  <key>`: {line}"
            ));
            return None;
        };
        let Some(rule) = Rule::parse(rule) else {
            errors.push(format!("  {LIST}:{at}: `{rule}` is not a rule"));
            return None;
        };
        if key.split('/').nth(1) != Some(krate) {
            errors.push(format!("  {LIST}:{at}: `{key}` is not in crate `{krate}`"));
        }
        if !rows.insert((krate.to_owned(), rule, key.to_owned())) {
            errors.push(format!("  {LIST}:{at}: `{line}` is listed twice"));
        }
        Some(())
    })?;
    Ok(rows)
}

/// The baseline with every row that names no violation deleted, and a crate's
/// `# <crate>` heading with it once its last row goes; the number of rows
/// deleted. What `cargo xtask prune-building` writes. It never adds a row: a new
/// violation is still `check`'s failure, so the list only shrinks.
pub(in crate::gates) fn pruned(
    tree: &dyn Tree,
    allow: &[Allow],
    min_sites: usize,
) -> Result<(String, usize), String> {
    let report_text = |r: Report| r.findings.join("\n");
    let text = ratchet::text(tree, "check-building", LIST).map_err(report_text)?;
    let rows =
        parse(&text).map_err(|e| report_text(ratchet::malformed("check-building", LIST, e)))?;
    let (sites, _) = found(tree, allow);
    if sites.len() < min_sites {
        return Err(format!(
            "{} violations found, at least {min_sites} expected; the walk is broken",
            sites.len()
        ));
    }
    let crates: BTreeSet<String> = rows.iter().map(|(c, _, _)| format!("# {c}")).collect();
    let is_row = |line: &str| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with('#')
    };
    let mut removed = 0;
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        if is_row(line) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let row =
                Rule::parse(parts[1]).map(|rule| (parts[0].to_owned(), rule, parts[2].to_owned()));
            if row.is_some_and(|row| !sites.contains(&row)) {
                removed += 1;
                continue;
            }
        }
        kept.push(line);
    }
    let mut out: Vec<&str> = Vec::new();
    for (i, &line) in kept.iter().enumerate() {
        if crates.contains(line.trim()) {
            let empty = !kept[i + 1..]
                .iter()
                .take_while(|l| !l.trim().is_empty() && !crates.contains(l.trim()))
                .any(|l| is_row(l));
            if empty {
                if out.last().is_some_and(|l| l.trim().is_empty()) {
                    out.pop();
                }
                continue;
            }
        }
        out.push(line);
    }
    let mut text = out.join("\n");
    text.push('\n');
    Ok((text, removed))
}
