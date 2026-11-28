use super::{DIR, KINDS, MATURITIES, NA_KINDS, NotApplicable, Row, Side};
use crate::gates::Tree;
use crate::gates::lex::skipped;
use std::collections::{BTreeMap, BTreeSet};

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
pub(super) fn parse_file(path: &str, text: &str) -> (Vec<Row>, Vec<String>) {
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
pub(in crate::gates) fn pinned_clap_sys(lock: &str) -> Option<String> {
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
pub(in crate::gates) fn load(tree: &dyn Tree) -> (Vec<(String, Row)>, Vec<String>) {
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
