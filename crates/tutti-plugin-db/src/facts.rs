//! Discovery decisions as pure functions over values.
//!
//! The scan reads the filesystem once per candidate into a [`FileFacts`]
//! (`FileFacts::read`, in `fs.rs`); everything here decides from that value and
//! a table row, touching no disk and no clock.

use crate::mtime::Mtime;
use crate::table::FileRow;

/// What the scan found on disk for one candidate, read once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFacts {
    /// The path does not exist.
    Missing,
    /// The path exists.
    Present {
        /// The file's last modification, or `None` if it cannot be read or lies
        /// before the Unix epoch. A file without a time is always rescanned.
        mtime: Option<Mtime>,
    },
}

impl FileFacts {
    /// Whether the path exists.
    #[must_use]
    pub fn exists(&self) -> bool {
        matches!(self, Self::Present { .. })
    }

    /// The file's last modification, or `None` for a missing file and one whose
    /// time cannot be read.
    #[must_use]
    pub fn mtime(&self) -> Option<Mtime> {
        match self {
            Self::Present { mtime } => *mtime,
            Self::Missing => None,
        }
    }
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
    match (row.mtime, facts.mtime()) {
        (Some(stamped), Some(read)) => stamped != read,
        _ => true,
    }
}

/// Returns `true` if the row is blacklisted and its file has not changed since
/// the blacklist was stamped.
///
/// A file with no readable time is never unchanged, so a blacklisted one is
/// probed again on every scan. That costs time, not safety, because the probe
/// runs out of process; a blacklist that held without a time could miss a
/// reinstall, which is worse.
#[must_use]
pub(crate) fn is_blacklisted_and_unchanged(row: &FileRow, facts: &FileFacts) -> bool {
    row.blacklist_reason().is_some() && !needs_rescan(row, facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::{Described, PluginClass, PluginDescriptor, PluginFormat, PluginId};

    use std::time::Duration;

    const T: u64 = 1_700_000_000;

    fn at(secs: u64) -> Option<Mtime> {
        Mtime::new(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn facts(mtime: Option<Mtime>) -> FileFacts {
        FileFacts::Present { mtime }
    }

    fn healthy(mtime: Option<Mtime>) -> FileRow {
        FileRow::probed(PluginFormat::Clap, &facts(mtime))
            .plugins(vec![Described {
                id: Some(PluginId::Clap("id".into())),
                descriptor: PluginDescriptor::new("name", PluginClass::Unknown),
            }])
            .build()
            .expect("ids of the row's format")
    }

    fn bad(mtime: Option<Mtime>) -> FileRow {
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

    /// A row stamped with a time and a file that reads without one are never
    /// equal: the file is rescanned.
    #[test]
    fn a_file_without_a_time_is_always_rescanned() {
        assert!(needs_rescan(&healthy(at(T)), &facts(None)));
        assert_eq!(
            classify(Some(&healthy(at(T))), &facts(None)),
            ScanDecision::Probe
        );
    }

    /// A row stamped without a time is never equal to any file, one with a
    /// time or one without.
    #[test]
    fn a_row_without_a_time_is_always_rescanned() {
        assert!(needs_rescan(&healthy(None), &facts(at(T))));
        assert!(needs_rescan(&healthy(None), &facts(None)));
        assert_eq!(
            classify(Some(&healthy(None)), &facts(None)),
            ScanDecision::Probe
        );
    }

    /// A blacklist stamped without a time never holds, so the file is probed
    /// again rather than skipped.
    #[test]
    fn a_blacklist_without_a_time_never_holds() {
        assert!(!is_blacklisted_and_unchanged(&bad(None), &facts(None)));
        assert_eq!(
            classify(Some(&bad(None)), &facts(None)),
            ScanDecision::Probe
        );
    }

    /// Equal times are not rescanned: the other tests in this module are not
    /// vacuous.
    #[test]
    fn equal_times_need_no_rescan() {
        assert!(!needs_rescan(&healthy(at(T)), &facts(at(T))));
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
