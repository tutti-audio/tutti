//! Every seed of every target replays through it, and every target has seeds.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

use std::path::PathBuf;
use tutti_fuzz::TARGETS;

fn corpus(name: &str) -> PathBuf {
    tutti_fixtures::repo_root()
        .join("testing")
        .join("tutti-fuzz")
        .join("corpus")
        .join(name)
}

fn seeds(name: &str) -> Vec<PathBuf> {
    let dir = corpus(name);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("no corpus at {}: {e}", dir.display()))
        .map(|e| e.expect("a readable corpus entry").path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files
}

#[test]
fn every_target_has_seeds() {
    for (name, _) in TARGETS {
        assert!(
            !seeds(name).is_empty(),
            "target `{name}` has no seed corpus"
        );
    }
}

#[test]
fn every_corpus_directory_is_a_target() {
    let root = corpus("");
    for entry in std::fs::read_dir(&root).expect("the corpus root") {
        let dir = entry.expect("a corpus entry").file_name();
        let dir = dir.to_string_lossy();
        assert!(
            TARGETS.iter().any(|(n, _)| *n == dir),
            "corpus/{dir} names no registered target"
        );
    }
}

#[test]
fn every_seed_replays_without_a_panic() {
    for (name, target) in TARGETS {
        for seed in seeds(name) {
            let data = std::fs::read(&seed).expect("a readable seed");
            target(&data);
        }
    }
}

#[test]
fn every_target_has_a_fuzz_binary() {
    let fuzz = tutti_fixtures::repo_root()
        .join("testing")
        .join("tutti-fuzz")
        .join("fuzz");
    let manifest = std::fs::read_to_string(fuzz.join("Cargo.toml")).expect("the fuzz manifest");
    for (name, _) in TARGETS {
        let source = fuzz.join("fuzz_targets").join(format!("{name}.rs"));
        assert!(
            source.is_file(),
            "no fuzz binary source at {}",
            source.display()
        );
        assert!(
            manifest.contains(&format!("name = \"{name}\"")),
            "fuzz/Cargo.toml has no [[bin]] named `{name}`"
        );
    }
}
