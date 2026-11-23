//! The second scan of `check-buffers`: a hand interleave loop or a hand
//! frame-times-width length in `src` is a row of [`LIST`].
//!
//! Interleave, deinterleave, convert and fold go through `tutti_buffers`, and
//! a frame count crosses to a slice length only through
//! `Samples::interleaved_len`. This scan lists the places that still do it by
//! hand so the list only shrinks.
//!
//! **Where it reads.** Every `crates/*/src/**/*.rs`, comments and literals
//! blanked, except `crates/tutti-buffers/src` (where the loops live), `tests`,
//! `benches` and `examples` directories, `tests.rs` files, files a
//! `#[cfg(test)] mod` declaration reaches, and `#[cfg(test)]` items.
//!
//! **What it finds** (a width word is `width`, `channels`, `channel_count`,
//! `n_channels`, `num_channels`, `nch`, `chans`, `ch`, `w` or `stride`, or a
//! call `.count()` on a receiver whose name holds `layout`, `.channels()` or
//! `.width()`; a frames word is `frames`, `frame_count` or a name ending
//! `_frames`; each may be a path, a call, `.get()` or `usize::from(..)` of
//! those):
//!
//! - **index**: an index `[X * W + Y]` or `[Y + X * W]` with `W` a width word
//!   (`out[f * width + c]`);
//! - **chunks**: `as_chunks::<N>` or `as_chunks_mut::<N>` with `N` the
//!   literal 2, 6 or 8 (stereo, 5.1, 7.1), in a function that names none of `from_le_bytes`,
//!   `from_be_bytes`, `from_ne_bytes`, `to_le_bytes`, `to_be_bytes` or
//!   `to_ne_bytes`; and `chunks_exact(W)` or `chunks_exact_mut(W)` with `W` a
//!   width word;
//! - **length**: `A * B` with one side a frames word and the other a width
//!   word, or either side a layout `.count()` (`frames * channels`,
//!   `n * usize::from(layout.count())`).
//!
//! **What it misses.** It is a grep, not a proof: a loop over `step_by`, a
//! `chunks(..)` or `chunks_exact(len)` whose length is not a width word, a
//! `chunks_exact(<literal>)` or `chunks_exact(a * b)`, an
//! index whose stride is `cols`, `n` or a local alias, a product split over
//! lines or through a temporary (`let s = width; frames * s`), a `.count()`
//! on a receiver not named for a layout, a loop that zips planes by hand
//! with no multiplication, and an `as_chunks` of any other literal (quad is 4) or a
//! constant. A clean run says no listed shape remains, nothing more.
//!
//! **Keys.** `<file>::<fn>` for a free function and `<file>::<Owner>::<fn>`
//! for a method (the owner as in `check-buffers`), `<file>::-` for a hit
//! outside any function. Several hits in one function share one row.
//!
//! **List.** [`LIST`] holds `<key>  <reason>` lines, `#` and blank lines
//! aside, the reason `pending`, `not-interleave: <why>` or `atomic: <why>`. A
//! `not-interleave` hit's stride is not a channel interleave (an FFT's complex
//! re/im packing, a matrix row); an `atomic` hit walks an interleave of atomic
//! cells (a ring the audio thread reads while another thread writes), which
//! no `tutti_buffers` view borrows. No `tutti_buffers` call replaces either. A free-text
//! `measured:` is refused. The gate fails on a hit
//! with no row, on a row with no hit (delete it), on an unknown reason and on
//! a key listed twice.

use super::buffers::{blocks, item_end, reason_error};
use super::lex::{
    blank, find_word_from, ident_at, is_test_file, matching, skipped_library, test_ranges, word,
};
use super::test_time::cfg_test_files;
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// The list of sites, `<key>  <reason>` per line.
pub(crate) const LIST: &str = "xtask/buffers-loops.txt";

/// The fewest source files a real run reads; fewer means the walk is broken.
/// It counts files, not sites: the list shrinks to zero as sites are fixed.
pub(super) const MIN_FILES: usize = 500;

