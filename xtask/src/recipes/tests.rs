//! Planning tests.

use super::*;

fn ctx() -> Ctx {
    Ctx {
        root: "/repo".into(),
        target: "/repo/target".into(),
    }
}

fn dry(name: &str, args: &[&str]) -> Vec<String> {
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    plan(name, &args, &ctx())
        .unwrap()
        .iter()
        .map(Step::display)
        .collect()
}

/// The fewest arguments `name` plans with: none, except the version
/// `release` needs.
fn least_args(name: &str) -> &'static [&'static str] {
    match name {
        "release" => &["0.1.0"],
        "conformance" => &["--results", "r"],
        "public-api" => &["tutti-rt"],
        "affected" => &["origin/main"],
        _ => &[],
    }
}

#[test]
fn every_recipe_plans_with_its_least_arguments() {
    for (name, ..) in RECIPES {
        assert!(!dry(name, least_args(name)).is_empty(), "{name}");
    }
}

#[test]
fn release_takes_one_plain_version() {
    assert_eq!(
        dry("release", &["0.1.0"]),
        ["release 0.1.0: set the workspace version and path pins, write the changelog section"]
    );
    for bad in [&[][..], &["v0.1.0"], &["0.1"], &["0.1.0", "x"]] {
        assert!(
            plan(
                "release",
                &bad.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                &ctx()
            )
            .is_err(),
            "{bad:?}"
        );
    }
}

/// Every static gate is a Rust gate: no recipe plans a script.
#[test]
fn no_recipe_runs_a_shell_script() {
    for (name, ..) in RECIPES {
        for line in dry(name, least_args(name)) {
            assert!(!line.contains("scripts/"), "{name}: {line}");
        }
    }
}

/// The static gates are recipes that run in this process, and `ci` (which CI
/// mirrors) runs each; `doc` runs the per-feature rustdoc between its two
/// `cargo doc` lines and the docs.rs build.
#[test]
fn the_ported_gates_are_in_process_steps_and_ci_runs_them() {
    assert_eq!(dry("check-range-types", &[]), ["gate check-range-types"]);
    assert_eq!(
        dry("check-submodule-pins", &[]),
        ["gate check-submodule-pins"]
    );
    assert_eq!(dry("check-paths", &[]), ["gate check-canonical-paths"]);
    for (recipe, gate) in [
        ("check-layout", "gate check-layout"),
        ("check-ffi", "gate check-ffi"),
        ("check-errors", "gate check-errors"),
        ("check-logging", "gate check-logging"),
        ("check-test-time", "gate check-test-time"),
        ("check-units-bare", "gate check-units-bare"),
        ("check-width", "gate check-width"),
        ("check-guards", "gate check-guards"),
        ("check-hosting", "gate check-hosting"),
        ("check-session", "gate check-session"),
        ("check-session-api", "gate check-session-api"),
        ("check-values", "gate check-values"),
        ("check-umbrella", "gate check-umbrella"),
        ("check-lints", "gate check-lints"),
        ("check-value-traits", "gate check-value-traits"),
        ("check-doc-refs", "gate check-doc-refs"),
        ("check-pointers", "gate check-pointers"),
        ("check-doc-idents", "gate check-doc-idents"),
        ("check-passive-tests", "gate check-passive-tests"),
        ("check-comments", "gate check-comments"),
        ("check-alloc-binaries", "gate check-alloc-binaries"),
        ("check-platform-seam", "gate check-platform-seam"),
        ("check-platform", "gate check-platform"),
        ("check-conformance", "gate check-conformance"),
    ] {
        assert_eq!(dry(recipe, &[]), [gate]);
    }
    let ci = dry("ci", &[]);
    for want in [
        "gate check-range-types",
        "gate check-submodule-pins",
        "gate check-canonical-paths",
        "gate check-layout",
        "gate check-ffi",
        "gate check-errors",
        "gate check-logging",
        "gate check-test-time",
        "gate check-units-bare",
        "gate check-width",
        "gate check-guards",
        "gate check-hosting",
        "gate check-session",
        "gate check-session-api",
        "gate check-values",
        "gate check-umbrella",
        "gate check-lints",
        "gate check-value-traits",
        "gate check-doc-refs",
        "gate check-pointers",
        "gate check-doc-idents",
        "gate check-passive-tests",
        "gate check-comments",
        "gate check-alloc-binaries",
        "gate check-platform-seam",
        "gate check-platform",
        "gate check-conformance",
    ] {
        assert!(ci.iter().any(|l| l == want), "{want}");
    }
}

