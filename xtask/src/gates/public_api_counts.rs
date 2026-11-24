//! `public-api-counts`: the exact number of public items of each crate, a ratchet.
//!
//! Part of `cargo xtask doc`, not a gate: it needs the docs nightly and `cargo-public-api`.
//! A crate's count is the number of lines of `cargo public-api --all-features -s` for it,
//! one per item, method, trait impl and variant. `xtask/public-api-counts.txt` holds
//! `<crate>  <count>` rows: a crate above its row fails (`check-public-items` names the
//! items it added), a crate below its row fails until the row is lowered, and a crate
//! with no row or a row with no crate fails.

use std::collections::BTreeMap;

/// The counts file.
pub(crate) const LIST: &str = "xtask/public-api-counts.txt";

/// The crates no row holds, each with why: their code is compiled out on other hosts, so
/// their count would be a fact about the host. `xtask/public-items/` holds them.
pub(crate) const SKIPPED: &[(&str, &str)] = &[
    ("tutti-au-ffi", "compiled only on macOS"),
    ("tutti-coremidi-ffi", "compiled only on macOS"),
    ("tutti-winmm-ffi", "compiled only on Windows"),
];

/// Whether `krate` is a row of [`SKIPPED`].
pub(crate) fn skipped(krate: &str) -> bool {
    SKIPPED.iter().any(|(k, _)| *k == krate)
}

/// The items of a `public-api` text: its non-blank lines.
pub(crate) fn count(text: &str) -> usize {
    text.lines().filter(|l| !l.trim().is_empty()).count()
}

/// The counts file for `actual`, as [`check`] reads it.
pub(crate) fn render(actual: &BTreeMap<String, usize>) -> String {
    let mut out = String::from(
        "# The exact count of each crate's public API: the lines of `cargo public-api\n\
         # --all-features -s`, checked by `cargo xtask doc`. A crate above its row fails: remove\n\
         # items or raise the row in the same change, with the reason in the PR\n\
         # (`cargo xtask check-public-items` names the items). A crate below its row\n\
         # fails until the row is lowered, so the count only falls on its own.\n",
    );
    for (krate, why) in SKIPPED {
        out.push_str(&format!(
            "# {krate} has no row ({why}): `xtask/public-items/` holds its items.\n"
        ));
    }
    for (krate, n) in actual {
        out.push_str(&format!("{krate}  {n}\n"));
    }
    out
}

/// The findings for `actual` against the counts file `text` (`None` when absent).
pub(crate) fn check(text: Option<&str>, actual: &BTreeMap<String, usize>) -> Vec<String> {
    let Some(text) = text else {
        return vec![format!("public-api-counts: {LIST} is missing")];
    };
    let mut rows = BTreeMap::new();
    let mut out = Vec::new();
    for (at, line) in text.lines().enumerate().map(|(n, l)| (n + 1, l.trim())) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts[..] {
            [krate, n] if n.parse::<usize>().is_ok() => {
                if rows
                    .insert(krate, n.parse::<usize>().unwrap_or(0))
                    .is_some()
                {
                    out.push(format!(
                        "public-api-counts: {LIST}:{at}: `{krate}` is listed twice"
                    ));
                }
            }
            _ => out.push(format!(
                "public-api-counts: {LIST}:{at}: not `<crate>  <count>`: {line}"
            )),
        }
    }
    for (krate, &n) in actual {
        match rows.get(krate.as_str()) {
            None => out.push(format!(
                "public-api-counts: {krate}: {n} public items and no row in {LIST}: add `{krate}  {n}`"
            )),
            Some(&m) if n > m => out.push(format!(
                "public-api-counts: {krate}: {n} public items, the row says {m}: remove items or \
                 raise the row in the same change, with the reason in the PR (`cargo xtask \
                 check-public-items` names the items)"
            )),
            Some(&m) if n < m => out.push(format!(
                "public-api-counts: {krate}: {n} public items, the row says {m}: lower the row to {n}"
            )),
            Some(_) => {}
        }
    }
    for krate in rows.keys().filter(|k| !actual.contains_key(**k)) {
        if let Some((_, why)) = SKIPPED.iter().find(|(k, _)| k == krate) {
            out.push(format!(
                "public-api-counts: {krate}: this crate is not counted on this host ({why}): delete the row"
            ));
            continue;
        }
        out.push(format!(
            "public-api-counts: {krate}: a row in {LIST} and no crate: delete the row"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actual(rows: &[(&str, usize)]) -> BTreeMap<String, usize> {
        rows.iter().map(|(k, n)| ((*k).to_owned(), *n)).collect()
    }

    #[test]
    fn a_count_is_the_non_blank_lines() {
        assert_eq!(count("pub fn a()\n\npub struct B\n  \n"), 2);
    }

    #[test]
    fn counts_equal_to_their_rows_pass() {
        let a = actual(&[("a", 3), ("b", 0)]);
        assert_eq!(check(Some(&render(&a)), &a), Vec::<String>::new());
    }

    #[test]
    fn above_below_missing_and_stale_rows_fail_with_their_messages() {
        let text = "# c\nup  5\ndown  9\nold  1\n";
        let a = actual(&[("up", 6), ("down", 7), ("new", 2)]);
        assert_eq!(
            check(Some(text), &a),
            [
                "public-api-counts: down: 7 public items, the row says 9: lower the row to 7",
                "public-api-counts: new: 2 public items and no row in xtask/public-api-counts.txt: add `new  2`",
                "public-api-counts: up: 6 public items, the row says 5: remove items or raise the row \
                 in the same change, with the reason in the PR (`cargo xtask check-public-items` names the items)",
                "public-api-counts: old: a row in xtask/public-api-counts.txt and no crate: delete the row",
            ]
        );
    }

    #[test]
    fn a_skipped_crate_with_no_row_passes_and_with_a_row_fails() {
        let a = actual(&[("a", 1)]);
        assert_eq!(check(Some("a  1\n"), &a), Vec::<String>::new());
        assert_eq!(
            check(Some("a  1\ntutti-winmm-ffi  1\n"), &a),
            [
                "public-api-counts: tutti-winmm-ffi: this crate is not counted on this host \
              (compiled only on Windows): delete the row"
            ]
        );
        assert!(render(&a).contains("# tutti-au-ffi has no row (compiled only on macOS)"));
    }

    #[test]
    fn a_missing_or_malformed_file_fails() {
        assert_eq!(
            check(None, &actual(&[])),
            ["public-api-counts: xtask/public-api-counts.txt is missing"]
        );
        let r = check(Some("a  x\na  1\na  2\n"), &actual(&[("a", 2)]));
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(r[0].contains(":1: not `<crate>  <count>`"));
        assert!(r[1].contains("`a` is listed twice"));
    }
}
