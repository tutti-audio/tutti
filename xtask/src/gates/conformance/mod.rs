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

use super::carry::{self, Fate};
use super::clap_sys::{self, Item};
use super::lex::{blank_keeping_docs, skipped};
use super::midi_spec;
use super::sf2_spec;
use super::vst3_sdk;
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

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

fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'/' | b'#' | b'-'))
}

fn is_token(s: &str, allow_digit_first: bool) -> bool {
    let mut b = s.bytes();
    let first = b.next();
    first.is_some_and(|f| f.is_ascii_lowercase() || (allow_digit_first && f.is_ascii_digit()))
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

fn check_facts(facts: &str) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for pair in facts.split_whitespace() {
        let (name, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("fact `{pair}` is not name=int"))?;
        let ident = name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase());
        if !ident {
            return Err(format!("fact name `{name}` is not a lowercase identifier"));
        }
        let ok = match value.strip_prefix("0x") {
            Some(h) => !h.is_empty() && u64::from_str_radix(h, 16).is_ok(),
            None => {
                let digits = value.strip_prefix('-').unwrap_or(value);
                !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                    && value.parse::<i64>().is_ok()
            }
        };
        if !ok {
            return Err(format!("fact `{pair}` has a value that is not an integer"));
        }
        if !seen.insert(name) {
            return Err(format!("fact `{name}` is given twice"));
        }
    }
    Ok(())
}

/// One non-blank, non-comment line as a row, or what is wrong with it. The
/// paraphrase may be empty here: a freshly generated row has none, and
/// [`load`] refuses it.
pub(super) fn parse_row(line: &str) -> Result<Row, String> {
    let cols: Vec<&str> = line.split('|').map(str::trim).collect();
    let (digest, cols) = match cols.len() {
        10 => (cols[9], &cols[..9]),
        _ => ("", &cols[..]),
    };
    let [
        id,
        side,
        maturity,
        kind,
        section,
        paraphrase,
        facts,
        na_kind,
        cite,
    ] = *cols
    else {
        return Err(format!(
            "expected 9 or 10 columns, found {}",
            cols.len() + usize::from(!digest.is_empty())
        ));
    };
    if digest.len() != if digest.is_empty() { 0 } else { 16 }
        || !digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(format!("digest `{digest}` is not 16 lowercase hex digits"));
    }
    if !is_id(id) {
        return Err(format!(
            "id `{id}` is empty or has a character outside a-z A-Z 0-9 . _ / # -"
        ));
    }
    if !is_token(side.strip_prefix("orphan:").unwrap_or(side), false) {
        return Err(format!("side `{side}` is not a non-empty lowercase token"));
    }
    if !maturity.is_empty() && !MATURITIES.contains(&maturity) {
        return Err(format!(
            "maturity `{maturity}` is not stable, draft or empty"
        ));
    }
    if !kind.is_empty() && !KINDS.contains(&kind) {
        return Err(format!(
            "kind `{kind}` is not required, recommended, permitted or empty"
        ));
    }
    check_facts(facts)?;
    let na = match (na_kind.is_empty(), cite.is_empty()) {
        (true, true) => None,
        (false, false) if NA_KINDS.contains(&na_kind) => Some(NotApplicable {
            kind: na_kind.to_owned(),
            cite: cite.to_owned(),
        }),
        (false, false) => {
            return Err(format!(
                "na-kind `{na_kind}` is not deprecated, forbidden-for-side or other-side"
            ));
        }
        _ => return Err("na-kind and cite must be both set or both empty".to_owned()),
    };
    Ok(Row {
        id: id.to_owned(),
        side: Side::parse(side),
        maturity: maturity.to_owned(),
        kind: kind.to_owned(),
        section: section.to_owned(),
        paraphrase: paraphrase.to_owned(),
        facts: facts.to_owned(),
        na,
        digest: digest.to_owned(),
    })
}

/// A file's rows; the `Err`s are `file:line: problem`.
fn parse_file(path: &str, text: &str) -> (Vec<Row>, Vec<String>) {
    let (mut rows, mut errs) = (Vec::new(), Vec::new());
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        match parse_row(t) {
            Ok(r) => rows.push(r),
            Err(e) => errs.push(format!("{path}:{}: {e}", i + 1)),
        }
    }
    (rows, errs)
}

/// The version `Cargo.lock` pins for `clap-sys`.
pub(super) fn pinned_clap_sys(lock: &str) -> Option<String> {
    let mut lines = lock.lines();
    while let Some(l) = lines.next() {
        if l.trim() == "name = \"clap-sys\"" {
            let v = lines.next()?.trim().strip_prefix("version = \"")?;
            return Some(v.strip_suffix('"')?.to_owned());
        }
    }
    None
}

