//! Rust source with its comments and the contents of its literals blanked.
//!
//! Every gate that looks for a token in code (`unsafe`, `extern "C" fn`, a
//! `pub use`) reads the blanked text, so prose and strings that say the word are
//! not code. Byte offsets and newlines are kept, so an offset in the blanked
//! text is the same offset in the source and a line number holds.

/// A byte that continues an identifier.
pub(super) fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Python's `\w`: a character that continues an identifier, Unicode included
/// (the gates written to a `\w` pattern read text, not just ASCII code).
pub(super) fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Directories no gate enters, wherever they sit.
pub(super) fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git" | "node_modules")
}

/// [`skipped`], and the directories that hold no library code.
pub(super) fn skipped_library(dir: &str) -> bool {
    skipped(dir) || matches!(dir, "tests" | "benches" | "examples")
}

/// Whether `path` is a test file: `tests.rs`, or under a `tests` directory.
pub(super) fn is_test_file(path: &str) -> bool {
    path.split('/').any(|c| c == "tests" || c == "tests.rs")
}

/// The identifier that starts at `from` after skipping whitespace, and the
/// offset after it; empty when none starts there.
pub(super) fn ident_at(code: &str, from: usize) -> (&str, usize) {
    let rest = &code[from..];
    let start = from + (rest.len() - rest.trim_start().len());
    let len = code[start..].bytes().take_while(|&c| word(c)).count();
    (&code[start..start + len], start + len)
}

/// `text` without its leading `#[..]` attributes.
pub(super) fn strip_attrs(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        if !text.starts_with("#[") {
            return text;
        }
        match matching(text, 1, b'[', b']') {
            Some(close) => text = &text[close + 1..],
            None => return text,
        }
    }
}

/// `src` with `//` and nested `/* */` comments, and what sits between the
/// quotes of a string, raw string, byte string or char literal, replaced by
/// spaces (newlines kept). The quotes stay, so `""` still marks a literal.
pub(super) fn blank(src: &str) -> String {
    split(src, false).0
}

/// [`blank`] that leaves `///` doc comments (not `////`) in place, so a gate
/// can read a tag a doc comment carries while every other comment and every
/// literal stays blanked. A `///` inside a string is still blanked.
pub(super) fn blank_keeping_docs(src: &str) -> String {
    split(src, true).0
}

/// `src` with everything but its `//` and `/* */` comments replaced by spaces
/// (newlines kept): the prose of a source file, at its own offsets.
pub(super) fn comments(src: &str) -> String {
    split(src, false).1
}

