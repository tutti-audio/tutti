//! The MIDI conformance surface of each document in [`DOCS`], read from the
//! spec's `pdftotext -layout` text. The text is never committed; the extraction is a pure function from
//! that text to [`Item`]s, so a test feeds it small in-memory texts.
//!
//! Each document has its own prefix (`m2-101` to `m2-116`), its own table of
//! rows beyond the keyword sentences ([`Tables`]) and its own heading quirks
//! ([`Doc`]). Three kinds of item:
//!
//! - `<doc>/<section>#<n>`: the `n`th sentence (1-based, in document order)
//!   under the deepest numbered heading `<section>` (lowercase: `7.4.2`,
//!   `d.2.1`) that carries a conformance word. Each spec's own table of
//!   conformance words (Table 2 in both M2-104 and M2-116, the same three
//!   rows) maps `shall` to `required`, `should` to `recommended`
//!   and `may` to `permitted`; the table has no negated forms, so `shall
//!   not`, `should not` and `may not` keep the kind of the bare word and
//!   carry the fact `neg=1`. A sentence with several words takes the
//!   strongest. A word in quotation marks names the word and is not a use of
//!   it, and neither is the first cell of a table row (the table that
//!   defines the words). Notes and examples are not set apart: the spec says
//!   the words are used solely for conformance. M2-101, M2-102, M2-103 and
//!   M2-115 carry the same Table 2 (and a Table 3 of words that are not conformance
//!   words: must, will, can, might).
//!   A description cell of a table row that says its value is copied or
//!   echoed from another Command ([`COPY_WORDS`]) and has no conformance word
//!   is a `required` row too, with the fact `cell=1`; it is numbered after the
//!   keyword rows of its section, so adding it moves no existing id. A later
//!   keyword row in the same section renumbers the cell rows after it, as
//!   keyword rows renumber each other. A definition list's `shall` sentence
//!   is an ordinary keyword row.
//! - `m2-104/mt/<mt>`: one per Message Type of the allocation table
//!   ([`Tables::Messages`]), facts `mt` and `bytes`, the UMP size.
//! - `m2-104/msg/<mt>/<status>`: one per defined message of the message
//!   tables ([`Tables::Messages`]). `<mt>` is the Message Type in lowercase hex,
//!   `<status>` the status in lowercase hex without `0x` and without leading
//!   zeros; for Message Type 0xD it is `<bank>.<status>`, each two hex
//!   digits. Facts: `mt`, `status` (and `bank` for 0xD), `bytes`.
//! - `m2-116/hdr/<n>`: one per byte of the File Header table
//!   ([`Tables::FileHeader`]), facts `byte` (1-based) and `value`.
//!
//! The text is cleaned first: lines that repeat on at least half the pages
//! (running headers and footers) and a page's last line when it is only
//! digits are dropped, and pages are joined with no paragraph break, so a
//! sentence that crosses a page is one sentence. Everything before the first
//! numbered heading `1` that has no dot leader is the cover and the table of
//! contents, and is skipped. A heading is a line that is the next sibling or
//! first child of a heading already seen (`7.4.2` after `7.4.1`, `D.1` after
//! `Appendix D:`), so a wrapped line that starts with a number is not one. A
//! document with [`Doc::implicit_levels`] also takes a heading whose missing
//! ancestors would each be the first of their level (`6.2.1.1` straight after
//! `6.2`); the missing levels count as seen. A document with
//! [`Doc::dotted_top`] also takes a top-level heading written `1. Alpha`. A
//! document with `implicit_levels` may open at `1.1`, with no heading `1`.
//!
//! [`SMF`] (Standard MIDI Files 1.0) has no numbered headings and no `pdftotext`
//! layout: its sections are the named lines of [`Named`], read from the OCR text
//! of the spec in The Complete MIDI 1.0 book, and its rows are
//! `smf/<section>#<n>` under those names.
//!
//! The extraction never returns half a set: a message row whose Message Type
//! has no size, two items with one id, or a configured table that is not
//! found is an error.

use std::collections::{BTreeMap, BTreeSet};

use super::spec_text;

pub(super) use docs::DOCS;
#[cfg(test)]
pub(super) use docs::{M2_104, M2_116};

/// One extracted spec item. `text` is the sentence behind a keyword row (for
/// the person writing its paraphrase) and is empty for a table row; it is
/// never committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub(super) id: String,
    pub(super) kind: &'static str,
    pub(super) section: String,
    pub(super) facts: String,
    pub(super) text: String,
}