/// Every inventory file's rows (all files read, every error collected).
pub(super) fn load(tree: &dyn Tree) -> (Vec<(String, Row)>, Vec<String>) {
    let (mut rows, mut errs) = (Vec::new(), Vec::new());
    let mut owner: BTreeMap<String, String> = BTreeMap::new();
    for path in tree.files(DIR, &skipped) {
        if !path.ends_with(".txt") {
            continue;
        }
        let Some(text) = tree.read(&path) else {
            continue;
        };
        let (file_rows, file_errs) = parse_file(&path, &text);
        errs.extend(file_errs);
        for r in file_rows {
            if matches!(r.side, Side::Orphan(_)) {
                errs.push(format!(
                    "{path}: orphan row `{}`: its sentence is gone; move its hand text to the row it belongs to and delete this row",
                    r.id
                ));
                continue;
            }
            if r.side == Side::Changed {
                errs.push(format!(
                    "{path}: requirement text changed under hand columns: row `{}`; reread its sentence and rewrite its side, paraphrase and other hand columns",
                    r.id
                ));
            } else if r.paraphrase.is_empty() {
                errs.push(format!("{path}: row `{}` has an empty paraphrase", r.id));
            }
            if let Some(first) = owner.get(&r.id) {
                errs.push(format!("{path}: id `{}` is already a row of {first}", r.id));
            } else {
                owner.insert(r.id.clone(), path.clone());
                rows.push((path.clone(), r));
            }
        }
    }
    (rows, errs)
}

/// One `/// Spec:` tag: its line, the ids it claims, and the name of the `fn`
/// it documents (the first `fn` declared within a dozen lines below it, when
/// there is one; a tag above something else has none).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Tag {
    pub(super) line: usize,
    pub(super) ids: Vec<String>,
    pub(super) item: Option<String>,
}

/// The name a line declares with `fn`, when it does.
fn fn_name(line: &str) -> Option<String> {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with('#') {
        return None;
    }
    let after = super::lex::find_word(t, "fn").map(|at| &t[at + 2..])?;
    let name: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The tags of one file, and the problems with them.
pub(super) fn tags(path: &str, text: &str) -> (Vec<Tag>, Vec<String>) {
    let code = blank_keeping_docs(text);
    let lines: Vec<&str> = code.lines().collect();
    let (mut out, mut errs) = (Vec::new(), Vec::new());
    for (i, l) in lines.iter().enumerate() {
        let Some(rest) = l.trim_start().strip_prefix(TAG) else {
            continue;
        };
        let at = i + 1;
        if lines
            .get(i + 1)
            .is_some_and(|n| n.trim_start().starts_with("///"))
        {
            errs.push(format!(
                "{path}:{at}: the Spec tag is not the last doc line"
            ));
        }
        let ids: Vec<String> = rest.split(',').map(|s| s.trim().to_owned()).collect();
        if ids.iter().any(String::is_empty) {
            errs.push(format!("{path}:{at}: the Spec tag has an empty id"));
            continue;
        }
        let item = lines[i + 1..].iter().take(12).find_map(|n| fn_name(n));
        out.push(Tag {
            line: at,
            ids,
            item,
        });
    }
    (out, errs)
}

fn triples(items: &[Item]) -> BTreeSet<(String, String, String)> {
    items
        .iter()
        .map(|i| (i.id.clone(), i.side.to_owned(), i.maturity.to_owned()))
        .collect()
}

/// Which absent spec sources are findings (`TUTTI_CONFORMANCE`).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Require {
    pub(super) clap: bool,
    pub(super) vst3: bool,
    pub(super) midi: bool,
    pub(super) sf2: bool,
}

impl Require {
    /// `require` demands the `clap-sys` source and the VST3 SDK; `require-all`
    /// also the MIDI and SoundFont spec texts.
    pub(super) fn parse(value: Option<&str>) -> Require {
        match value {
            Some("require") => Require {
                clap: true,
                vst3: true,
                midi: false,
                sf2: false,
            },
            Some("require-all") => Require {
                clap: true,
                vst3: true,
                midi: true,
                sf2: true,
            },
            _ => Require::default(),
        }
    }
}

fn vst3_triples(items: &[vst3_sdk::Item]) -> BTreeSet<(String, String, String)> {
    items
        .iter()
        .map(|i| (i.id.clone(), i.side.to_owned(), i.maturity.to_owned()))
        .collect()
}

