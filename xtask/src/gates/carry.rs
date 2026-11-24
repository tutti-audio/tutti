//! How a regenerated inventory keeps its hand columns: by the requirement
//! sentence, never by its position.
//!
//! A row extracted from spec text carries the [`digest`] of its sentence in the
//! tenth column. A sentence inserted in a section shifts the ids after it, so
//! the hand columns follow the digest: within a section, the nth row with a
//! digest takes the hand columns of the nth old row with that digest. A row
//! with no sentence (a table row) has a structural id and carries by id.
//!
//! A sentence reworded under hand columns has a new digest. Its row keeps the
//! new text's generated columns and loses the hand columns ([`Fate::Changed`]),
//! which the gate then refuses until a person rereads the sentence. Old rows
//! with hand columns that no sentence claims are [`Carried::orphans`]: each is
//! written back as a row with side `orphan:<old side>`, its old id, hand columns and digest,
//! after the last row of its section, and stays through every later run until a
//! person moves its text to the right row and deletes it. The gate refuses both
//! `changed` and `orphan` rows.
//!
//! The first regeneration after an inventory gains the digest column has no old
//! digests: an old row without one takes the digest of the sentence currently at
//! its id, so a rewording in that same run is accepted. The existing inventories
//! have been through it; it applies only to a newly added inventory.

use super::conformance::{Row, Side};
use std::collections::BTreeMap;

/// A short stable digest of a requirement sentence: FNV-1a 64 of its words
/// joined by single spaces, as 16 hex digits. Empty for no text.
pub(super) fn digest(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut first = true;
    for word in text.split_whitespace() {
        if !first {
            h = (h ^ u64::from(b' ')).wrapping_mul(0x0100_0000_01b3);
        }
        first = false;
        for b in word.bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    if first {
        String::new()
    } else {
        format!("{h:016x}")
    }
}

/// The section an id numbers its sentences in: the id up to its last `#`, or
/// the whole id when it has none.
pub(super) fn section_of(id: &str) -> &str {
    id.rsplit_once('#').map_or(id, |(s, _)| s)
}

/// Whether a row has anything a person wrote.
fn has_hand(r: &Row) -> bool {
    !r.paraphrase.is_empty() || r.na.is_some()
}

/// What happened to the old row of one extracted item.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Fate {
    /// The old row of the same sentence, wherever its id was.
    Kept(Row),
    /// The old row at this id had hand columns and another sentence.
    Changed(Row),
    /// No old row.
    New,
}

#[derive(Debug)]
pub(super) struct Carried {
    /// One per item, in order.
    pub(super) fates: Vec<Fate>,
    /// Old rows no item took, with hand columns.
    pub(super) orphans: Vec<Row>,
    /// Old rows no item took, with none.
    pub(super) dropped: Vec<Row>,
}

/// Match `old` rows to `items` (`(id, text)`, text empty for a structural row).
/// An old row without a digest takes that of the item at its id (the first
/// regeneration after the column was added).
pub(super) fn carry(old: Vec<Row>, items: &[(&str, &str)]) -> Carried {
    let (kept_orphans, old): (Vec<Row>, Vec<Row>) = old
        .into_iter()
        .partition(|r| matches!(r.side, Side::Orphan(_)));
    let by_id: BTreeMap<&str, &str> = items.iter().copied().collect();
    let mut old: Vec<(Row, Option<String>)> = old
        .into_iter()
        .map(|r| {
            let d = if r.digest.is_empty() {
                by_id.get(r.id.as_str()).map(|t| digest(t))
            } else {
                Some(r.digest.clone())
            };
            (r, d)
        })
        .collect();
    // (section, digest, occurrence) -> old index
    let mut keyed: BTreeMap<(String, String, usize), usize> = BTreeMap::new();
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (i, (r, d)) in old.iter().enumerate() {
        let Some(d) = d.as_ref().filter(|d| !d.is_empty()) else {
            continue;
        };
        let n = seen
            .entry((section_of(&r.id).to_owned(), d.clone()))
            .or_default();
        keyed.insert((section_of(&r.id).to_owned(), d.clone(), *n), i);
        *n += 1;
    }
    let mut taken = vec![false; old.len()];
    let mut fates: Vec<Option<Fate>> = Vec::new();
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    let by_old_id: BTreeMap<String, usize> = old
        .iter()
        .enumerate()
        .map(|(i, (r, _))| (r.id.clone(), i))
        .collect();
    for (id, text) in items {
        let d = digest(text);
        let hit = if d.is_empty() {
            by_old_id
                .get(*id)
                .copied()
                .filter(|&i| old[i].1.as_deref().is_none_or(str::is_empty))
        } else {
            let n = seen
                .entry((section_of(id).to_owned(), d.clone()))
                .or_default();
            let k = (section_of(id).to_owned(), d, *n);
            *n += 1;
            keyed.get(&k).copied()
        };
        fates.push(hit.map(|i| {
            taken[i] = true;
            Fate::Kept(old[i].0.clone())
        }));
    }
    // Reworded sentences: the old row at the same id, if still unclaimed.
    let fates = fates
        .into_iter()
        .zip(items)
        .map(|(f, (id, _))| {
            f.unwrap_or_else(|| match by_old_id.get(*id) {
                Some(&i) if !taken[i] => {
                    taken[i] = true;
                    if has_hand(&old[i].0) {
                        Fate::Changed(old[i].0.clone())
                    } else {
                        Fate::New
                    }
                }
                _ => Fate::New,
            })
        })
        .collect();
    let (mut orphans, mut dropped) = (kept_orphans, Vec::new());
    for (i, (mut r, _)) in old.drain(..).enumerate() {
        if !taken[i] {
            if has_hand(&r) {
                r.side = r.side.orphaned();
                orphans.push(r);
            } else {
                dropped.push(r);
            }
        }
    }
    Carried {
        fates,
        orphans,
        dropped,
    }
}