/// The blanked code and the comments alone, both at the source's offsets.
fn split(src: &str, keep_docs: bool) -> (String, String) {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut prose: Vec<u8> = b
        .iter()
        .map(|&c| if c == b'\n' { b'\n' } else { b' ' })
        .collect();
    let n = b.len();
    let wipe = |out: &mut [u8], from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = src[i..].find('\n').map_or(n, |j| i + j);
            prose[i..end].copy_from_slice(&b[i..end]);
            let line = &src[i..end];
            if !(keep_docs && line.starts_with("///") && !line.starts_with("////")) {
                wipe(&mut out, i, end);
            }
            i = end;
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 1;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            let j = j.min(n);
            prose[i..j].copy_from_slice(&b[i..j]);
            wipe(&mut out, i, j);
            i = j;
        } else if word(c) {
            // A whole identifier, unless it opens a raw or byte literal.
            let start = i;
            let mut j = i;
            while j < n && word(b[j]) {
                j += 1;
            }
            let ident = &b[start..j];
            if (ident == b"r" || ident == b"br") && matches!(b.get(j), Some(b'"' | b'#')) {
                let hashes = b[j..].iter().take_while(|&&h| h == b'#').count();
                if b.get(j + hashes) == Some(&b'"') {
                    let body = j + hashes + 1;
                    let close: Vec<u8> = std::iter::once(b'"')
                        .chain(std::iter::repeat_n(b'#', hashes))
                        .collect();
                    let end = b[body..]
                        .windows(close.len())
                        .position(|w| w == close.as_slice())
                        .map_or(n, |p| body + p);
                    wipe(&mut out, body, end);
                    i = (end + close.len()).min(n);
                    continue;
                }
            }
            if ident == b"b" && b.get(j) == Some(&b'"') {
                i = j;
                continue;
            }
            if ident == b"b" && b.get(j) == Some(&b'\'') {
                i = j;
                continue;
            }
            i = j;
        } else if c == b'"' {
            let mut j = i + 1;
            while j < n && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = j.min(n);
            wipe(&mut out, i + 1, end);
            i = (end + 1).min(n);
        } else if c == b'\'' {
            match char_literal_end(b, i) {
                Some(end) => {
                    wipe(&mut out, i + 1, end - 1);
                    i = end;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    let utf8 = "blanking keeps UTF-8: it only writes spaces over whole spans";
    (
        String::from_utf8(out).expect(utf8),
        String::from_utf8(prose).expect(utf8),
    )
}

/// The index after a char literal opening at `at` (`'x'`, `'\n'`, `'\x41'`,
/// `'\u{1F600}'`), or `None` when the quote is a lifetime's.
fn char_literal_end(b: &[u8], at: usize) -> Option<usize> {
    let rest = &b[at + 1..];
    let first = *rest.first()?;
    if first == b'\\' {
        let esc = *rest.get(1)?;
        let len = match esc {
            b'x' => 4,
            b'u' => {
                let close = rest.iter().position(|&c| c == b'}')?;
                close + 1
            }
            _ => 2,
        };
        (rest.get(len) == Some(&b'\'')).then_some(at + 1 + len + 1)
    } else if first == b'\'' || first == b'\n' {
        None
    } else {
        // One UTF-8 scalar.
        let len = match first {
            0..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        (rest.get(len) == Some(&b'\'')).then_some(at + 1 + len + 1)
    }
}

/// The line (1-based) of a byte offset.
pub(super) fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()[..at]
        .iter()
        .filter(|&&c| c == b'\n')
        .count()
        + 1
}

/// The offset of the first whole-word `needle` in `hay`.
pub(super) fn find_word(hay: &str, needle: &str) -> Option<usize> {
    find_word_from(hay, needle, 0)
}

pub(super) fn find_word_from(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let b = hay.as_bytes();
    let mut at = from;
    while let Some(p) = hay.get(at..)?.find(needle) {
        let s = at + p;
        let e = s + needle.len();
        let before = s == 0 || !word(b[s - 1]);
        let after = e >= b.len() || !word(b[e]);
        if before && after {
            return Some(s);
        }
        at = s + 1;
    }
    None
}

/// The index of the closer matching the opener at `open` (in blanked text, so
/// no literal can hold a bracket), or `None` when it never closes.
pub(super) fn matching(blanked: &str, open: usize, o: u8, c: u8) -> Option<usize> {
    let mut depth = 0;
    for (i, &x) in blanked.as_bytes().iter().enumerate().skip(open) {
        if x == o {
            depth += 1;
        } else if x == c {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// The byte ranges of test-only items, in blanked text: an item behind
/// `#[cfg(test)]` or `#[cfg(all(.., test, ..))]` (any spacing), and the enclosing
/// block, or the rest of the file at its top level, of an inner `#![cfg(test)]`.
/// An item runs to its body's closing brace or to the `;` that ends it, skipping
/// `()` and `[]` groups so a `;` in an array type or a `where` bound does not end
/// it. `cfg(any(test, ..))` and `cfg(not(..))` are not test-only and are not ranges.
pub(super) fn test_ranges(code: &str) -> Vec<(usize, usize)> {
    let b = code.as_bytes();
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut from = 0;
    while let Some(p) = code[from..].find('#') {
        let at = from + p;
        from = at + 1;
        let mut i = skip_ws(b, at + 1);
        let inner = b.get(i) == Some(&b'!');
        if inner {
            i = skip_ws(b, i + 1);
        }
        if b.get(i) != Some(&b'[') {
            continue;
        }
        let Some(close) = matching(code, i, b'[', b']') else {
            continue;
        };
        let body = code[i + 1..close].trim();
        let Some(pred) = body
            .strip_prefix("cfg")
            .map(str::trim_start)
            .and_then(|r| r.strip_prefix('('))
            .and_then(|r| r.trim_end().strip_suffix(')'))
        else {
            continue;
        };
        let pred: String = pred.split_whitespace().collect();
        if !test_only(&pred) {
            continue;
        }
        if out.last().is_some_and(|&(_, e)| at < e) {
            continue;
        }
        let range = if inner {
            let open = enclosing_open(code, at);
            let end = open
                .and_then(|o| matching(code, o, b'{', b'}'))
                .map_or(code.len(), |c| c + 1);
            (open.unwrap_or(0), end)
        } else {
            (at, item_end(code, close + 1))
        };
        from = range.1.max(from);
        out.push(range);
    }
    out
}

/// Whether a `cfg` predicate (whitespace removed) holds only under `cfg(test)`.
fn test_only(pred: &str) -> bool {
    if pred == "test" {
        return true;
    }
    let Some(args) = pred.strip_prefix("all(").and_then(|r| r.strip_suffix(')')) else {
        return false;
    };
    split_args(args).into_iter().any(test_only)
}

/// `args` split at the commas outside parentheses.
fn split_args(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0usize, 0);
    for (i, c) in args.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&args[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&args[start..]);
    out
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// The offset of the `{` that encloses `at`, or `None` at the top level.
fn enclosing_open(code: &str, at: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, &c) in code.as_bytes()[..at].iter().enumerate().rev() {
        match c {
            b'}' => depth += 1,
            b'{' if depth == 0 => return Some(i),
            b'{' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// The end of the item that starts at `from`: just past its body's `}` or its `;`,
/// skipping balanced `()` and `[]` groups.
fn item_end(code: &str, from: usize) -> usize {
    let b = code.as_bytes();
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'(' => i = matching(code, i, b'(', b')').unwrap_or(b.len()),
            b'[' => i = matching(code, i, b'[', b']').unwrap_or(b.len()),
            b'{' => return matching(code, i, b'{', b'}').map_or(b.len(), |c| c + 1),
            b';' => return i + 1,
            _ => {}
        }
        i += 1;
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_literals_are_blanked_with_offsets_kept() {
        let src =
            "let a = 1; // unsafe\n/* unsafe /* x */ unsafe */ let s = \"unsafe\"; let c = 'u';\n";
        let b = blank(src);
        assert_eq!(b.len(), src.len());
        assert!(!b.contains("unsafe"));
        assert_eq!(b.matches('\n').count(), 2);
        assert!(b.contains("let s = \"      \";"));
        assert!(b.contains("let c = ' ';"));
    }

    #[test]
    fn doc_comments_survive_when_asked_and_nothing_else_does() {
        let src = "/// Spec: a.b\n//// not doc\n// plain\nfn f() { let s = \"/// Spec: x\"; }\n";
        let kept = blank_keeping_docs(src);
        assert_eq!(kept.len(), src.len());
        assert!(kept.contains("/// Spec: a.b"));
        assert!(!kept.contains("not doc") && !kept.contains("plain"));
        assert!(!kept.contains("Spec: x"));
        assert!(!blank(src).contains("Spec: a.b"));
    }

    #[test]
    fn comments_keep_only_the_prose_at_its_offsets() {
        let src = "let s = \"// not\"; // said\n/* block */ x\n";
        let c = comments(src);
        assert_eq!(c.len(), src.len());
        assert!(!c.contains("not") && !c.contains("let") && !c.contains(" x"));
        assert_eq!(c.find("// said"), src.find("// said"));
        assert_eq!(c.find("/* block */"), src.find("/* block */"));
    }

    #[test]
    fn raw_strings_byte_strings_and_lifetimes() {
        let b =
            blank("r#\"unsafe \" x\"# b\"unsafe\" br\"unsafe\" fn f<'a>(x: &'a u8) { unsafe {} }");
        assert_eq!(b.matches("unsafe").count(), 1);
        assert!(b.contains("<'a>(x: &'a u8)"));
    }

    #[test]
    fn escapes_do_not_end_a_literal_early() {
        let b = blank(r#"let s = "a\"unsafe"; let c = '\''; unsafe {}"#);
        assert_eq!(b.matches("unsafe").count(), 1);
    }

    #[test]
    fn whole_words_only() {
        assert_eq!(find_word("not_unsafe unsafe", "unsafe"), Some(11));
        assert_eq!(find_word("unsafe_code", "unsafe"), None);
        assert_eq!(line_of("a\nb\nc", 4), 3);
    }

    #[test]
    fn a_test_item_ends_at_its_body_not_at_a_semicolon_in_its_signature() {
        let src = "#[cfg(test)] fn f(x: [u8; 3]) -> [u8; 2] { a(); }\nfn live() {}\n";
        let r = test_ranges(src);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(&src[r[0].0..r[0].1], &src[..src.find('\n').unwrap()]);
        let bound = "#[cfg(test)] fn g<T>() where T: Into<[u8; 1]> { x(); }\nfn live() {}\n";
        let r = test_ranges(bound);
        assert!(bound[r[0].0..r[0].1].ends_with("x(); }"), "{r:?}");
        let semi = "#[cfg(test)] use a::b;\nfn live() { x(); }\n";
        let r = test_ranges(semi);
        assert_eq!(&semi[r[0].0..r[0].1], "#[cfg(test)] use a::b;");
    }

    #[test]
    fn test_only_predicates_are_ranges_and_the_rest_are_not() {
        for yes in [
            "#[cfg(test)] mod m {}",
            "#[ cfg ( test ) ] mod m {}",
            "#[cfg(all(test, feature = \"x\"))] mod m {}",
            "#[cfg(all(unix, all(feature = \"x\", test)))] mod m {}",
        ] {
            assert_eq!(test_ranges(yes).len(), 1, "{yes}");
        }
        for no in [
            "#[cfg(any(test, feature = \"x\"))] mod m {}",
            "#[cfg(not(test))] mod m {}",
            "#[cfg(all(unix, feature = \"x\"))] mod m {}",
            "#[cfg_attr(test, derive(Debug))] struct S;",
            "#[derive(Debug)] struct S;",
        ] {
            assert!(test_ranges(no).is_empty(), "{no}");
        }
    }

    #[test]
    fn an_inner_cfg_test_covers_its_block_or_the_file() {
        let file = "fn live() {}\n#![cfg(test)]\nfn t() {}\n";
        let r = test_ranges(file);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, file.len());
        let nested = "mod m { #![cfg(test)] fn t() {} }\nfn live() {}\n";
        let r = test_ranges(nested);
        assert_eq!(&nested[r[0].0..r[0].1], "{ #![cfg(test)] fn t() {} }");
    }
}
