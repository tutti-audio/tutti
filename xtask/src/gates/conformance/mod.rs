//! `check-conformance`: the spec inventories are well formed, every test tag
//! names a row, and the CLAP and VST3 inventories are the sets `clap-sys` and
//! the vendored VST3 SDK declare.
//!
//! An inventory is one or more files `testing/conformance/<area>[-<part>].txt`,
//! one spec item per line, nine columns separated by `|`:
//!
//! ```text
//! id | side | maturity | kind | section | paraphrase | facts | na-kind | cite
//! ```
//!
//! Blank lines and lines starting with `#` are skipped. `id` is non-empty and
//! made of `a-z A-Z 0-9 . _ / # -` (a spec's own capitals, `vst3.IComponent.setActive`, stay), unique across every file. `side` is a non-empty
//! lowercase token (`host`, `plugin`, `core`, `initiator`, ..). `maturity` is
//! `stable`, `draft` or empty; `kind` is `required`, `recommended`, `permitted`
//! or empty; `section` is free text or empty; `paraphrase` is the item in our
//! own words and is not empty in a committed row; `facts` is empty or
//! space-separated `name=int` pairs (`name` a lowercase identifier, `int`
//! decimal (negative with a leading `-`) or `0x` hex, no name
//! twice). `na-kind` (`deprecated`, `forbidden-for-side`, `other-side`) and
//! `cite` (the spec location) are both empty or both set. A row with the wrong
//! column count or any invalid column is an error naming the file and line;
//! nothing is half-read. A row extracted from spec text has a tenth column,
//! `digest` ([`super::carry`]): 16 hex digits of its sentence. The generator
//! carries the hand columns by digest, a sentence reworded under hand columns is
//! a row with side `changed` that this gate refuses, and with the spec text
//! present each stored digest must be the digest of the row's sentence.
//!
//! A test claims rows with a last rustdoc line `/// Spec: id, id` on its `fn`
//! (or above a case in the plugin contract's list; a contract case runs on
//! every format, so its tag lists every format's ids). A tag naming an id no
//! inventory holds, an empty tag, or a tag that is not the last doc line is a
//! finding. Rows no test claims are Missing, which is the work list and never
//! a finding.
//!
//! The rows whose id starts `clap.` are held to the `clap-sys` source the lock
//! file pins ([`super::clap_sys`]): the committed `(id, side,
//! maturity)` triples equal the extracted ones. With the source absent the
//! comparison is skipped and said so, unless `TUTTI_CONFORMANCE=require`.
//!
//! The rows whose id starts `vst3.` are held to the SDK headers under
//! `vendor/vst3-sdk/pluginterfaces` ([`super::vst3_sdk`]): the committed `(id,
//! side, maturity)` triples equal the extracted ones. The SDK is a submodule,
//! so a checkout without it skips the comparison and says so, unless
//! `TUTTI_CONFORMANCE=require` (CI sets it).
//!
//! `vst2.txt` (the VST 2.4 surface) and `au.txt` (the AudioUnit v2 surface) are
//! written by hand and checked by review: no VST 2.4 header is in the repo
//! (Steinberg no longer distributes the SDK) and the AudioToolbox headers ship
//! with macOS only, so nothing compares them to a source. They are held to the
//! same row format and tag rules as the others.
//!
//! The rows whose id starts with a MIDI document's prefix (`m2-101/` to
//! `m2-116/`, and `smf/`) are held to that document's spec text in the directory
//! `TUTTI_MIDI_SPECS` points at ([`super::midi_spec`]): the committed `(id,
//! kind, section, facts)` quadruples equal the extracted ones. The MIDI specs
//! are not in the repo and not in CI, so with the text absent the comparison
//! is skipped and said so, and only `TUTTI_CONFORMANCE=require-all` (which
//! also requires the `clap-sys` source and the SoundFont text) makes the
//! absence a finding. CI sets
//! `require`, which never demands the MIDI text; `require-all` is for a local
//! run on a machine that holds the specs.
//!
//! `sf2.txt` (SoundFont 2.04) mixes the two kinds. The rows in the families
//! [`super::sf2_spec`] extracts (`sf2.gen.`, `sf2.mod.default.`, the modulator
//! enumerations, the INFO and pdta sub-chunks, `sf2.sampletype.`) are held to
//! the spec text in the directory `TUTTI_SF2_SPEC` points at: the committed
//! `(id, kind, section, facts)` quadruples equal the extracted ones. Every
//! other `sf2.` row is written by hand, and its `section` must be a section
//! number the text has (`SF2 §7.2`). The text is not in the repo and not in
//! CI, so with it absent the comparison is skipped and said so, and only
//! `TUTTI_CONFORMANCE=require-all` makes the absence a finding.