#[test]
fn test_builds_the_plugins_then_runs_nextest() {
    assert_eq!(
        dry("test", &["--no-fail-fast"]),
        [
            "cargo build --locked -p tutti-plugin-server --bin plugin-server",
            "cargo build --locked -p tutti-plugin-probe --bin plugin-probe",
            "cargo nextest run --locked --workspace --no-fail-fast",
        ]
    );
}

#[test]
fn ci_runs_each_dependency_once() {
    let steps = dry("ci", &[]);
    let count = |s: &str| steps.iter().filter(|l| l.as_str() == s).count();
    assert_eq!(
        count("cargo build --locked -p tutti-plugin-server --bin plugin-server"),
        1
    );
    assert_eq!(count("cargo fmt --all --check"), 1);
    // lint first, doc last, as the justfile's dependency list ran.
    assert_eq!(steps[0], "cargo fmt --all --check");
    assert!(
        steps[steps.len() - 2].contains("cargo doc --locked --workspace --no-deps --all-features")
    );
    assert!(steps.last().unwrap().starts_with("umbrella-api: "));
    let deny = steps
        .iter()
        .position(|l| l.starts_with("cargo deny --version"))
        .unwrap();
    let loom = steps.iter().position(|l| l.contains("--cfg loom")).unwrap();
    let test = steps
        .iter()
        .position(|l| l.contains("nextest run --locked --workspace"))
        .unwrap();
    assert!(deny < loom && loom < test);
}

#[test]
fn test_all_does_not_repeat_the_plugin_builds() {
    let steps = dry("test-all", &[]);
    let n = steps
        .iter()
        .filter(|l| l.contains("--bin plugin-probe"))
        .count();
    assert_eq!(n, 1);
}

#[test]
fn bench_save_and_cmp_name_the_baseline() {
    let save = dry("bench-save", &["x"]);
    assert_eq!(save.len(), 10);
    assert!(save.iter().all(|l| l.ends_with("-- --save-baseline x")));
    let cmp = dry("bench-cmp", &[]);
    assert!(cmp.iter().all(|l| l.ends_with("-- --baseline main")));
}

#[test]
fn bench_smoke_runs_every_bench_once() {
    let s = dry("bench-smoke", &[]);
    assert_eq!(s.len(), 10);
    assert!(s.iter().all(|l| l.ends_with("-- --test")));
}

#[test]
fn loom_recipes_carry_their_environment() {
    let s = dry("loom", &[]);
    assert_eq!(
        s[0],
        "LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS='--cfg loom' cargo test --locked -p tutti-rt --release --test pos_ring_loom"
    );
    assert_eq!(
        dry("check-loom", &[]),
        [
            "RUSTFLAGS='--cfg loom' cargo clippy --locked -p tutti-rt -p tutti-plugin-ipc-ffi -p tutti-winmm-ffi --all-targets -- -D warnings"
        ]
    );
}

/// The web build check is the commands CI's clippy job runs: the platform, rt
/// and umbrella crates, then the umbrella with every feature the web path uses.
#[test]
fn check_wasm_lints_the_platform_crate_the_rt_crate_and_the_umbrella_with_the_web_features() {
    assert_eq!(
        dry("check-wasm", &[]),
        [
            "cargo clippy --locked --target wasm32-unknown-unknown -p tutti-platform -p tutti-rt -p tutti --lib -- -D warnings",
            "cargo clippy --locked --target wasm32-unknown-unknown -p tutti --lib --features sampler,soundfont,synth,analyze,decode-wav,decode-flac,decode-mp3,decode-ogg,encode-wav,export,midi -- -D warnings",
        ]
    );
}

/// Named models run alone, with their own environment; a name that is no
/// model, or only a prefix of one, is refused.
#[test]
fn loom_runs_only_the_models_it_is_named() {
    assert_eq!(
        dry("loom", &["handoff_loom", "pos_ring_loom"]),
        [
            "LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS='--cfg loom' cargo test --locked -p tutti-rt --release --test pos_ring_loom",
            "RUSTFLAGS='--cfg loom' cargo test --locked -p tutti-rt --release --test handoff_loom -- --nocapture",
        ]
    );
    for bad in ["handoff", "nothing_loom", "loom"] {
        let args = [bad.to_owned()];
        assert!(plan("loom", &args, &ctx()).is_err(), "{bad}");
    }
}