const WIDTH_WORDS: [&str; 10] = [
    "width",
    "channels",
    "channel_count",
    "n_channels",
    "num_channels",
    "nch",
    "chans",
    "ch",
    "w",
    "stride",
];

const BYTE_CODECS: [&str; 6] = [
    "from_le_bytes",
    "from_be_bytes",
    "from_ne_bytes",
    "to_le_bytes",
    "to_be_bytes",
    "to_ne_bytes",
];

/// An operand reduced to what it names: `usize::from(..)` and `.get()` peeled.
/// Returns the path before a trailing call and the call's name, if any.
fn peel(op: &str) -> (&str, Option<&str>) {
    let mut t = op.trim();
    if let Some(inner) = t
        .strip_prefix("usize::from(")
        .and_then(|r| r.strip_suffix(')'))
    {
        t = inner.trim();
    }
    while let Some(r) = t.strip_suffix(".get()") {
        t = r;
    }
    match t.strip_suffix("()") {
        Some(call) => match call.rsplit_once('.') {
            Some((recv, name)) => (recv, Some(name)),
            None => (call, Some(call)),
        },
        None => (t, None),
    }
}

fn last_segment(path: &str) -> &str {
    path.rsplit(['.', ':']).next().unwrap_or(path)
}

fn width_word(op: &str) -> bool {
    match peel(op) {
        (recv, Some("count")) => last_segment(recv).contains("layout"),
        (_, Some("channels" | "width")) => true,
        (_, Some(_)) => false,
        (path, None) => {
            let w = last_segment(path);
            WIDTH_WORDS.contains(&w)
        }
    }
}

fn layout_count(op: &str) -> bool {
    matches!(peel(op), (recv, Some("count")) if last_segment(recv).contains("layout"))
}

fn frames_word(op: &str) -> bool {
    let (path, call) = peel(op);
    let w = call.unwrap_or_else(|| last_segment(path));
    w == "frames" || w == "frame_count" || w.ends_with("_frames")
}

