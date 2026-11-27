//! `public-api` counts: the exact number of public items of each crate, printed by
//! `cargo xtask doc` for a reader who wants it.
//!
//! Part of `doc`, not a gate: it needs the docs nightly and `cargo-public-api`. A crate's
//! count is the number of lines of `cargo public-api --all-features -s` for it, one per
//! item, method, derive and trait impl line. It is a report, not a ratchet: two pull
//! requests that each add an item to a crate pass alone and would fail together on the
//! merged count, and an auto-trait or derive line moves it with no item added.
//! `check-public-items` is the ratchet.

use std::collections::BTreeMap;

/// The crates the table leaves out, each with why: their code is compiled out on other
/// hosts, so their count would be a fact about the host. `xtask/public-items/` lists them.
pub(crate) const SKIPPED: &[(&str, &str)] = &[
    ("tutti-au-ffi", "compiled only on macOS"),
    ("tutti-coremidi-ffi", "compiled only on macOS"),
    ("tutti-winmm-ffi", "compiled only on Windows"),
    ("tutti-winmidi2-ffi", "compiled only on Windows"),
];

/// Whether `krate` is a row of [`SKIPPED`].
pub(crate) fn skipped(krate: &str) -> bool {
    SKIPPED.iter().any(|(k, _)| *k == krate)
}

/// The items of a `public-api` text: its non-blank lines.
pub(crate) fn count(text: &str) -> usize {
    text.lines().filter(|l| !l.trim().is_empty()).count()
}

/// The table `doc` prints: a line per crate, sorted, the total, and the skipped crates.
pub(crate) fn table(actual: &BTreeMap<String, usize>) -> Vec<String> {
    let mut out: Vec<String> = actual
        .iter()
        .map(|(krate, n)| format!("public-api: {krate}  {n}"))
        .collect();
    out.push(format!(
        "public-api: total  {}",
        actual.values().sum::<usize>()
    ));
    let left: Vec<String> = SKIPPED
        .iter()
        .map(|(k, why)| format!("{k} ({why})"))
        .collect();
    out.push(format!(
        "public-api: not counted on this host: {} (`xtask/public-items/` lists them)",
        left.join(", ")
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_is_the_non_blank_lines() {
        assert_eq!(count("pub fn a()\n\npub struct B\n  \n"), 2);
    }

    #[test]
    fn the_table_is_sorted_with_a_total_and_the_skipped_crates_with_their_reasons() {
        let actual: BTreeMap<String, usize> = [("b".to_owned(), 5), ("a".to_owned(), 2)]
            .into_iter()
            .collect();
        assert_eq!(
            table(&actual),
            [
                "public-api: a  2",
                "public-api: b  5",
                "public-api: total  7",
                "public-api: not counted on this host: tutti-au-ffi (compiled only on macOS), \
                 tutti-coremidi-ffi (compiled only on macOS), tutti-winmm-ffi (compiled only on Windows), \
                 tutti-winmidi2-ffi (compiled only on Windows) \
                 (`xtask/public-items/` lists them)",
            ]
        );
    }

    #[test]
    fn a_skipped_crate_is_one_of_the_table_rows() {
        assert!(skipped("tutti-winmm-ffi"));
        assert!(!skipped("tutti-rt"));
    }
}
