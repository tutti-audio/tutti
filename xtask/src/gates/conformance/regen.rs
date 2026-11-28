use super::parse::{parse_file, parse_row};
use super::{Row, Side};
use crate::gates::carry::{self, Fate};
use crate::gates::clap_sys::Item;
use crate::gates::{midi_spec, sf2_spec, vst3_sdk};
use std::collections::BTreeMap;

/// The comment lines that open `clap.txt`: its columns and id grammar.
const HEADER: &str = "\
# CLAP spec inventory. `cargo xtask conformance-inventory clap` regenerates the
# id, side and maturity columns from clap-sys; the others are edited by hand.
#
# Columns: id | side | maturity | kind | section | paraphrase | facts | na-kind | cite
#
# Ids, one per function pointer of a clap-sys vtable struct, event type or flag:
#   clap.ext.<ext>.<fn>                 plugin side of an extension (stable or draft)
#   clap.ext.<ext>.host.<fn>            host side of an extension
#   clap.ext.<ext>.<part>.<fn>          a sub-table: audio_ports_config.info,
#                                       context_menu.builder, undo.context, undo.delta, tuning.t
#   clap.factory.<name>[.<part>].<fn>   factories and what they hand out (core;
#                                       provider is plugin, indexer and
#                                       metadata_receiver are host)
#   clap.entry.<fn>, clap.plugin.<fn>, clap.host.<fn>   the core tables (core)
#   clap.stream.<istream|ostream>.<fn>, clap.events.<input|output>.<fn>   (core)
#   clap.event.<type>                   an event type, e.g. clap.event.note_on
#   clap.event.flag.<flag>              an event header flag, e.g. clap.event.flag.is_live
";

/// The text of `testing/conformance/clap.txt` for the extracted `items`: their
/// id, side and maturity, with every other column kept from `existing` for an
/// id that is still there; the rows keep the order of `items`.
pub(in crate::gates) fn regenerate(
    existing: Option<&str>,
    items: &[Item],
) -> Result<String, String> {
    let mut old: BTreeMap<String, Row> = BTreeMap::new();
    if let Some(text) = existing {
        let (rows, errs) = parse_file("clap.txt", text);
        if !errs.is_empty() {
            return Err(errs.join("\n"));
        }
        old.extend(rows.into_iter().map(|r| (r.id.clone(), r)));
    }
    let mut out = String::from(HEADER);
    for it in items {
        let row = match old.remove(&it.id) {
            Some(mut r) => {
                r.side = Side::Spec(it.side.to_owned());
                r.maturity = it.maturity.to_owned();
                r
            }
            None => Row {
                id: it.id.clone(),
                side: Side::Spec(it.side.to_owned()),
                maturity: it.maturity.to_owned(),
                kind: String::new(),
                section: String::new(),
                paraphrase: String::new(),
                facts: String::new(),
                na: None,
                digest: String::new(),
            },
        };
        out.push_str(&row.render());
        out.push('\n');
    }
    Ok(out)
}

/// The comment lines that open `vst3.txt`: its columns and id grammar.
const VST3_HEADER: &str = "\
# VST3 spec inventory. The spec is the SDK under `vendor/vst3-sdk/pluginterfaces`
# (base, vst, gui headers). `cargo xtask conformance-inventory vst3` regenerates
# the id, side, maturity and section columns from the headers; the others are
# edited by hand. `cargo xtask check-conformance` holds (id, side, maturity) to
# the headers.
#
# Columns: id | side | maturity | kind | section | paraphrase | facts | na-kind | cite
#
# Ids, one per pure virtual method of an interface (a class the header gives a
# DECLARE_CLASS_IID): vst3.<Interface>.<method>, e.g. vst3.IComponent.setActive.
# side: plugin for an interface the doc comment marks [plug imp], host for
# [host imp], core for both marks or none (an unmarked interface takes the side
# of the SDK interface it derives from). maturity: stable. section: the header's
# path under pluginterfaces/, with the [released: x.y.z] mark when it has one.
";