#[test]
fn check_windows_stubs_under_the_target_dir() {
    let steps = plan("check-windows", &[], &ctx()).unwrap();
    assert_eq!(steps.len(), 10);
    let Step::WriteFile { path, contents } = &steps[0] else {
        panic!()
    };
    assert_eq!(path, &PathBuf::from("/repo/target/tutti-winstub/malloc.h"));
    assert_eq!(contents, "#include <stddef.h>\nvoid *_alloca(size_t);\n");
    for s in &steps[1..] {
        let l = s.display();
        assert!(l.starts_with(
            "CC_x86_64_pc_windows_msvc=clang AR_x86_64_pc_windows_msvc=llvm-ar CFLAGS_x86_64_pc_windows_msvc=-I/repo/target/tutti-winstub cargo clippy --locked --target x86_64-pc-windows-msvc "
        ), "{l}");
        assert!(l.ends_with(" -- -D warnings"));
    }
}

#[test]
fn package_packages_without_building_and_ci_runs_it() {
    assert_eq!(
        dry("package", &[]),
        ["cargo package --locked --no-verify -p <each crate under crates/>"]
    );
    assert!(
        dry("ci", &[])
            .iter()
            .any(|l| l.starts_with("cargo package --locked --no-verify"))
    );
}

#[test]
fn deny_probes_cargo_deny_first() {
    let steps = plan("deny", &[], &ctx()).unwrap();
    assert_eq!(steps[0], Step::RequireCargoDeny);
    assert_eq!(steps[1].display(), "cargo deny --locked check advisories");
}

#[test]
fn doc_documents_every_feature_with_private_items_then_the_umbrella() {
    let s = dry("doc", &[]);
    assert_eq!(s.len(), 2);
    assert_eq!(
        s[0],
        "RUSTDOCFLAGS='-D warnings --document-private-items' cargo doc --locked --workspace --no-deps --all-features"
    );
    assert!(s[1].starts_with("umbrella-api: "), "{}", s[1]);
}

#[test]
fn profile_bench_keeps_its_defaults_and_its_glob() {
    let s = dry("profile-bench", &[]);
    assert_eq!(
        s[0],
        "cargo bench --locked -p tutti-nodes --bench engine_render --profile profiling --no-run"
    );
    assert_eq!(
        s[1],
        "samply record target/profiling/build/tutti-nodes/*/out/engine_render-* --bench --profile-time 10"
    );
}

#[test]
fn book_renders_its_sounds_then_builds_into_its_own_dir_or_the_one_named() {
    let sound =
        "cargo run --locked --release -p tutti-book --example getting_started -- book/src/sounds";
    assert_eq!(dry("book", &[]), [sound, "mdbook build book"]);
    assert_eq!(
        dry("book", &["/srv/site/book"]),
        [sound, "mdbook build book --dest-dir /srv/site/book"]
    );
}

#[test]
fn verify_audio_defaults_its_out_dir() {
    let s = dry("verify-audio", &[]);
    assert_eq!(s.len(), 11);
    assert!(s[0].ends_with("render_sampler_cases -- /tmp/tutti-verify/sampler"));
    assert!(dry("verify-audio", &["o"])[10].ends_with("verify_dsp.py o/dsp"));
}

#[test]
fn arguments_are_refused_where_the_recipe_has_none() {
    assert!(plan("lint", &["x".into()], &ctx()).is_err());
    assert!(plan("bench-save", &["a".into(), "b".into()], &ctx()).is_err());
    assert!(plan("nope", &[], &ctx()).is_err());
}

#[test]
fn check_paths_and_doc_tests_pass_their_arguments_through() {
    assert_eq!(
        dry("check-paths", &["tutti_rt", "cell"]),
        ["gate check-canonical-paths tutti_rt cell"]
    );
    let d = dry("test-doc", &["--", "x"]);
    assert_eq!(d[0], "cargo test --locked --doc --workspace -- x");
    assert_eq!(
        d[1],
        "cargo test --locked --doc -p tutti-session --all-features -- x"
    );
}

#[test]
fn a_dependency_asked_for_twice_runs_once() {
    let c = ctx();
    let mut p = Planner {
        ctx: &c,
        done: HashSet::new(),
        out: Vec::new(),
    };
    p.dep("plugin-probe").unwrap();
    p.dep("plugin-probe").unwrap();
    assert_eq!(p.out.len(), 1);
}

