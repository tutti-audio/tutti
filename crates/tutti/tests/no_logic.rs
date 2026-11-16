//! The one rule this crate has, enforced.
//!
//! `src/lib.rs` must contain nothing but `pub use` and documentation. Logic
//! that wires subsystems belongs in the crate that owns them, where
//! every consumer gets it. Re-exporting items of other crates is fine;
//! `cargo xtask check-umbrella` holds that no crate is re-exported whole and
//! no item sits at two paths.
//!
//! So the failure mode is specific and it is one `fn build()` away: someone
//! adds a small convenience constructor here because it is the crate that can
//! see every subsystem, and the reason this package can exist at all quietly
//! stops being true. This test is the cheapest possible guard against that,
//! and it names the alternative in its failure message.

/// The file, read at compile time, so this test cannot go stale against a
/// moved path.
const LIB: &str = include_str!("../src/lib.rs");

/// Lines that are neither blank, nor documentation, nor part of an attribute
/// (which may span lines).
fn code_lines() -> Vec<(usize, &'static str)> {
    let mut open = 0i32;
    let mut out = Vec::new();
    for (i, l) in LIB.lines().enumerate() {
        let l = l.trim();
        if open > 0 || l.starts_with("#[") {
            open += l.matches('[').count() as i32 - l.matches(']').count() as i32;
            continue;
        }
        if !l.is_empty() && !l.starts_with("//") && !l.starts_with("#!") {
            out.push((i + 1, l));
        }
    }
    out
}

/// The code lines, joined into whole statements.
///
/// Line-based checking is not enough: a `pub use a::{ .. }` spanning three
/// lines makes its continuations look like bare expressions. Statements are accumulated
/// until braces balance and the text ends in `;`, `{` or `}`.
fn statements() -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut buf = String::new();
    let mut depth = 0i32;

    for (n, line) in code_lines() {
        if buf.is_empty() {
            start = n;
        } else {
            buf.push(' ');
        }
        buf.push_str(line);
        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;

        if depth <= 0 && (line.ends_with(';') || line.ends_with('{') || line.ends_with('}')) {
            out.push((start, std::mem::take(&mut buf)));
            depth = 0;
        }
    }
    if !buf.is_empty() {
        out.push((start, buf));
    }
    out
}

/// **No item in this crate may be anything but a re-export or a module.**
///
/// Checked by shape rather than by keyword blacklist: every statement must
/// be a `pub use`, a `pub mod`, or a closing brace. A blacklist of
/// `fn `/`struct `/`impl ` would miss a type alias, a const, a macro or a
/// trait — and the point is not to catch a particular bad word but to keep
/// the file to one shape.
#[test]
fn the_facade_contains_no_code() {
    let offenders: Vec<_> = statements()
        .into_iter()
        .filter(|(_, s)| {
            let s = s.trim();
            !(s.starts_with("pub use ") || s.starts_with("pub mod ") || s == "}")
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "crates/tutti/src/lib.rs must contain only `pub use`, `pub mod` and \
         documentation. Found:\n{}\n\nThis crate exists ONLY because it holds \
         no logic — the package of this name that did was deleted in \
         4b5bd2fd. Whatever this wants to be belongs in the crate that owns \
         the thing it wires: device bootstrap in tutti-cpal, graph logic in \
         tutti-engine. See the module docs.",
        offenders
            .iter()
            .map(|(n, s)| format!("  line {n}: {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