/// The text of `testing/conformance/vst3.txt` for the extracted `items`: their
/// id, side, maturity and section, with the other columns kept from `existing`
/// for an id that is still there. Also the ids added and removed.
pub(in crate::gates) fn regenerate_vst3(
    existing: Option<&str>,
    items: &[vst3_sdk::Item],
) -> Result<(String, Vec<String>, Vec<String>), String> {
    let mut old: BTreeMap<String, Row> = BTreeMap::new();
    if let Some(text) = existing {
        let (rows, errs) = parse_file("vst3.txt", text);
        if !errs.is_empty() {
            return Err(errs.join("\n"));
        }
        old.extend(rows.into_iter().map(|r| (r.id.clone(), r)));
    }
    let (mut out, mut added) = (String::from(VST3_HEADER), Vec::new());
    for it in items {
        let mut row = old.remove(&it.id).unwrap_or_else(|| {
            added.push(it.id.clone());
            Row {
                id: it.id.clone(),
                side: Side::Spec(String::new()),
                maturity: String::new(),
                kind: String::new(),
                section: String::new(),
                paraphrase: String::new(),
                facts: String::new(),
                na: None,
                digest: String::new(),
            }
        });
        row.side = Side::Spec(it.side.to_owned());
        row.maturity = it.maturity.to_owned();
        row.section = it.section.clone();
        out.push_str(&row.render());
        out.push('\n');
    }
    Ok((out, added, old.into_keys().collect()))
}

/// The comment lines that open a MIDI inventory file: its columns and id
/// grammar.
fn midi_header(doc: &midi_spec::Doc) -> String {
    let (p, label, title) = (doc.prefix, doc.label, doc.title);
    if let midi_spec::Tables::Named(_) = doc.tables {
        return format!(
            "\
# {title} inventory. The spec text is not in
# the repo: `TUTTI_MIDI_SPECS=<dir> cargo xtask conformance-inventory midi`
# regenerates the id, kind, section and facts columns from the OCR text of the
# spec in The Complete MIDI 1.0 (1996) book, a `{src}*.txt` in that directory;
# the other columns are written by hand, in our own words. `cargo xtask
# check-conformance` holds the generated columns to the text when
# `TUTTI_MIDI_SPECS` is set. Nothing of the spec is quoted here: ids, section
# names, keyword kinds and functional facts only.
#
# Columns: id | side | maturity | kind | section | paraphrase | facts | na-kind | cite | digest
# digest is the FNV-1a 64 of the row's sentence, whitespace normalised: the hand
# columns follow it when a revision shifts the numbers. side `changed` is a row
# whose sentence changed under hand columns: reread it and rewrite them.
#
# Ids:
#   {p}/<section>#<n>              the nth sentence under the named section (the
#                                 headings of the spec: header-chunk, track-chunks,
#                                 set-tempo, ..) that binds. The spec has no
#                                 table of conformance words: must, shall, always,
#                                 never, required and assumed give required,
#                                 should (and polite, idea) recommended, may and
#                                 optional permitted; `not required` reads as may
#                                 and `not optional` as must; the negated forms
#                                 keep the kind and carry the fact neg=1. A
#                                 sentence with none of them that states how the
#                                 format stores something (stored, represented,
#                                 begins, followed, ..) is required with the fact
#                                 def=1.
# side: {sides} (todo marks a row not yet read).
",
            src = doc.source,
            sides = doc.sides,
        );
    }
    let id = |shape: String, what: &str| format!("#   {shape:<30}{what}\n");
    let mut out = format!(
        "\
# {title} inventory. The spec text is not in
# the repo: `TUTTI_MIDI_SPECS=<dir> cargo xtask conformance-inventory midi`
# regenerates the id, kind, section and facts columns from the text that
# `pdftotext -layout` (26.01.0) makes of the {label} PDF in that directory (or a
# .txt of it beside the PDF); the other columns are written by hand, in our own
# words. `cargo xtask check-conformance` holds the generated columns to the
# text when `TUTTI_MIDI_SPECS` is set. Nothing of the spec is quoted here: ids,
# section numbers, keyword kinds and functional facts only.
#
# Columns: id | side | maturity | kind | section | paraphrase | facts | na-kind | cite | digest
# digest is the FNV-1a 64 of the row's sentence, whitespace normalised: the hand
# columns follow it when a revision shifts the numbers. side `changed` is a row
# whose sentence changed under hand columns: reread it and rewrite them.
#
# Ids:
"
    );
    out.push_str(&id(
        format!("{p}/<section>#<n>"),
        "the nth sentence under the numbered section",
    ));
    out.push_str(
        "\
#                                 (appendix letters lowercase: d.2.1) that carries
#                                 a conformance word. kind: shall is required,
#                                 should recommended, may permitted; the negated
#                                 forms keep the kind and carry the fact neg=1.
",
    );
    if let midi_spec::Tables::Messages { .. } = doc.tables {
        out.push_str(&id(
            format!("{p}/mt/<mt>"),
            "a Message Type: facts mt, bytes (the UMP size)",
        ));
        out.push_str(&id(
            format!("{p}/msg/<mt>/<status>"),
            "a defined message: <mt> and <status> are",
        ));
        out.push_str(
            "\
#                                 lowercase hex without 0x; Flex Data (mt d) is
#                                 <bank>.<status>, two hex digits each. Facts:
#                                 mt, status, bank (Flex Data), bytes
",
        );
    }
    out.push_str(doc.note);
    out.push_str(&format!(
        "# side: {} (todo marks a row not yet read).\n",
        doc.sides
    ));
    out
}