/// The `(id, kind, section, facts)` of every row whose id starts `prefix`.
fn generated(rows: &[(String, Row)], prefix: &str) -> BTreeSet<[String; 4]> {
    rows.iter()
        .map(|(_, r)| r)
        .filter(|r| r.id.starts_with(prefix))
        .map(|r| {
            [
                r.id.clone(),
                r.kind.clone(),
                r.section.clone(),
                r.facts.clone(),
            ]
        })
        .collect()
}

fn midi_quads(items: &[midi_spec::Item]) -> BTreeSet<[String; 4]> {
    items
        .iter()
        .map(|i| {
            [
                i.id.clone(),
                i.kind.to_owned(),
                i.section.clone(),
                i.facts.clone(),
            ]
        })
        .collect()
}

/// Each extracted sentence's row stores the digest of that sentence: a row
/// whose hand columns sit on another sentence (a hand edit, or a file written
/// for other text) fails here.
fn check_digests<'a>(
    r: &mut Report,
    rows: &[(String, Row)],
    label: &str,
    recipe: &str,
    items: impl Iterator<Item = (&'a str, &'a str)>,
) {
    let have: BTreeMap<&str, &str> = rows
        .iter()
        .map(|(_, row)| (row.id.as_str(), row.digest.as_str()))
        .collect();
    for (id, text) in items {
        let want = carry::digest(text);
        match have.get(id) {
            Some(&d) if !want.is_empty() && d != want => r.fail(format!(
                "{DIR}: the {label} row `{id}` stores digest `{d}` but its sentence digests to `{want}`: its hand columns may sit on another sentence; run `cargo xtask conformance-inventory {recipe}`"
            )),
            _ => {}
        }
    }
}