/// What a document has besides its keyword sentences.
#[derive(Clone, Copy)]
pub(super) enum Tables {
    /// The Message Type allocation table and the tables of defined messages.
    Messages {
        /// The allocation table's number.
        mt_table: u32,
        /// `(table number, row shape)` of each table that defines messages.
        messages: &'static [(u32, Rows)],
    },
    /// The table that spells the file header, one byte per column.
    FileHeader { table: u32 },
    /// Keyword sentences only.
    None,
    /// A document with no numbered headings: its sections are named lines.
    Named(&'static Named),
}

/// A spec whose sections are lines of fixed text, read from an OCR text with
/// no page breaks (Standard MIDI Files 1.0, in The Complete MIDI 1.0 book).
/// The spec has no table of conformance words: `words` are the ones it uses
/// to bind (`must`, `should`, `may`), `phrases` are read as another word
/// before they are looked up (`not optional` binds as `must`) and `defs` mark
/// a sentence that defines the format with no such word, which is a
/// `required` row with the fact `def=1`.
pub(super) struct Named {
    /// `(heading text, section)` in document order. A line is the heading when
    /// its tail is the text and what comes before is only hex bytes and field
    /// names of a meta-event line (`FF 58 04 nn dd cc bb Time Signature`). The
    /// first heading opens the spec and the first line after it that is
    /// `end` closes it.
    headings: &'static [(&'static str, &'static str)],
    end: &'static str,
    /// The page footer, with the page number before or after it.
    footer: &'static str,
    words: &'static spec_text::Words,
    phrases: &'static [(&'static str, &'static str)],
    defs: &'static [&'static str],
}

/// One spec document the extractor reads.
pub(super) struct Doc {
    /// The id prefix: `m2-104`.
    pub(super) prefix: &'static str,
    /// The document's name in `section` columns: `M2-104`.
    pub(super) label: &'static str,
    /// The title and version in the inventory header.
    pub(super) title: &'static str,
    /// The inventory file under `testing/conformance/`.
    pub(super) file: &'static str,
    /// The spec's file name starts with this (a `.txt` or a `.pdf`).
    pub(super) source: &'static str,
    /// The side column's words in the inventory header.
    pub(super) sides: &'static str,
    /// Lines the inventory header adds before the side line.
    pub(super) note: &'static str,
    /// Take a heading whose missing ancestors are each the first of their
    /// level.
    implicit_levels: bool,
    /// Take a top-level heading written with a closing dot (`1. Introduction`).
    dotted_top: bool,
    pub(super) tables: Tables,
}

/// How a message table's lines read as rows.
#[derive(Clone, Copy)]
pub(super) enum Rows {
    /// A line with a Message Type, a group cell and a status.
    Formats,
    /// A line with a Flex Data status bank, a status and a name.
    FlexStatus,
}

/// The Message Type of the Flex Data messages, whose status is a bank and a
/// status.
const FLEX_MT: u32 = 0xD;

const WORDS: [(&str, &str); 3] = [
    ("shall", "required"),
    ("should", "recommended"),
    ("may", "permitted"),
];

/// One cleaned line, classified.
#[derive(Debug, PartialEq)]
enum Line {
    Blank,
    /// A heading and its label (`7.4.2`, `D`, `D.1`, as written).
    Heading(String),
    /// A table or figure caption: `(is_table, number)`.
    Caption(bool, u32),
    Text(String),
}

/// The pages of `text` with running furniture removed, as one line list.
fn clean(text: &str) -> Vec<String> {
    let pages: Vec<Vec<&str>> = text
        .split('\x0c')
        .map(|p| p.lines().collect::<Vec<_>>())
        .collect();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for page in &pages {
        let mut here = BTreeSet::new();
        for l in page {
            let t = l.trim();
            if !t.is_empty() {
                here.insert(t);
            }
        }
        for t in here {
            *seen.entry(t).or_default() += 1;
        }
    }
    let floor = (pages.len() / 2).max(3);
    let mut out = Vec::new();
    for page in &pages {
        let last = page.iter().rposition(|l| !l.trim().is_empty());
        let mut kept: Vec<String> = Vec::new();
        for (i, l) in page.iter().enumerate() {
            let t = l.trim();
            let page_number =
                Some(i) == last && !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
            let furniture = seen.get(t).is_some_and(|&n| !t.is_empty() && n >= floor);
            if !page_number && !furniture {
                kept.push((*l).to_owned());
            }
        }
        while kept.first().is_some_and(|l| l.trim().is_empty()) {
            kept.remove(0);
        }
        while kept.last().is_some_and(|l| l.trim().is_empty()) {
            kept.pop();
        }
        out.extend(kept);
    }
    out
}

/// A heading's label parts: `(appendix, numbers)`.
type Label = (bool, Vec<u32>);