#[test]
fn conformance_reads_its_options_and_defaults_the_output() {
    let s = dry("conformance", &["--results", "junit"]);
    assert_eq!(s.len(), 1);
    assert_eq!(
        s[0],
        "conformance: join /repo/junit/<os>.xml to the inventories, write /repo/site/conformance/index.html, ../index.html and ../badges/*.json"
    );
    let s = dry("conformance", &["--out", "o", "--results", "j"]);
    assert!(s[0].contains("/repo/j/<os>.xml") && s[0].contains("write /repo/o/index.html"));
}

#[test]
fn conformance_refuses_a_missing_repeated_or_unknown_option() {
    let plan = |a: &[&str]| {
        let a: Vec<String> = a.iter().map(|s| (*s).to_owned()).collect();
        plan("conformance", &a, &ctx())
    };
    assert!(plan(&[]).unwrap_err().contains("--results"));
    assert!(plan(&["--results"]).unwrap_err().contains("needs a value"));
    assert!(
        plan(&["--results", "a", "--results", "b"])
            .unwrap_err()
            .contains("twice")
    );
    assert!(plan(&["--nope", "a"]).is_err());
}

#[test]
fn coverage_builds_the_plugins_under_llvm_cov_then_runs_it() {
    let steps = plan("coverage", &[], &ctx()).unwrap();
    assert_eq!(steps[0], Step::RequireLlvmCov);
    assert_eq!(
        steps[1].display(),
        "(cargo llvm-cov show-env) cargo build --locked -p tutti-plugin-server --bin plugin-server"
    );
    assert!(
        steps[2]
            .display()
            .ends_with("-p tutti-plugin-probe --bin plugin-probe")
    );
    let run = steps[3].display();
    assert!(
        run.starts_with("cargo llvm-cov nextest --locked --ignore-run-fail --workspace"),
        "{run}"
    );
    assert!(run.ends_with("--html --output-dir site/coverage"), "{run}");
    assert_eq!(steps.len(), 4);
}

#[test]
fn coverage_takes_a_package_list_in_place_of_the_workspace() {
    let s = dry(
        "coverage",
        &["--package", "tutti-clap-host,tutti-ump", "--out", "o"],
    );
    let run = s.last().unwrap();
    assert!(
        run.contains("-p tutti-clap-host -p tutti-ump --html --output-dir o"),
        "{run}"
    );
    assert!(!run.contains("--workspace"));
}

/// Each no_std crate is built bare and once per listed feature, for the bare-metal target.
#[test]
fn check_no_std_builds_each_listed_crate_for_the_bare_metal_target() {
    let steps = dry("check-no-std", &[]);
    assert_eq!(
        steps,
        [
            "cargo build --locked -p tutti-units --no-default-features --target aarch64-unknown-none",
            "cargo build --locked -p tutti-units --no-default-features --target aarch64-unknown-none --features serde",
            "cargo build --locked -p tutti-music --no-default-features --target aarch64-unknown-none",
            "cargo build --locked -p tutti-music --no-default-features --target aarch64-unknown-none --features serde",
            "cargo build --locked -p tutti-ump --no-default-features --target aarch64-unknown-none",
            "cargo build --locked -p tutti-ump --no-default-features --target aarch64-unknown-none --features serde",
            "cargo build --locked -p tutti-midi-protocol --no-default-features --target aarch64-unknown-none",
            "cargo build --locked -p tutti-midi-protocol --no-default-features --target aarch64-unknown-none --features serde",
            "cargo build --locked -p tutti-curve --no-default-features --target aarch64-unknown-none",
            "cargo build --locked -p tutti-buffers --no-default-features --target aarch64-unknown-none",
            "cargo build --locked -p tutti-buffers --no-default-features --target aarch64-unknown-none --features serde",
            "cargo build --locked -p tutti-metering --no-default-features --target aarch64-unknown-none",
            "cargo build --locked -p tutti-tempo --no-default-features --target aarch64-unknown-none",
            "cargo build --locked -p tutti-tempo --no-default-features --target aarch64-unknown-none --features serde",
            "cargo build --locked -p tutti-voicing --no-default-features --target aarch64-unknown-none",
        ]
    );
}