/// What a regeneration of an inventory with sentence rows did.
#[derive(Debug)]
pub(in crate::gates) struct Regen {
    /// The file's text.
    pub(in crate::gates) text: String,
    /// Ids no old row had (side `todo`, no paraphrase).
    pub(in crate::gates) added: Vec<String>,
    /// Ids whose old row had no hand columns and is gone.
    pub(in crate::gates) removed: Vec<String>,
    /// Ids whose sentence changed under hand columns: the row is kept with
    /// those columns blanked (side `changed`) and the gate refuses it.
    pub(in crate::gates) changed: Vec<(String, Row)>,
    /// Old rows with hand columns that no sentence claims any more.
    pub(in crate::gates) orphans: Vec<Row>,
    /// Which of `orphans` are in `text` already.
    placed: Vec<bool>,
}

impl Regen {
    fn new(text: String, carried: &carry::Carried) -> Self {
        Regen {
            text,
            added: Vec::new(),
            removed: carried.dropped.iter().map(|r| r.id.clone()).collect(),
            changed: Vec::new(),
            orphans: carried.orphans.clone(),
            placed: vec![false; carried.orphans.len()],
        }
    }

    /// Write the orphan rows of `section` (all that are left for `""`).
    fn write_orphans_of(&mut self, section: &str) {
        for (o, placed) in self.orphans.iter().zip(&mut self.placed) {
            if !*placed && (section.is_empty() || carry::section_of(&o.id) == section) {
                *placed = true;
                self.text.push_str(&o.render());
                self.text.push('\n');
            }
        }
    }

    /// The text of a MIDI inventory file for the extracted `items` of `doc`:
    /// their id, kind, section, facts and digest, with the other columns carried
    /// from `existing` by sentence ([`carry`]); a new sentence is a `todo` row with
    /// no paraphrase (the gate refuses it).
    pub(in crate::gates) fn midi(
        doc: &midi_spec::Doc,
        existing: Option<&str>,
        items: &[midi_spec::Item],
    ) -> Result<Self, String> {
        let old = parsed(doc.file, existing)?;
        let keys: Vec<(&str, &str)> = items
            .iter()
            .map(|i| (i.id.as_str(), i.text.as_str()))
            .collect();
        let carried = carry::carry(old, &keys);
        let mut out = Self::new(midi_header(doc), &carried);
        for (n, (it, fate)) in items.iter().zip(carried.fates).enumerate() {
            if fate == Fate::New {
                out.added.push(it.id.clone());
            }
            let (row, lost) = apply(
                fate,
                &Extracted {
                    id: &it.id,
                    kind: it.kind,
                    section: &it.section,
                    facts: &it.facts,
                    text: &it.text,
                },
            );
            if let Some(old) = lost {
                out.changed.push((it.id.clone(), old));
            }
            out.text.push_str(&row.render());
            out.text.push('\n');
            let section = carry::section_of(&it.id);
            if items
                .get(n + 1)
                .is_none_or(|next| carry::section_of(&next.id) != section)
            {
                out.write_orphans_of(section);
            }
        }
        out.write_orphans_of("");
        Ok(out)
    }