fn render(label: &Label) -> String {
    let (app, nums) = label;
    if !app {
        return nums
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".");
    }
    let letter = char::from(b'A' + (nums[0] - 1) as u8);
    let rest: Vec<String> = nums[1..].iter().map(u32::to_string).collect();
    if rest.is_empty() {
        letter.to_string()
    } else {
        format!("{letter}.{}", rest.join("."))
    }
}

/// A heading candidate: the label and nothing about whether it is expected.
fn parse_heading(line: &str, dotted_top: bool) -> Option<Label> {
    let indent = line.len() - line.trim_start().len();
    let t = line.trim();
    if indent > 6 || t.contains(".....") || t.len() > 160 {
        return None;
    }
    if let Some(rest) = t.strip_prefix("Appendix ") {
        let mut c = rest.chars();
        let letter = c.next().filter(char::is_ascii_uppercase)?;
        let after = c.as_str().trim_start_matches(' ');
        // `Appendix A.` heads a heading; a line that closes a sentence is text.
        let after = after
            .strip_prefix(':')
            .or_else(|| after.strip_prefix('.').filter(|_| !t.ends_with('.')))?;
        after.trim().chars().next()?;
        return Some((true, vec![u32::from(letter as u8 - b'A') + 1]));
    }
    let (label, title) = t.split_once(char::is_whitespace)?;
    let title = title.trim_start();
    // A table row (`8   Balance   -`) has a gap inside what would be the title.
    if title.contains("   ")
        || !title
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '"')
    {
        return None;
    }
    // A dotted label (`1. Introduction`) is a heading only in a document that
    // writes its top-level headings so; elsewhere it is a list item.
    let label = match label.strip_suffix('.') {
        Some(bare) if dotted_top && !bare.contains('.') => bare,
        Some(_) => return None,
        None => label,
    };
    let mut parts = label.split('.');
    let first = parts.next()?;
    let (app, head) = match first.as_bytes() {
        [l] if l.is_ascii_uppercase() => (true, u32::from(l - b'A') + 1),
        b if !b.is_empty() && b.iter().all(u8::is_ascii_digit) => (false, first.parse().ok()?),
        _ => return None,
    };
    let mut nums = vec![head];
    for p in parts {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        nums.push(p.parse().ok()?);
    }
    // An appendix letter alone is no heading (`Appendix` carries it), and an
    // appendix sub-heading needs a number.
    if app && nums.len() < 2 {
        return None;
    }
    Some((app, nums))
}

/// Tracks the headings seen, accepting only the next sibling or first child
/// of one already seen.
#[derive(Default, Clone)]
struct Outline {
    implicit: bool,
    /// An appendix heading has been seen: the body's numbering is over.
    in_appendix: bool,
    seen: BTreeSet<String>,
    last_child: BTreeMap<String, u32>,
}

impl Outline {
    /// Count the missing ancestors of `label` as seen when each is the first
    /// of its level and some ancestor is seen.
    fn fill_implicit(&mut self, label: &Label) {
        let (app, nums) = label;
        let up = |n: usize| -> Label { (*app, nums[..n].to_vec()) };
        let mut depth = nums.len().saturating_sub(1);
        while depth >= 1 && !self.seen.contains(&render(&up(depth))) {
            depth -= 1;
        }
        // The ancestors of lengths `depth + 1 ..= len - 1` are missing.
        let missing = depth + 1..nums.len();
        if depth == 0 || missing.is_empty() || missing.clone().any(|n| nums[n - 1] != 1) {
            return;
        }
        let mut parent = render(&up(depth));
        for n in missing {
            let this = render(&up(n));
            self.last_child.insert(parent, 1);
            self.seen.insert(this.clone());
            parent = this;
        }
    }

    fn accept(&mut self, label: &Label) -> bool {
        if !self.implicit {
            return self.accept_next(label);
        }
        let mut trial = self.clone();
        trial.fill_implicit(label);
        let ok = trial.accept_next(label);
        if ok {
            *self = trial;
        }
        ok
    }

    fn accept_next(&mut self, label: &Label) -> bool {
        let (app, nums) = label;
        let (parent, number) = match (app, nums.len()) {
            (true, 1) => ("#".to_owned(), nums[0]),
            (false, 1) if self.in_appendix => return false,
            (false, 1) => (String::new(), nums[0]),
            _ => {
                let up: Label = (*app, nums[..nums.len() - 1].to_vec());
                let parent = render(&up);
                if !self.seen.contains(&parent) {
                    return false;
                }
                (parent, nums[nums.len() - 1])
            }
        };
        if self.last_child.get(&parent).copied().unwrap_or(0) + 1 != number {
            return false;
        }
        self.last_child.insert(parent, number);
        self.seen.insert(render(label));
        self.in_appendix |= *app;
        true
    }
}

