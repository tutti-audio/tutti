//! Discovery decisions as pure functions over values.
//!
//! The scan reads the filesystem once per candidate into a [`FileFacts`]
//! (`FileFacts::read`, in `fs.rs`); everything here decides from that value and
//! a table row, touching no disk and no clock.

use crate::table::FileRow;
use std::time::SystemTime;
use tutti_plugin_types::PluginFormat;

/// What the scan found on disk for one candidate, read once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    /// Whether the path exists.
    pub exists: bool,
    /// The file's last modification, or `None` if it cannot be read or lies
    /// before the Unix epoch. A file without a time is always rescanned.
    pub mtime: Option<SystemTime>,
    /// The format the path's extension names.
    pub format: Option<PluginFormat>,
}

/// What a scan does with one discovered plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ScanDecision {
    /// Blacklisted, and the file has not changed since: skip it.
    Blacklisted,
    /// Recorded, and the file has not changed since: skip it.
    UpToDate,
    /// New or changed: probe it.
    Probe,
}

/// Decides what a scan does with a candidate, given its row (if any) and its
/// facts.
///
/// The blacklist is mtime-aware: a blacklisted file whose time changed on disk
/// (a reinstall, a vendor update) is probed again rather than skipped forever.
#[must_use]
pub(crate) fn classify(row: Option<&FileRow>, facts: &FileFacts) -> ScanDecision {
    match row {
        Some(r) if is_blacklisted_and_unchanged(r, facts) => ScanDecision::Blacklisted,
        Some(r) if !needs_rescan(r, facts) => ScanDecision::UpToDate,
        _ => ScanDecision::Probe,
    }
}

/// Returns `true` unless the file's modification time equals the one the row
/// stamped. A time missing on either side is never equal, so a file or a row
/// without one is always rescanned.
#[must_use]
pub(crate) fn needs_rescan(row: &FileRow, facts: &FileFacts) -> bool {
    match (row.mtime, facts.mtime) {
        (Some(stamped), Some(read)) => stamped != read,
        _ => true,
    }
}

/// Returns `true` if the row is blacklisted and its file has not changed since
/// the blacklist was stamped.
#[must_use]
pub(crate) fn is_blacklisted_and_unchanged(row: &FileRow, facts: &FileFacts) -> bool {
    row.blacklist_reason().is_some() && !needs_rescan(row, facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::{PluginClass, PluginDescriptor};

    use std::time::Duration;

    const T: u64 = 1_700_000_000;

    fn at(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn facts(mtime: Option<SystemTime>) -> FileFacts {
        FileFacts {
            exists: true,
            mtime,
            format: Some(PluginFormat::Clap),
        }
    }

    fn healthy(mtime: Option<SystemTime>) -> FileRow {
        FileRow::probed(
            PluginFormat::Clap,
            &facts(mtime),
            vec![PluginDescriptor::new("id", "name", PluginClass::Unknown)],
        )
    }

    fn bad(mtime: Option<SystemTime>) -> FileRow {
        FileRow::blacklisted("crashed".into(), &facts(mtime))
    }

    #[test]
    fn an_unknown_plugin_is_probed() {
        assert_eq!(classify(None, &facts(at(T))), ScanDecision::Probe);
    }

    #[test]
    fn a_changed_file_is_probed_whichever_way_the_time_moved() {
        let r = healthy(at(T));
        assert_eq!(classify(Some(&r), &facts(at(T + 1))), ScanDecision::Probe);
        assert_eq!(classify(Some(&r), &facts(at(T - 1))), ScanDecision::Probe);
    }

    #[test]
    fn an_unchanged_row_is_up_to_date() {
        let r = healthy(at(T));
        assert_eq!(classify(Some(&r), &facts(at(T))), ScanDecision::UpToDate);
    }

    #[test]
    fn an_unchanged_blacklisted_row_is_skipped_as_blacklisted() {
        let r = bad(at(T));
        assert_eq!(classify(Some(&r), &facts(at(T))), ScanDecision::Blacklisted);
    }

    #[test]
    fn a_changed_blacklisted_file_is_readmitted() {
        let r = bad(at(T));
        assert_eq!(classify(Some(&r), &facts(at(T + 5))), ScanDecision::Probe);
        assert!(!is_blacklisted_and_unchanged(&r, &facts(at(T + 5))));
    }

    #[test]
    fn a_healthy_unchanged_row_is_not_blacklisted() {
        let r = healthy(at(T));
        assert!(!is_blacklisted_and_unchanged(&r, &facts(at(T))));
    }

    /// A time missing on either side is never equal: the file is rescanned,
    /// and a blacklist stamped without a time never holds.
    #[test]
    fn a_missing_time_on_either_side_is_always_rescanned() {
        assert!(needs_rescan(&healthy(None), &facts(None)));
        assert!(needs_rescan(&healthy(at(T)), &facts(None)));
        assert!(needs_rescan(&healthy(None), &facts(at(T))));
        assert!(!needs_rescan(&healthy(at(T)), &facts(at(T))));
        assert_eq!(
            classify(Some(&healthy(None)), &facts(None)),
            ScanDecision::Probe
        );
        assert_eq!(
            classify(Some(&bad(None)), &facts(None)),
            ScanDecision::Probe
        );
        assert!(!is_blacklisted_and_unchanged(&bad(None), &facts(None)));
    }

    /// Facts at T2 over a row at T1, then a blacklisting with those facts: the
    /// classification skips it. This is the scanner's path (a probe fails, the
    /// row is stamped with the facts the decision saw).
    #[test]
    fn blacklisting_a_changed_file_with_the_facts_makes_it_skipped() {
        let r = healthy(at(T));
        let f = facts(at(T + 9));
        assert_eq!(classify(Some(&r), &f), ScanDecision::Probe);
        let out = FileRow::blacklisted("boom".into(), &f);
        assert_eq!(out.mtime, at(T + 9));
        assert_eq!(classify(Some(&out), &f), ScanDecision::Blacklisted);
    }

    #[test]
    fn a_blacklisted_row_takes_the_time_from_the_facts() {
        let out = FileRow::blacklisted("boom".into(), &facts(at(T)));
        assert_eq!(out.mtime, at(T));
        assert_eq!(out.blacklist_reason(), Some("boom"));
        assert!(out.plugins().is_empty());
    }
}