mod check;
mod parse;
mod regen;
#[cfg(test)]
mod tests;

pub(super) use check::{Require, check, tags};
pub(super) use parse::{load, pinned_clap_sys};
pub(super) use regen::{Regen, regenerate, regenerate_vst3};

const DIR: &str = "testing/conformance";
const NA_KINDS: [&str; 3] = ["deprecated", "forbidden-for-side", "other-side"];
const KINDS: [&str; 3] = ["required", "recommended", "permitted"];
const MATURITIES: [&str; 2] = ["stable", "draft"];
const TAG: &str = "/// Spec:";
/// The ids of the VST3 inventory.
const VST3_PREFIX: &str = "vst3.";

/// A row's side column: the spec side the row is about, or where the row
/// stands while its sentence is being reread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Side {
    /// The side of the spec the row is about (`plugin`, `host`, `todo`, ..).
    Spec(String),
    /// The sentence changed under the row's hand columns, which are blanked.
    Changed,
    /// The row keeps the hand columns of a sentence no row claims any more;
    /// holds the side it had.
    Orphan(String),
}

/// The text of [`Side::Changed`].
const CHANGED: &str = "changed";
/// The text before the old side of [`Side::Orphan`].
const ORPHAN_PREFIX: &str = "orphan:";

impl Side {
    /// The side a column's text writes.
    fn parse(text: &str) -> Self {
        if text == CHANGED {
            Self::Changed
        } else if let Some(old) = text.strip_prefix(ORPHAN_PREFIX) {
            Self::Orphan(old.to_owned())
        } else {
            Self::Spec(text.to_owned())
        }
    }

    /// This side as a row that keeps its hand columns without a sentence.
    pub(super) fn orphaned(self) -> Self {
        match self {
            Self::Spec(side) => Self::Orphan(side),
            Self::Changed => Self::Orphan(CHANGED.to_owned()),
            orphan @ Self::Orphan(_) => orphan,
        }
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spec(side) => f.write_str(side),
            Self::Changed => f.write_str(CHANGED),
            Self::Orphan(side) => write!(f, "{ORPHAN_PREFIX}{side}"),
        }
    }
}

/// Why a row is not a requirement on its side, and where the spec says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NotApplicable {
    /// One of [`NA_KINDS`].
    pub(super) kind: String,
    /// The spec location.
    pub(super) cite: String,
}

/// One inventory row, every column as written (trimmed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    pub(super) id: String,
    pub(super) side: Side,
    pub(super) maturity: String,
    pub(super) kind: String,
    pub(super) section: String,
    pub(super) paraphrase: String,
    pub(super) facts: String,
    pub(super) na: Option<NotApplicable>,
    /// [`super::carry::digest`] of the requirement sentence a text-extracted row
    /// stands for; empty for a row with no sentence.
    pub(super) digest: String,
}

impl Row {
    fn render(&self) -> String {
        let (na_kind, cite) = self
            .na
            .as_ref()
            .map_or(("", ""), |na| (na.kind.as_str(), na.cite.as_str()));
        let side = self.side.to_string();
        let mut cols = vec![
            self.id.as_str(),
            &side,
            &self.maturity,
            &self.kind,
            &self.section,
            &self.paraphrase,
            &self.facts,
            na_kind,
            cite,
        ];
        if !self.digest.is_empty() {
            cols.push(&self.digest);
        }
        cols.join(" | ").trim_end().to_owned()
    }
}