/// A line that starts with a number and a closing dot (`3. Continue`).
fn dotted_item(t: &str) -> bool {
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && t[digits..].starts_with(". ")
}

/// Whether the nearest text line before or after `lines[at]` is a dotted item
/// too: a run of them is a list, and a dotted heading stands alone.
fn next_to_dotted_item(lines: &[String], at: usize) -> bool {
    let text = |l: &&String| !l.trim().is_empty();
    let before = lines[..at].iter().rev().find(text);
    let after = lines[at + 1..].iter().find(text);
    before.is_some_and(|l| dotted_item(l.trim())) || after.is_some_and(|l| dotted_item(l.trim()))
}

fn caption(t: &str) -> Option<(bool, u32)> {
    let (is_table, rest) = match (t.strip_prefix("Table "), t.strip_prefix("Figure ")) {
        (Some(r), _) => (true, r),
        (_, Some(r)) => (false, r),
        _ => return None,
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = rest[digits.len()..].strip_prefix(' ')?;
    if digits.is_empty() || t.contains(".....") {
        return None;
    }
    after
        .chars()
        .next()
        .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit())?;
    Some((is_table, digits.parse().ok()?))
}

/// The classified lines of the body (from the first real heading `1`).
fn classify(lines: &[String], doc: &Doc) -> Result<Vec<Line>, String> {
    let mut outline = Outline {
        implicit: doc.implicit_levels,
        ..Outline::default()
    };
    let mut out = Vec::new();
    let mut started = false;
    for (at, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.is_empty() {
            if started {
                out.push(Line::Blank);
            }
            continue;
        }
        if let Some(label) = parse_heading(l, doc.dotted_top)
            .filter(|_| !(dotted_item(t) && next_to_dotted_item(lines, at)))
        {
            let first = !started
                && (label == (false, vec![1])
                    || (doc.implicit_levels && label == (false, vec![1, 1])));
            if first || (started && outline.accept(&label)) {
                if first {
                    if label.1.len() == 2 {
                        // A document that opens at `1.1` has no heading `1`.
                        outline.accept(&(false, vec![1]));
                    }
                    outline.accept(&label);
                    started = true;
                }
                out.push(Line::Heading(render(&label)));
                continue;
            }
        }
        if !started {
            continue;
        }
        match caption(t) {
            Some((table, n)) => out.push(Line::Caption(table, n)),
            None => out.push(Line::Text(l.clone())),
        }
    }
    if !started {
        return Err("no numbered heading `1` found: is this the spec text?".to_owned());
    }
    Ok(out)
}

/// A line that is a table row: it has a gap of three spaces between cells.
fn cells(text: &str) -> Vec<&str> {
    text.split("   ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect()
}

fn is_abbreviation(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "e.g" | "i.e" | "vs" | "cf" | "approx" | "fig" | "sec" | "no"
    )
}

/// The sentences of one paragraph unit: split after `. ? !` that is followed
/// by a space and a capital, digit-free of abbreviations.
fn sentences(unit: &str) -> Vec<&str> {
    let b = unit.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..b.len() {
        if !matches!(b[i], b'.' | b'?' | b'!') {
            continue;
        }
        // Skip closing quotes and brackets after the mark.
        let mut j = i + 1;
        while unit[j..].starts_with(['"', '\'', ')', '\u{201d}', '\u{2019}']) {
            j += unit[j..].chars().next().map_or(1, char::len_utf8);
        }
        let rest = &unit[j..];
        let Some(next) = rest.strip_prefix(' ').map(str::trim_start) else {
            continue;
        };
        if !next
            .chars()
            .next()
            .is_some_and(|c| c.is_uppercase() || matches!(c, '\u{2018}' | '\u{201c}' | '"' | '\''))
        {
            continue;
        }
        let word_start = unit[..i]
            .rfind(|c: char| c.is_whitespace())
            .map_or(0, |p| p + 1);
        if b[i] == b'.'
            && is_abbreviation(unit[word_start..i].trim_start_matches(['(', '\u{201c}', '"']))
        {
            continue;
        }
        out.push(unit[start..j].trim());
        start = j;
    }
    out.push(unit[start..].trim());
    out.retain(|s| !s.is_empty());
    out
}

struct Unit {
    section: String,
    text: String,
}

