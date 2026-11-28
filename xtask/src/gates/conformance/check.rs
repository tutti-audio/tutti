use super::parse::{load, pinned_clap_sys};
use super::{DIR, Row, TAG, VST3_PREFIX};
use crate::gates::clap_sys::{self, Item};
use crate::gates::lex::{self, blank_keeping_docs, skipped};
use crate::gates::{Report, Tree, carry, midi_spec, sf2_spec, vst3_sdk};
use std::collections::{BTreeMap, BTreeSet};

/// One `/// Spec:` tag: its line, the ids it claims, and the name of the `fn`
/// it documents (the first `fn` declared within a dozen lines below it, when
/// there is one; a tag above something else has none).
#[derive(Debug, PartialEq, Eq)]
pub(in crate::gates) struct Tag {
    pub(in crate::gates) line: usize,
    pub(in crate::gates) ids: Vec<String>,
    pub(in crate::gates) item: Option<String>,
}

/// The name a line declares with `fn`, when it does.
fn fn_name(line: &str) -> Option<String> {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with('#') {
        return None;
    }
    let after = lex::find_word(t, "fn").map(|at| &t[at + 2..])?;
    let name: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The tags of one file, and the problems with them.
pub(in crate::gates) fn tags(path: &str, text: &str) -> (Vec<Tag>, Vec<String>) {
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
pub(in crate::gates) struct Require {
    pub(in crate::gates) clap: bool,
    pub(in crate::gates) vst3: bool,
    pub(in crate::gates) midi: bool,
    pub(in crate::gates) sf2: bool,
}

impl Require {
    /// `require` demands the `clap-sys` source and the VST3 SDK; `require-all`
    /// also the MIDI and SoundFont spec texts.
    pub(in crate::gates) fn parse(value: Option<&str>) -> Require {
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

pub(in crate::gates) fn check(
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
