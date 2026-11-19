//! Every Rust snippet the book writes inline is a run of lines of one of the
//! book's programs, so a snippet cannot drift from code that compiles and is
//! tested.
//!
//! A snippet is a fenced block whose info string starts with `rust` and that
//! holds no `{{#include}}` (an include is the program's own text already).
//! Its lines, trimmed and with mdBook's hidden `# ` lines and blank lines
//! dropped, must appear in that order, one after another, in the trimmed
//! lines of some file in `testing/tutti-book/examples/`.

use std::fs;
use std::path::Path;
use tutti_fixtures::repo_root;

/// The trimmed, non-blank lines of `text`.
fn lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect()
}

/// The inline Rust snippets of a chapter, each with its first line number.
fn snippets(chapter: &str) -> Vec<(usize, Vec<&str>)> {
    let mut out = Vec::new();
    let mut open: Option<(usize, Vec<&str>)> = None;
    for (n, line) in chapter.lines().enumerate() {
        let fence = line.trim_start().strip_prefix("```");
        match (&mut open, fence) {
            (None, Some(info)) if info.starts_with("rust") => open = Some((n + 2, Vec::new())),
            (Some(_), Some(_)) => {
                let (at, body) = open.take().expect("open");
                if !body.iter().any(|l| l.starts_with("{{#include")) {
                    out.push((at, body));
                }
            }
            (Some((_, body)), None) => {
                let l = line.trim();
                if !l.is_empty() && l != "#" && !l.starts_with("# ") {
                    body.push(l);
                }
            }
            _ => {}
        }
    }
    out
}

fn contains_run(haystack: &[&str], needle: &[&str]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

fn read_dir(dir: &Path, ext: &str) -> Vec<(String, String)> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .map(|p| {
            let text = fs::read_to_string(&p).expect("readable");
            (p.display().to_string(), text)
        })
        .collect();
    files.sort();
    files
}

#[test]
fn every_inline_snippet_is_a_run_of_lines_of_a_program() {
    let root = repo_root();
    let programs = read_dir(&root.join("testing/tutti-book/examples"), "rs");
    let program_lines: Vec<Vec<&str>> = programs.iter().map(|(_, t)| lines(t)).collect();
    let chapters = read_dir(&root.join("book/src"), "md");
    let mut checked = 0;
    let mut stray = Vec::new();
    for (path, text) in &chapters {
        for (at, snippet) in snippets(text) {
            checked += 1;
            if !program_lines.iter().any(|p| contains_run(p, &snippet)) {
                stray.push(format!("{path}:{at}"));
            }
        }
    }
    assert!(checked > 0, "the book holds no inline snippet to check");
    assert!(
        stray.is_empty(),
        "snippets that are in no program: {stray:?}"
    );
}

#[test]
fn a_snippet_matches_only_lines_that_follow_each_other() {
    let program = lines("let a = 1;\n\n    let b = 2;\nlet c = 3;\n");
    let chapter = "```rust\nlet a = 1;\n# hidden();\nlet b = 2;\n```\n\
                   ```rust,ignore\nlet a = 1;\nlet c = 3;\n```\n\
                   ```rust\n{{#include x.rs}}\n```\n";
    let found = snippets(chapter);
    assert_eq!(found.len(), 2, "the include block is not a snippet");
    assert!(contains_run(&program, &found[0].1));
    assert!(!contains_run(&program, &found[1].1));
}