/// The paragraph units of the body, each under its section.
fn units(lines: &[Line]) -> Vec<Unit> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut cur: Option<String> = None;
    let mut last_item = 0;
    let flush = |cur: &mut Option<String>, section: &str, out: &mut Vec<Unit>| {
        if let Some(text) = cur.take() {
            out.push(Unit {
                section: section.to_owned(),
                text,
            });
        }
    };
    for l in lines {
        let raw = match l {
            Line::Heading(label) => {
                flush(&mut cur, &section, &mut out);
                section = label.to_ascii_lowercase();
                last_item = 0;
                continue;
            }
            Line::Blank | Line::Caption(..) => {
                flush(&mut cur, &section, &mut out);
                continue;
            }
            Line::Text(t) => t,
        };
        let t = raw.trim();
        let (bullet, body) = match t.strip_prefix('\u{2022}') {
            Some(r) => (true, r.trim_start()),
            None => (false, t),
        };
        // A numbered item is the next of a list (`1.` starts one); a wrapped
        // line that starts `32. Instead` is not.
        let item = body
            .split_once(". ")
            .filter(|(n, r)| !r.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|(n, _)| n.parse::<u32>().ok());
        let numbered = item.is_some_and(|n| n == 1 || n == last_item + 1);
        if let Some(n) = item.filter(|_| numbered) {
            last_item = n;
        }
        let row = cells(body);
        if row.len() > 1 {
            flush(&mut cur, &section, &mut out);
            // The row that defines a conformance word is not a use of it.
            let defines = WORDS.iter().any(|(w, _)| row[0].eq_ignore_ascii_case(w));
            if !defines {
                out.push(Unit {
                    section: section.clone(),
                    text: row.join(" "),
                });
            }
            continue;
        }
        let starts_note = body.starts_with("Note:") || body.starts_with("Example");
        if bullet || numbered || starts_note {
            flush(&mut cur, &section, &mut out);
        }
        match cur.as_mut() {
            Some(c) => {
                if c.ends_with('-') && body.starts_with(|ch: char| ch.is_lowercase()) {
                    c.push_str(body);
                } else {
                    c.push(' ');
                    c.push_str(body);
                }
            }
            None => cur = Some(body.to_owned()),
        }
    }
    flush(&mut cur, &section, &mut out);
    out
}

/// The `<doc>/<section>#<n>` rows.
fn keyword_items(lines: &[Line], doc: &Doc) -> Vec<Item> {
    let mut count: BTreeMap<String, u32> = BTreeMap::new();
    let mut out = Vec::new();
    for unit in units(lines) {
        let parts: Vec<&str> = match doc.tables {
            // Standard MIDI Files joins independent rules with `;`.
            Tables::Named(_) => sentences(&unit.text)
                .into_iter()
                .flat_map(|s| s.split("; "))
                .collect(),
            _ => sentences(&unit.text),
        };
        for s in parts {
            let (kind, negated, def) = match doc.tables {
                Tables::Named(n) => match smf_kind(s, n) {
                    Some(k) => k,
                    None => continue,
                },
                _ => match spec_text::keyword(s, &WORDS) {
                    Some((k, neg)) => (k, neg, false),
                    None => continue,
                },
            };
            let n = count.entry(unit.section.clone()).or_default();
            *n += 1;
            let section = match doc.tables {
                Tables::Named(_) => unit.section.clone(),
                _ => unit.section.to_ascii_uppercase(),
            };
            let facts: Vec<&str> = [(negated, "neg=1"), (def, "def=1")]
                .iter()
                .filter(|(on, _)| *on)
                .map(|(_, f)| *f)
                .collect();
            out.push(Item {
                id: format!("{}/{}#{n}", doc.prefix, unit.section),
                kind,
                section: format!("{} \u{a7}{section}", doc.label),
                facts: facts.join(" "),
                text: s.to_owned(),
            });
        }
    }
    out
}

/// `(kind, negated, def)` of a sentence of a [`Named`] spec, or `None` when
/// it binds nothing.
fn smf_kind(sentence: &str, named: &Named) -> Option<(&'static str, bool, bool)> {
    let mut read = sentence.to_owned();
    for (phrase, word) in named.phrases {
        read = replace_ignoring_case(&read, phrase, word);
    }
    if let Some((kind, negated)) = spec_text::keyword(&read, named.words) {
        return Some((kind, negated, false));
    }
    let defines = sentence
        .split(|c: char| !c.is_ascii_alphabetic())
        .any(|w| named.defs.contains(&w.to_ascii_lowercase().as_str()));
    defines.then_some(("required", false, true))
}