pub(super) fn check(
    tree: &dyn Tree,
    clap: Result<&dyn Tree, String>,
    midi: &dyn Fn(&midi_spec::Doc) -> Result<String, String>,
    sf2: &dyn Fn() -> Result<String, String>,
    require: Require,
) -> Report {
    let mut r = Report::default();
    let (rows, errs) = load(tree);
    for e in errs {
        r.fail(e);
    }
    let known: BTreeSet<&str> = rows.iter().map(|(_, row)| row.id.as_str()).collect();

    let mut tagged = BTreeSet::new();
    for root in ["crates", "testing"] {
        for path in tree.files(root, &skipped) {
            if !path.ends_with(".rs") {
                continue;
            }
            let Some(text) = tree.read(&path) else {
                continue;
            };
            let (found, errs) = tags(&path, &text);
            for e in errs {
                r.fail(e);
            }
            for Tag { line, ids, .. } in found {
                for id in ids {
                    if known.contains(id.as_str()) {
                        tagged.insert(id);
                    } else {
                        r.fail(format!(
                            "{path}:{line}: the Spec tag names unknown id `{id}`"
                        ));
                    }
                }
            }
        }
    }

    let committed: BTreeSet<(String, String, String)> = rows
        .iter()
        .filter(|(_, row)| row.id.starts_with("clap."))
        .map(|(_, row)| (row.id.clone(), row.side.to_string(), row.maturity.clone()))
        .collect();
    let spec = tree
        .read("Cargo.lock")
        .and_then(|l| pinned_clap_sys(&l))
        .map_or_else(|| Err("Cargo.lock pins no clap-sys".to_owned()), |_| clap)
        .and_then(clap_sys::extract);
    match spec {
        Ok(items) => {
            let want = triples(&items);
            for t in want.difference(&committed) {
                r.fail(format!(
                    "{DIR}: clap-sys declares `{}` ({}, {}) that no clap row matches; run `cargo xtask conformance-inventory clap`",
                    t.0, t.1, t.2
                ));
            }
            for t in committed.difference(&want) {
                r.fail(format!(
                    "{DIR}: the clap row `{}` ({}, {}) is not what clap-sys declares; run `cargo xtask conformance-inventory clap`",
                    t.0, t.1, t.2
                ));
            }
        }
        Err(why) if require.clap => r.fail(format!("clap inventory not checked: {why}")),
        Err(why) => r.say(format!("clap inventory not checked: {why}")),
    }
    let committed_vst3: BTreeSet<(String, String, String)> = rows
        .iter()
        .filter(|(_, row)| row.id.starts_with(VST3_PREFIX))
        .map(|(_, row)| (row.id.clone(), row.side.to_string(), row.maturity.clone()))
        .collect();
    match vst3_sdk::extract(tree) {
        Ok(items) => {
            let want = vst3_triples(&items);
            for t in want.difference(&committed_vst3) {
                r.fail(format!(
                    "{DIR}: the VST3 SDK declares `{}` ({}, {}) that no vst3 row matches; run `cargo xtask conformance-inventory vst3`",
                    t.0, t.1, t.2
                ));
            }
            for t in committed_vst3.difference(&want) {
                r.fail(format!(
                    "{DIR}: the vst3 row `{}` ({}, {}) is not what the VST3 SDK declares; run `cargo xtask conformance-inventory vst3`",
                    t.0, t.1, t.2
                ));
            }
        }
        Err(why) if require.vst3 => r.fail(format!("vst3 inventory not checked: {why}")),
        Err(why) => r.say(format!("vst3 inventory not checked: {why}")),
    }
    for doc in midi_spec::DOCS {
        let label = doc.label;
        match midi(doc).and_then(|text| midi_spec::extract(doc, &text)) {
            Ok(items) => {
                let want = midi_quads(&items);
                let have = generated(&rows, &format!("{}/", doc.prefix));
                for q in want.difference(&have) {
                    r.fail(format!(
                        "{DIR}: the {label} text gives `{}` ({}, {}, {}) that no row matches; run `cargo xtask conformance-inventory midi`",
                        q[0], q[1], q[2], q[3]
                    ));
                }
                for q in have.difference(&want) {
                    r.fail(format!(
                        "{DIR}: the {label} row `{}` ({}, {}, {}) is not what the text gives; run `cargo xtask conformance-inventory midi`",
                        q[0], q[1], q[2], q[3]
                    ));
                }
                check_digests(
                    &mut r,
                    &rows,
                    label,
                    "midi",
                    items.iter().map(|i| (i.id.as_str(), i.text.as_str())),
                );
            }
            Err(why) if require.midi => r.fail(format!("{label} inventory not checked: {why}")),
            Err(why) => r.say(format!("{label} inventory not checked: {why}")),
        }
    }
    // A hand-written SF2 row names its section as `SF2 §<n>[.<n>]*`, checked with or
    // without the text; that the section exists is checked below, against the text.
    for (path, row) in &rows {
        if row.id.starts_with("sf2.")
            && !sf2_spec::owns(&row.id)
            && sf2_spec::section_number(&row.section).is_none()
        {
            r.fail(format!(
                "{path}: the SF2 row `{}` has section `{}`, which is not `SF2 §<n>`",
                row.id, row.section
            ));
        }
    }
    match sf2().and_then(|text| sf2_spec::extract(&text)) {
        Ok(spec) => {
            let want: BTreeSet<[String; 4]> = spec
                .items
                .iter()
                .map(|i| {
                    [
                        i.id.clone(),
                        i.kind.to_owned(),
                        i.section.clone(),
                        i.facts.clone(),
                    ]
                })
                .collect();
            let have: BTreeSet<[String; 4]> = rows
                .iter()
                .map(|(_, r)| r)
                .filter(|r| sf2_spec::owns(&r.id))
                .map(|r| {
                    [
                        r.id.clone(),
                        r.kind.clone(),
                        r.section.clone(),
                        r.facts.clone(),
                    ]
                })
                .collect();
            for q in want.difference(&have) {
                r.fail(format!(
                    "{DIR}: the SF2 text gives `{}` ({}, {}, {}) that no row matches; run `cargo xtask conformance-inventory sf2`",
                    q[0], q[1], q[2], q[3]
                ));
            }
            for q in have.difference(&want) {
                r.fail(format!(
                    "{DIR}: the SF2 row `{}` ({}, {}, {}) is not what the text gives; run `cargo xtask conformance-inventory sf2`",
                    q[0], q[1], q[2], q[3]
                ));
            }
            check_digests(
                &mut r,
                &rows,
                "SF2",
                "sf2",
                spec.items.iter().map(|i| (i.id.as_str(), i.text.as_str())),
            );
            for (path, row) in &rows {
                if row.id.starts_with("sf2.") && !sf2_spec::owns(&row.id) {
                    let known = sf2_spec::section_number(&row.section)
                        .is_some_and(|n| spec.headings.contains(n));
                    if sf2_spec::section_number(&row.section).is_some() && !known {
                        r.fail(format!(
                            "{path}: the SF2 row `{}` has section `{}`, a section the text does not have",
                            row.id, row.section
                        ));
                    }
                }
            }
        }
        Err(why) if require.sf2 => r.fail(format!("SF2 inventory not checked: {why}")),
        Err(why) => r.say(format!("SF2 inventory not checked: {why}")),
    }
    r.say(format!(
        "check-conformance: {} rows, {} ids tagged",
        rows.len(),
        tagged.len()
    ));
    r
}

mod regen;
#[cfg(test)]
mod tests;

pub(super) use regen::{Regen, regenerate, regenerate_midi, regenerate_sf2, regenerate_vst3};