/// Splits `text` at the top-level (outside `()`, `[]`, `{}`) occurrences of `sep`.
fn split_depth0(text: &str, sep: u8) -> Vec<&str> {
    let b = text.as_bytes();
    let (mut depth, mut from) = (0i32, 0);
    let mut out = Vec::new();
    for i in 0..b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            c if c == sep && depth == 0 => {
                out.push(&text[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push(&text[from..]);
    out
}

fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Whether the index body is `X * W + Y` or `Y + X * W` with `W` a width word.
fn index_shape(body: &str) -> bool {
    let body = squeeze(body);
    if body.contains("..") || body.contains(';') || body.contains('-') {
        return false;
    }
    let terms = split_depth0(&body, b'+');
    if terms.len() != 2 || terms.iter().any(|t| t.is_empty()) {
        return false;
    }
    terms.iter().enumerate().any(|(i, t)| {
        let factors = split_depth0(t, b'*');
        factors.len() == 2
            && factors.iter().all(|f| !f.is_empty())
            && (width_word(factors[0]) || width_word(factors[1]))
            && !split_depth0(terms[1 - i], b'*')
                .iter()
                .skip(1)
                .any(|_| true)
    })
}

/// The operand ending at `end` (exclusive), in text without whitespace
/// handling: a path, call groups and `usize::from(..)` kept.
fn operand_before(code: &str, end: usize) -> &str {
    let b = code.as_bytes();
    let mut i = end;
    while i > 0 && b[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    let stop = i;
    loop {
        while i > 0 && (word(b[i - 1]) || matches!(b[i - 1], b'.' | b':')) {
            i -= 1;
        }
        if i > 0 && b[i - 1] == b')' {
            let mut depth = 0i32;
            let mut j = i;
            while j > 0 {
                j -= 1;
                match b[j] {
                    b')' => depth += 1,
                    b'(' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            i = j;
            continue;
        }
        break;
    }
    &code[i..stop]
}

/// The operand starting at `from`: a path then call groups.
fn operand_after(code: &str, from: usize) -> &str {
    let b = code.as_bytes();
    let mut i = from;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    loop {
        while i < b.len() && (word(b[i]) || matches!(b[i], b'.' | b':')) {
            i += 1;
        }
        if i < b.len()
            && b[i] == b'('
            && let Some(close) = matching(code, i, b'(', b')')
        {
            i = close + 1;
            if b.get(i) == Some(&b'.') {
                continue;
            }
        }
        break;
    }
    &code[start..i]
}

/// A function: where its body is and what it is keyed by.
pub(super) struct Func {
    pub(super) start: usize,
    pub(super) close: usize,
    pub(super) key: String,
}

pub(super) fn functions(code: &str) -> Vec<Func> {
    let blocks = blocks(code);
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "fn", from) {
        from = at + 2;
        let (name, name_end) = ident_at(code, from);
        if name.is_empty() {
            continue;
        }
        let end = item_end(code, name_end);
        if code.as_bytes().get(end) != Some(&b'{') {
            continue;
        }
        let Some(close) = matching(code, end, b'{', b'}') else {
            continue;
        };
        let owner = blocks
            .iter()
            .filter(|b| b.open < at && at < b.close)
            .max_by_key(|b| b.open);
        let key = match owner {
            Some(b) => format!("{}::{name}", b.owner),
            None => name.to_owned(),
        };
        out.push(Func {
            start: at,
            close,
            key,
        });
    }
    out
}

/// The keys of one file's hits and the shapes each holds.
fn scan(
    path: &str,
    src: &str,
    whole_file_is_test: bool,
) -> BTreeMap<String, BTreeSet<&'static str>> {
    let mut hits: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    if whole_file_is_test {
        return hits;
    }
    let code = blank(src);
    let tests = test_ranges(&code);
    let funcs = functions(&code);
    let b = code.as_bytes();
    let mut add = |at: usize, shape: &'static str| {
        if tests.iter().any(|&(a, z)| a <= at && at <= z) {
            return;
        }
        let key = match funcs
            .iter()
            .filter(|f| f.start < at && at < f.close)
            .max_by_key(|f| f.start)
        {
            Some(f) => format!("{path}::{}", f.key),
            None => format!("{path}::-"),
        };
        hits.entry(key).or_default().insert(shape);
    };

    for (i, &c) in b.iter().enumerate() {
        match c {
            b'[' if i > 0 && (word(b[i - 1]) || matches!(b[i - 1], b')' | b']')) => {
                if let Some(close) = matching(&code, i, b'[', b']')
                    && index_shape(&code[i + 1..close])
                {
                    add(i, "index");
                }
            }
            b'*' => {
                let prev = b[..i].iter().rev().find(|c| !c.is_ascii_whitespace());
                let next = b.get(i + 1);
                let binary = prev.is_some_and(|&p| word(p) || p == b')' || p == b']');
                if !binary || matches!(next, Some(b'=' | b'/' | b'*')) {
                    continue;
                }
                let (l, r) = (operand_before(&code, i), operand_after(&code, i + 1));
                if (frames_word(l) && width_word(r))
                    || (width_word(l) && frames_word(r))
                    || layout_count(l)
                    || layout_count(r)
                {
                    add(i, "length");
                }
            }
            _ => {}
        }
    }
    for name in ["as_chunks", "as_chunks_mut"] {
        let mut from = 0;
        while let Some(at) = find_word_from(&code, name, from) {
            from = at + name.len();
            let rest = &code[from..];
            let Some(arg) = rest.strip_prefix("::<") else {
                continue;
            };
            let n: String = arg.chars().take_while(char::is_ascii_digit).collect();
            let literal = arg[n.len()..].starts_with('>')
                && n.parse::<u32>().is_ok_and(|n| matches!(n, 2 | 6 | 8));
            let bytes = funcs
                .iter()
                .filter(|f| f.start < at && at < f.close)
                .max_by_key(|f| f.start)
                .is_some_and(|f| {
                    BYTE_CODECS
                        .iter()
                        .any(|w| code[f.start..f.close].contains(w))
                });
            if literal && !bytes {
                add(at, "chunks");
            }
        }
    }
    for name in ["chunks_exact", "chunks_exact_mut"] {
        let mut from = 0;
        while let Some(at) = find_word_from(&code, name, from) {
            from = at + name.len();
            if b.get(from) != Some(&b'(') {
                continue;
            }
            let Some(close) = matching(&code, from, b'(', b')') else {
                continue;
            };
            let arg = code[from + 1..close].trim();
            if !arg.is_empty() && !arg.contains('*') && width_word(arg) {
                add(at, "chunks");
            }
        }
    }
    hits
}

/// The files the gate reads.
fn source_files(tree: &dyn Tree) -> Vec<String> {
    tree.files("crates", &skipped_library)
        .into_iter()
        .filter(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            p.ends_with(".rs")
                && parts.get(2) == Some(&"src")
                && parts[1] != "tutti-buffers"
                && parts.last() != Some(&"tests.rs")
        })
        .collect()
}

const LOOP_KINDS: [&str; 3] = ["pending", "not-interleave", "atomic"];

/// The list's rows, key to reason; the errors name the line.
fn parse(text: &str) -> Result<BTreeMap<String, String>, Vec<String>> {
    let mut rows = BTreeMap::new();
    let mut errors = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = n + 1;
        let (key, reason) = match line.split_once(char::is_whitespace) {
            Some((k, r)) => (k, r.trim()),
            None => (line, ""),
        };
        if let Some(why) = reason_error(reason, &LOOP_KINDS) {
            errors.push(format!("  {LIST}:{at}: `{key}`: {why}"));
        }
        if rows.insert(key.to_owned(), reason.to_owned()).is_some() {
            errors.push(format!("  {LIST}:{at}: `{key}` is listed twice"));
        }
    }
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

pub(super) fn check(tree: &dyn Tree, min_files: usize) -> Report {
    let mut r = Report::default();
    let Some(text) = tree.read(LIST) else {
        r.fail(format!("check-buffers: FAIL: {LIST} is missing"));
        return r;
    };
    let rows = match parse(&text) {
        Ok(rows) => rows,
        Err(errors) => {
            r.fail(format!("check-buffers: FAIL ({LIST} is malformed):"));
            errors.into_iter().for_each(|e| r.fail(e));
            return r;
        }
    };
    let files: BTreeMap<String, String> = source_files(tree)
        .into_iter()
        .filter_map(|p| tree.read(&p).map(|t| (p, t)))
        .collect();
    if files.len() < min_files {
        r.fail(format!(
            "check-buffers: FAIL: {} source files read, at least {min_files} expected; the walk is broken",
            files.len()
        ));
        return r;
    }
    let reached = cfg_test_files(&files);
    let mut sites: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    for (path, text) in &files {
        let whole = is_test_file(path) || reached.contains(path);
        for (k, shapes) in scan(path, text, whole) {
            sites.entry(k).or_default().extend(shapes);
        }
    }
    let unlisted: Vec<_> = sites.keys().filter(|k| !rows.contains_key(*k)).collect();
    let stale: Vec<_> = rows.keys().filter(|k| !sites.contains_key(*k)).collect();
    if unlisted.is_empty() && stale.is_empty() {
        r.say("check-buffers: hand loops OK");
    } else {
        if !unlisted.is_empty() {
            r.fail(format!(
                "check-buffers: FAIL (a hand interleave loop or frame-times-width length; go \
                 through tutti_buffers (interleave, deinterleave, Fold, Samples::interleaved_len), \
                 or add `<key>  <reason>` to {LIST}):"
            ));
            for k in unlisted {
                let shapes: Vec<_> = sites[k].iter().copied().collect();
                r.fail(format!("  {k}: {}", shapes.join(", ")));
            }
        }
        if !stale.is_empty() {
            r.fail(format!(
                "check-buffers: FAIL (a row of {LIST} names no hit; delete it):"
            ));
            for k in stale {
                r.fail(format!("  {k}"));
            }
        }
    }
    let pending = rows.values().filter(|v| *v == "pending").count();
    r.say(format!(
        "check-buffers: {} hand-loop sites, {} rows in {LIST} ({pending} pending)",
        sites.len(),
        rows.len()
    ));
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const F: &str = "crates/a/src/lib.rs";

    fn keys(src: &str) -> Vec<String> {
        scan(F, src, false).into_keys().collect()
    }

    fn hit(body: &str) -> bool {
        !keys(&format!("fn f() {{ {body} }}\n")).is_empty()
    }

    fn run(list: &str, files: &[(&str, &str)]) -> Report {
        let mut all = vec![(LIST, list)];
        all.extend_from_slice(files);
        check(&Mem::with(&all), 0)
    }

    #[test]
    fn an_index_of_frame_times_width_plus_channel_is_a_hit() {
        for b in [
            "out[f * width + c] = x;",
            "out[i * channels + ch] = x;",
            "out[c + f * self.channels] = x;",
            "out[frame * usize::from(layout.count()) + c] = x;",
            "out[ f*stride + c ] = x;",
        ] {
            assert!(hit(b), "{b}");
        }
    }

    #[test]
    fn an_index_that_is_not_frame_times_width_is_not_a_hit() {
        for b in [
            "let v = x[i * 2 + 1];",
            "let v = x[i * len + j];",
            "let v = x[f * width];",
            "let v = x[..f * width + c];",
            "let v = [f * width + c; 4];",
            "let v = x[f * width + c * 2];",
            "let v = x[f * width - c];",
        ] {
            assert!(!hit(b), "{b}");
        }
    }

    #[test]
    fn as_chunks_of_a_small_literal_is_a_hit_and_a_byte_decode_is_not() {
        assert!(hit("for p in out.as_chunks::<2>().0 {}"));
        assert!(hit("for p in out.as_chunks_mut::<6>().0 {}"));
        assert!(!hit("for p in out.as_chunks::<4>().0 {}"));
        assert!(!hit("for p in out.as_chunks::<16>().0 {}"));
        assert!(!hit("for p in out.as_chunks::<N>().0 {}"));
        assert!(!hit(
            "for p in b.as_chunks::<4>().0 { u32::from_le_bytes(*p); }"
        ));
    }

    #[test]
    fn chunks_exact_of_a_width_word_is_a_hit_and_of_another_length_is_not() {
        assert!(hit("for f in out.chunks_exact(ch) {}"));
        assert!(hit("for f in out.chunks_exact_mut(self.stride) {}"));
        assert!(hit("for f in out.chunks_exact(width) {}"));
        assert!(!hit("for f in out.chunks_exact(len) {}"));
        assert!(!hit("for f in out.chunks_exact(2 * len) {}"));
        assert!(!hit("for f in out.chunks(ch) {}"));
    }

    #[test]
    fn frames_times_a_width_or_a_layout_count_is_a_hit() {
        for b in [
            "let n = frames * channels;",
            "let n = channels * block_frames;",
            "let n = out.frames().get() * width;",
            "let n = frames * block.channels();",
            "let n = max_block * usize::from(layout.count());",
            "let n = layout.count() * 2;",
        ] {
            assert!(hit(b), "{b}");
        }
    }

    #[test]
    fn a_product_that_is_not_frames_times_a_width_is_not_a_hit() {
        for b in [
            "let n = frames * 2;",
            "let n = len * width;",
            "let n = frames * rate;",
            "let n = it.count() * 2;",
            "let n = *frames;",
            "frames *= channels;",
            "let n = a * b; let m = frames + channels;",
        ] {
            assert!(!hit(b), "{b}");
        }
    }

    #[test]
    fn a_key_is_the_file_the_owner_and_the_function() {
        let src = "impl W { fn a(&self) { let n = frames * channels; } }\n\
                   fn free() { let n = frames * channels; let m = frames * width; }\n\
                   const N: usize = frames * width;\n";
        assert_eq!(
            keys(src),
            [
                format!("{F}::-"),
                format!("{F}::W::a"),
                format!("{F}::free")
            ]
        );
    }

    #[test]
    fn a_test_item_a_test_file_and_the_buffers_crate_are_skipped() {
        let hit_src = "fn f() { let n = frames * channels; }\n";
        let r = run(
            "",
            &[
                (
                    F,
                    "#[cfg(test)]\nmod t { fn f() { let n = frames * channels; } }\n",
                ),
                ("crates/a/src/tests.rs", hit_src),
                ("crates/a/src/m/tests/x.rs", hit_src),
                ("crates/a/tests/x.rs", hit_src),
                ("crates/tutti-buffers/src/lib.rs", hit_src),
            ],
        );
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_file_a_cfg_test_mod_declaration_reaches_is_skipped() {
        let hit_src = "fn f() { let n = frames * channels; }\n";
        let r = run(
            "",
            &[
                (F, "#[cfg(test)]\nmod helpers;\n"),
                ("crates/a/src/helpers.rs", hit_src),
            ],
        );
        assert!(r.passed(), "{:?}", r.findings);
        let r = run(
            "",
            &[(F, "mod helpers;\n"), ("crates/a/src/helpers.rs", hit_src)],
        );
        assert_eq!(r.findings[1..], ["  crates/a/src/helpers.rs::f: length"]);
    }

    #[test]
    fn a_cfg_test_fn_is_skipped_and_its_neighbour_is_not() {
        let src = "#[cfg(test)]\nfn t() { let n = frames * channels; }\nfn p() { let n = frames * channels; }\n";
        assert_eq!(keys(src), [format!("{F}::p")]);
    }

    #[test]
    fn a_comment_or_literal_is_not_a_hit() {
        assert!(
            keys("// let n = frames * channels;\nfn f() { let s = \"frames * channels\"; }\n")
                .is_empty()
        );
    }

    #[test]
    fn a_hit_with_no_row_fails_and_names_its_shape() {
        let r = run("", &[(F, "fn f() { let n = frames * channels; }\n")]);
        assert_eq!(r.findings[1..], ["  crates/a/src/lib.rs::f: length"]);
    }

    #[test]
    fn a_listed_hit_passes() {
        let r = run(
            "crates/a/src/lib.rs::f  pending\n",
            &[(F, "fn f() { let n = frames * channels; }\n")],
        );
        assert!(r.passed());
        assert_eq!(
            r.out[1],
            "check-buffers: 1 hand-loop sites, 1 rows in xtask/buffers-loops.txt (1 pending)"
        );
    }

    #[test]
    fn a_row_with_no_hit_is_stale_and_fails() {
        let r = run(
            "crates/a/src/lib.rs::gone  pending\n",
            &[(F, "fn f() {}\n")],
        );
        assert!(!r.passed());
        assert_eq!(r.findings[1..], ["  crates/a/src/lib.rs::gone"]);
    }

    #[test]
    fn an_unknown_reason_a_duplicate_and_a_missing_list_fail() {
        for list in [
            "crates/a/src/lib.rs::f  later\n",
            "crates/a/src/lib.rs::f  measured: a fixed 5.1 table\n",
            "crates/a/src/lib.rs::f  spec-f32: control\n",
            "crates/a/src/lib.rs::f  not-interleave\n",
            "crates/a/src/lib.rs::f  atomic\n",
            "crates/a/src/lib.rs::f  not-audio: a 5.1 mixing table\n",
            "crates/a/src/lib.rs::f  pending\ncrates/a/src/lib.rs::f  pending\n",
        ] {
            let r = run(list, &[(F, "fn f() { let n = frames * channels; }\n")]);
            assert!(!r.passed(), "{list}");
        }
        assert!(!check(&Mem::with(&[(F, "fn f() {}\n")]), 0).passed());
        for reason in [
            "not-interleave: complex re/im packing",
            "atomic: a ring of atomic cells",
        ] {
            let ok = run(
                &format!("crates/a/src/lib.rs::f  {reason}\n"),
                &[(F, "fn f() { let n = frames * channels; }\n")],
            );
            assert!(ok.passed(), "{reason}");
        }
    }

    #[test]
    fn a_walk_that_reads_too_few_files_fails() {
        let r = check(&Mem::with(&[(LIST, ""), (F, "fn f() {}\n")]), 2);
        assert!(r.findings[0].contains("the walk is broken"));
    }
}