/// `text` with each ASCII-case-insensitive occurrence of `phrase` replaced.
fn replace_ignoring_case(text: &str, phrase: &str, with: &str) -> String {
    let (lower, phrase) = (text.to_ascii_lowercase(), phrase.to_ascii_lowercase());
    let mut out = String::new();
    let mut at = 0;
    while let Some(found) = lower[at..].find(&phrase) {
        out.push_str(&text[at..at + found]);
        out.push_str(with);
        at += found + phrase.len();
    }
    out.push_str(&text[at..]);
    out
}

/// Whether `line` is the heading `text`: its tail is the text and what comes
/// before is a hex byte then meta-event field names (`nn`, `tttttt`, `len`).
fn is_named_heading(line: &str, text: &str) -> bool {
    let t = line.trim();
    let Some(head) = t.strip_suffix(text) else {
        return false;
    };
    let tokens: Vec<&str> = head.split_whitespace().collect();
    head.is_empty()
        || head.ends_with(' ')
            && tokens.len() <= 8
            && tokens[0].len() == 2
            && tokens[0].bytes().all(|b| b.is_ascii_hexdigit())
            && tokens.iter().all(|w| {
                w.len() <= 2
                    || matches!(*w, "len" | "text" | "data")
                    || w.bytes().all(|b| Some(b) == w.bytes().next())
            })
}

/// Whether `line` is a page footer of the spec (`Standard MIDI Files 1.0 5`).
fn is_footer(line: &str, footer: &str) -> bool {
    let t = line.trim();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    t == footer
        || t.strip_suffix(footer).is_some_and(|n| digits(n.trim_end()))
        || t.strip_prefix(footer)
            .is_some_and(|n| digits(n.trim_start()))
}

/// The classified lines of a [`Named`] spec: from its first heading to its
/// `end` line, page footers dropped and a sentence a footer cut rejoined.
fn named_lines(text: &str, named: &Named) -> Result<Vec<Line>, String> {
    let all: Vec<&str> = text.lines().collect();
    let first = named.headings[0].0;
    let from = all
        .iter()
        .position(|l| l.trim() == first)
        .ok_or_else(|| format!("no line `{first}` found: is this the spec text?"))?;
    let to = all[from..]
        .iter()
        .position(|l| l.trim() == named.end)
        .map(|n| n + from)
        .ok_or_else(|| format!("no line `{}` closes the spec", named.end))?;
    let body = &all[from..to];
    let mut out = Vec::new();
    let mut expected = 0;
    let mut skip_to = 0;
    for (i, l) in body.iter().enumerate() {
        if i < skip_to {
            continue;
        }
        if is_footer(l, named.footer) {
            let next = body[i + 1..].iter().position(|n| !n.trim().is_empty());
            let prev = out.iter().rev().find(|x| **x != Line::Blank);
            let open = matches!(prev, Some(Line::Text(p))
                if !p.trim_end().ends_with(['.', ':', '?', '!', '"', ')']));
            let lower = next.is_some_and(|n| {
                body[i + 1 + n]
                    .trim_start()
                    .starts_with(|c: char| c.is_lowercase())
            });
            if open && lower {
                while out.last() == Some(&Line::Blank) {
                    out.pop();
                }
                skip_to = i + 1 + next.unwrap_or(0);
            }
            continue;
        }
        if l.trim().is_empty() {
            out.push(Line::Blank);
            continue;
        }
        match named
            .headings
            .iter()
            .position(|(h, _)| is_named_heading(l, h))
        {
            Some(k) => {
                if k > expected {
                    return Err(format!(
                        "heading `{}` not found",
                        named.headings[expected].0
                    ));
                }
                if k < expected {
                    let want = named.headings.get(expected).map_or("none", |h| h.0);
                    return Err(format!(
                        "heading `{}` is out of order or repeated: the spec's headings come once each, in order, and `{want}` was expected",
                        named.headings[k].0
                    ));
                }
                expected += 1;
                out.push(Line::Heading(named.headings[k].1.to_owned()));
            }
            None => out.push(Line::Text((*l).to_owned())),
        }
    }
    if let Some((text, _)) = named.headings.get(expected) {
        return Err(format!("heading `{text}` not found"));
    }
    Ok(out)
}

/// Words that make a field description state that the field's value is taken
/// from another Command (`copied from the received Ping Command`): the spec
/// fixes the field by reference, with no conformance word. The list is
/// closed: a cell that states a derived value (a length, a count) is not
/// taken.
const COPY_WORDS: [&str; 2] = ["copied", "echoed"];