#[test]
fn public_api_lists_on_the_docs_nightly_or_diffs_from_a_base() {
    assert_eq!(
        dry("public-api", &["tutti-rt"]),
        ["cargo +nightly-2026-10-14 public-api --package tutti-rt --all-features -s"]
    );
    assert_eq!(
        dry("public-api", &["tutti-rt", "origin/main"]),
        [
            "cargo +nightly-2026-10-14 public-api --package tutti-rt --all-features -s diff origin/main..HEAD"
        ]
    );
}

/// `umbrella-api` runs in `doc`, as CI's rustdoc job runs it, and so in `ci`;
/// `gates` builds nothing and leaves it out.
#[test]
fn umbrella_api_runs_in_doc_and_ci_not_gates() {
    let umbrella = |recipe: &str| {
        plan(recipe, &[], &ctx())
            .unwrap()
            .iter()
            .any(|s| matches!(s, Step::UmbrellaApi(_)))
    };
    for recipe in ["umbrella-api", "doc", "ci"] {
        assert!(umbrella(recipe), "{recipe}");
    }
    assert!(!umbrella("gates"));
}

#[test]
fn umbrella_api_takes_one_optional_directory() {
    let step = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        plan("umbrella-api", &args, &ctx())
    };
    assert!(matches!(&step(&[]).unwrap()[..], [Step::UmbrellaApi(None)]));
    assert!(matches!(&step(&["texts"]).unwrap()[..], [Step::UmbrellaApi(Some(d))] if d == "texts"));
    assert_eq!(
        step(&["a", "b"]).unwrap_err(),
        "`umbrella-api` takes at most 1 argument(s), got 2"
    );
}

#[test]
fn the_public_api_command_is_one_for_both_recipes() {
    assert_eq!(
        public_api_cmd("tutti-rt"),
        "cargo +nightly-2026-10-14 public-api --package tutti-rt --all-features -s"
    );
    assert_eq!(
        dry("public-api", &["tutti-rt"]),
        [public_api_cmd("tutti-rt")]
    );
}

#[test]
fn public_api_refuses_no_crate_and_a_third_argument() {
    assert_eq!(
        plan("public-api", &[], &ctx()).unwrap_err(),
        "`public-api` takes a crate: `cargo xtask public-api <crate> [<base>]`"
    );
    let three: Vec<String> = ["a", "b", "c"].iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(
        plan("public-api", &three, &ctx()).unwrap_err(),
        "`public-api` takes at most 2 argument(s), got 3"
    );
}

#[test]
fn fuzz_lists_runs_one_target_or_all_on_the_pinned_nightly() {
    assert_eq!(
        dry("fuzz", &[]),
        ["list the directories of testing/tutti-fuzz/corpus"]
    );
    let one = format!(
        "fuzz wav_decode: cargo +{DOCS_NIGHTLY} fuzz run --fuzz-dir testing/tutti-fuzz/fuzz wav_decode testing/tutti-fuzz/fuzz/corpus/wav_decode testing/tutti-fuzz/corpus/wav_decode -- -max_total_time=60"
    );
    assert_eq!(dry("fuzz", &["wav_decode"]), [one.as_str()]);
    assert_eq!(
        dry("fuzz", &["wav_decode", "--secs", "30"]),
        [one.replace("=60", "=30")]
    );
    let all = dry("fuzz", &["all", "--secs", "5"]);
    assert_eq!(all.len(), 1);
    assert!(all[0].starts_with("fuzz all: cargo +"), "{all:?}");
    assert!(all[0].ends_with("-max_total_time=5 for each directory of testing/tutti-fuzz/corpus"));
}

#[test]
fn fuzz_refuses_a_bad_target_or_duration() {
    for bad in [
        &["--secs", "5"][..],
        &["wav_decode", "--secs", "0"],
        &["wav_decode", "--secs", "x"],
        &["wav_decode", "--secs"],
        &["../x"],
        &["wav_decode", "--time", "5"],
    ] {
        let args: Vec<String> = bad.iter().map(|s| (*s).to_owned()).collect();
        assert!(plan("fuzz", &args, &ctx()).is_err(), "{bad:?}");
    }
}

#[test]
fn affected_takes_exactly_one_base() {
    let step = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        plan("affected", &args, &ctx())
    };
    assert!(
        matches!(&step(&["origin/main"]).unwrap()[..], [Step::Affected(b)] if b == "origin/main")
    );
    assert!(step(&[]).unwrap_err().contains("takes a base"));
    assert!(step(&["a", "b"]).is_err());
}