    /// The comment lines of `sf2.txt` are kept as written; this is the text of
    /// the file for the extracted `items`: every extracted row takes the extracted
    /// kind, section, facts and digest and keeps the rest of its hand columns while
    /// its sentence is the same ([`carry`]); a hand-written row and the comments
    /// stay where they are; a new extracted id is appended as a `todo` row with no
    /// paraphrase (the gate refuses it).
    pub(in crate::gates) fn sf2(existing: &str, items: &[sf2_spec::Item]) -> Result<Self, String> {
        let mut owned = Vec::new();
        for (n, line) in existing.lines().enumerate() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let row = parse_row(t).map_err(|e| format!("sf2.txt:{}: {e}", n + 1))?;
            if sf2_spec::owns(&row.id) {
                owned.push(row);
            }
        }
        let keys: Vec<(&str, &str)> = items
            .iter()
            .map(|i| (i.id.as_str(), i.text.as_str()))
            .collect();
        let carried = carry::carry(owned, &keys);
        let mut out = Self::new(String::new(), &carried);
        let mut rows: BTreeMap<&str, Row> = BTreeMap::new();
        for (it, fate) in items.iter().zip(carried.fates) {
            if fate == Fate::New {
                out.added.push(it.id.clone());
            }
            let (row, lost) = apply(
                fate,
                &Extracted {
                    id: &it.id,
                    kind: it.kind,
                    section: &it.section,
                    facts: &it.facts,
                    text: &it.text,
                },
            );
            if let Some(old) = lost {
                out.changed.push((it.id.clone(), old));
            }
            rows.insert(it.id.as_str(), row);
        }
        for line in existing.lines() {
            let t = line.trim();
            let id = t.split('|').next().unwrap_or("").trim();
            let is_orphan = t
                .split('|')
                .nth(1)
                .is_some_and(|c| matches!(Side::parse(c.trim()), Side::Orphan(_)));
            if is_orphan {
                continue;
            }
            if t.is_empty() || t.starts_with('#') || !sf2_spec::owns(id) {
                out.text.push_str(line);
                out.text.push('\n');
            } else if let Some(row) = rows.remove(id) {
                out.text.push_str(&row.render());
                out.text.push('\n');
            }
        }
        for it in items {
            if let Some(row) = rows.remove(it.id.as_str()) {
                out.text.push_str(&row.render());
                out.text.push('\n');
            }
        }
        out.write_orphans_of("");
        Ok(out)
    }
}

/// An extracted item's id, kind, section, facts and text.
struct Extracted<'a> {
    id: &'a str,
    kind: &'a str,
    section: &'a str,
    facts: &'a str,
    text: &'a str,
}

/// The row an item becomes: its generated columns over what `fate` keeps.
fn apply(fate: Fate, it: &Extracted) -> (Row, Option<Row>) {
    let blank = |side: Side| Row {
        id: it.id.to_owned(),
        side,
        maturity: String::new(),
        kind: String::new(),
        section: String::new(),
        paraphrase: String::new(),
        facts: String::new(),
        na: None,
        digest: String::new(),
    };
    let (mut row, lost) = match fate {
        Fate::Kept(mut r) => {
            r.id = it.id.to_owned();
            (r, None)
        }
        Fate::Changed(old) => (blank(Side::Changed), Some(old)),
        Fate::New => (blank(Side::Spec("todo".to_owned())), None),
    };
    row.kind = it.kind.to_owned();
    row.section = it.section.to_owned();
    row.facts = it.facts.to_owned();
    row.digest = carry::digest(it.text);
    (row, lost)
}

fn parsed(file: &str, existing: Option<&str>) -> Result<Vec<Row>, String> {
    let Some(text) = existing else {
        return Ok(Vec::new());
    };
    let (rows, errs) = parse_file(file, text);
    if errs.is_empty() {
        Ok(rows)
    } else {
        Err(errs.join("\n"))
    }
}