/// The `<doc>/<section>#<n>` rows that carry no conformance word: a row of a
/// table whose description cells (every cell after the field name, over the
/// row's wrapped lines) hold one of [`COPY_WORDS`]. A row is the run of lines
/// between blank lines that starts with a line of two or more cells. A row with
/// a conformance word is already a keyword row. The rows are numbered after
/// the keyword rows of their section, so adding this kind moves no existing id;
/// a later keyword row in a section renumbers the cell rows after it. A table
/// ends at the first line of prose (a line of one cell that is not a wrapped
/// line of a row), at a caption or at a heading.
fn cell_items(lines: &[Line], doc: &Doc, taken: &[Item]) -> Vec<Item> {
    let mut count: BTreeMap<&str, u32> = BTreeMap::new();
    for it in taken {
        if let Some((section, _)) = it.id.rsplit_once('#') {
            *count.entry(section).or_default() += 1;
        }
    }
    let mut added: BTreeMap<String, u32> = BTreeMap::new();
    let mut out: Vec<Item> = Vec::new();
    let (mut section, mut in_table) = (String::new(), false);
    let mut row: Vec<String> = Vec::new();
    let mut flush = |row: &mut Vec<String>, section: &str, in_table: bool, out: &mut Vec<Item>| {
        let text = row.join(" ");
        let described = text.split_whitespace().any(|w| {
            let w = w.trim_matches(|c: char| !c.is_alphanumeric());
            COPY_WORDS.iter().any(|c| w.eq_ignore_ascii_case(c))
        });
        if in_table && described && spec_text::keyword(&text, &WORDS).is_none() {
            let key = format!("{}/{section}", doc.prefix);
            let extra = added.entry(key.clone()).or_default();
            *extra += 1;
            let n = count.get(key.as_str()).copied().unwrap_or(0) + *extra;
            out.push(Item {
                id: format!("{key}#{n}"),
                kind: "required",
                section: format!("{} \u{a7}{}", doc.label, section.to_ascii_uppercase()),
                facts: "cell=1".to_owned(),
                text,
            });
        }
        row.clear();
    };
    for l in lines {
        match l {
            Line::Text(t) if !row.is_empty() || cells(t.trim()).len() > 1 => {
                // A row's first line has two or more cells; its wrapped lines
                // follow with no blank between.
                let cs = cells(t.trim());
                let skip = usize::from(row.is_empty());
                row.extend(cs[skip..].iter().map(|c| (*c).to_owned()));
            }
            other => {
                flush(&mut row, &section, in_table, &mut out);
                match other {
                    Line::Heading(label) => {
                        section = label.to_ascii_lowercase();
                        in_table = false;
                    }
                    Line::Caption(true, _) => in_table = true,
                    Line::Text(_) => in_table = false,
                    _ => {}
                }
            }
        }
    }
    flush(&mut row, &section, in_table, &mut out);
    out
}

/// The lines of table `n`: from its caption to the next caption or heading.
fn table_lines(lines: &[Line], n: u32) -> Result<Vec<&str>, String> {
    let at = lines
        .iter()
        .position(|l| *l == Line::Caption(true, n))
        .ok_or_else(|| format!("Table {n} not found"))?;
    Ok(lines[at + 1..]
        .iter()
        .take_while(|l| matches!(l, Line::Blank | Line::Text(_)))
        .filter_map(|l| match l {
            Line::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect())
}

/// The size in bytes of each Message Type, from the allocation table.
fn mt_sizes(lines: &[Line], table: u32) -> Result<BTreeMap<u32, u32>, String> {
    let mut out = BTreeMap::new();
    for l in table_lines(lines, table)? {
        let t: Vec<&str> = l.split_whitespace().collect();
        if let [mt, bits, "bits", ..] = t[..]
            && let (Some(mt), Ok(bits)) = (spec_text::hex(mt, 1), bits.parse::<u32>())
        {
            out.insert(mt, bits / 8);
        }
    }
    if out.len() != 16 {
        return Err(format!(
            "Table {table} gives {} Message Type sizes, not 16",
            out.len()
        ));
    }
    Ok(out)
}

/// `mt` and `status` of a `Rows::Formats` line.
fn format_row(line: &str) -> Option<(u32, u32, Option<u32>)> {
    let t: Vec<&str> = line.split_whitespace().collect();
    let i = t.iter().position(|x| spec_text::hex(x, 1).is_some())?;
    let mt = spec_text::hex(t[i], 1)?;
    if mt == FLEX_MT {
        let bank = spec_text::hex(t.get(i + 5)?, 2)?;
        let status = spec_text::hex(t.get(i + 6)?, 2)?;
        return Some((mt, status, Some(bank)));
    }
    let mut k = i + 2;
    if t.get(k) == Some(&"status") {
        k += 1;
    }
    Some((mt, spec_text::hex(t.get(k)?, 2)?, None))
}

/// `(bank, status)` of a `Rows::FlexStatus` line.
fn flex_status_row(line: &str) -> Option<(u32, u32)> {
    let t: Vec<&str> = line.split_whitespace().collect();
    Some((
        spec_text::hex(t.first()?, 2)?,
        spec_text::hex(t.get(1)?, 2)?,
    ))
}

fn message_item(
    doc: &Doc,
    mt: u32,
    bank: Option<u32>,
    status: u32,
    bytes: u32,
    table: u32,
) -> Item {
    let (id_status, facts) = match bank {
        Some(b) => (
            format!("{b:02x}.{status:02x}"),
            format!("mt=0x{mt:x} bank=0x{b:x} status=0x{status:x} bytes={bytes}"),
        ),
        None => (
            format!("{status:x}"),
            format!("mt=0x{mt:x} status=0x{status:x} bytes={bytes}"),
        ),
    };
    Item {
        id: format!("{}/msg/{mt:x}/{id_status}", doc.prefix),
        kind: "",
        section: format!("{} Table {table}", doc.label),
        facts,
        text: String::new(),
    }
}

/// The Message Type rows and the defined-message rows.
fn message_items(
    lines: &[Line],
    doc: &Doc,
    mt_table: u32,
    tables: &[(u32, Rows)],
) -> Result<Vec<Item>, String> {
    let sizes = mt_sizes(lines, mt_table)?;
    let mut out: Vec<Item> = sizes
        .iter()
        .map(|(mt, bytes)| Item {
            id: format!("{}/mt/{mt:x}", doc.prefix),
            kind: "",
            section: format!("{} Table {mt_table}", doc.label),
            facts: format!("mt=0x{mt:x} bytes={bytes}"),
            text: String::new(),
        })
        .collect();
    for &(table, shape) in tables {
        let mut found = 0;
        for l in table_lines(lines, table)? {
            let row = match shape {
                Rows::Formats => format_row(l),
                Rows::FlexStatus => flex_status_row(l).map(|(b, s)| (FLEX_MT, s, Some(b))),
            };
            let Some((mt, status, bank)) = row else {
                continue;
            };
            let bytes = *sizes
                .get(&mt)
                .ok_or_else(|| format!("Table {table}: Message Type 0x{mt:x} has no size"))?;
            out.push(message_item(doc, mt, bank, status, bytes, table));
            found += 1;
        }
        if found == 0 {
            return Err(format!("Table {table} has no message rows"));
        }
    }
    Ok(out)
}

/// The `<doc>/hdr/<n>` rows: the `Value` line of the header table, eight
/// hex bytes.
fn header_items(lines: &[Line], doc: &Doc, table: u32) -> Result<Vec<Item>, String> {
    let values: Vec<u32> = table_lines(lines, table)?
        .into_iter()
        .find_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            (t.first() == Some(&"Value")).then(|| {
                t[1..]
                    .iter()
                    .map(|x| spec_text::hex(x, 2))
                    .collect::<Option<Vec<u32>>>()
            })
        })
        .flatten()
        .ok_or_else(|| format!("Table {table} has no Value line of hex bytes"))?;
    if values.len() != 8 {
        return Err(format!(
            "Table {table} gives {} header bytes, not 8",
            values.len()
        ));
    }
    Ok(values
        .iter()
        .enumerate()
        .map(|(i, v)| Item {
            id: format!("{}/hdr/{}", doc.prefix, i + 1),
            kind: "",
            section: format!("{} Table {table}", doc.label),
            facts: format!("byte={} value=0x{v:x}", i + 1),
            text: String::new(),
        })
        .collect())
}

/// Every item of the spec text: the keyword rows in document order, then the
/// document's table rows.
pub(super) fn extract(doc: &Doc, text: &str) -> Result<Vec<Item>, String> {
    let lines = match doc.tables {
        Tables::Named(n) => named_lines(text, n)?,
        _ => classify(&clean(text), doc)?,
    };
    let mut out = keyword_items(&lines, doc);
    let cells = cell_items(&lines, doc, &out);
    out.extend(cells);
    out.extend(match doc.tables {
        Tables::Messages { mt_table, messages } => message_items(&lines, doc, mt_table, messages)?,
        Tables::FileHeader { table } => header_items(&lines, doc, table)?,
        Tables::None | Tables::Named(_) => Vec::new(),
    });
    let mut seen = BTreeSet::new();
    for it in &out {
        if !seen.insert(it.id.as_str()) {
            return Err(format!("two items share the id `{}`", it.id));
        }
    }
    Ok(out)
}

mod docs;
#[cfg(test)]
mod tests;
