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

/// Every recipe that is one in-process gate is a step of `ci` (which CI mirrors),
/// so a gate added to the recipe table cannot be left out of it.
#[test]
fn every_gate_recipe_is_a_step_of_ci() {
    let ci = dry("ci", &[]);
    let mut gates = 0;
    for (name, ..) in RECIPES {
        let steps = dry(name, least_args(name));
        let [only] = &steps[..] else { continue };
        if only.starts_with("gate check-") {
            gates += 1;
            assert!(ci.contains(only), "{name}: {only}");
        }
    }
    assert!(gates > 0);
}

/// Every row of the gate table is a recipe, a step of `ci`, and a step of `gates`
/// unless the row says it needs more than the checkout.
#[test]
fn every_gate_row_is_a_recipe_and_a_step_of_ci_and_of_gates() {
    let (ci, gates) = (dry("ci", &[]), dry("gates", &[]));
    for g in crate::gates::GATES {
        assert!(RECIPES.iter().any(|r| r.0 == g.recipe), "{}", g.recipe);
        let step = dry(g.recipe, &[]);
        assert!(ci.contains(&step[0]), "{}", g.recipe);
        assert_eq!(gates.contains(&step[0]), g.local, "{}", g.recipe);
    }
}

#[test]
fn test_builds_the_plugins_before_it_runs_nextest() {
    let steps = dry("test", &["--no-fail-fast"]);
    let at = |needle: &str| steps.iter().position(|l| l.contains(needle)).unwrap();
    assert!(at("--bin plugin-server") < at("nextest run"));
    assert!(at("--bin plugin-probe") < at("nextest run"));
    assert!(steps.last().unwrap().ends_with("--no-fail-fast"));
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
    assert!(!save.is_empty());
    assert!(save.iter().all(|l| l.ends_with("-- --save-baseline x")));
    let cmp = dry("bench-cmp", &[]);
    assert!(!cmp.is_empty());
    assert!(cmp.iter().all(|l| l.ends_with("-- --baseline main")));
}

/// Every `rtsan` command enables the sanitizer and builds into `<target>/rtsan`, so the
/// normal build is never invalidated; the suites switch on `tutti-rt`'s feature.
#[cfg(not(windows))]
#[test]
fn rtsan_builds_into_its_own_target_with_the_sanitizer_on() {
    let s = dry("rtsan", &[]);
    for l in &s {
        assert!(
            l.starts_with("RTSAN_ENABLE=1 CARGO_TARGET_DIR=/repo/target/rtsan cargo "),
            "{l}"
        );
    }
    assert!(s[2].contains("--features tutti-rt/rtsan"));
    assert!(s[3].contains("--features tutti-rt/rtsan"));
}

const LOCK: &str = "[[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.130\"\n\n[[package]]\nname = \"wasm-bindgen-macro\"\nversion = \"0.2.99\"\n";

/// A runner at the locked `wasm-bindgen` version passes.
#[test]
fn a_wasm_runner_at_the_locked_version_passes() {
    assert_eq!(
        check_wasm_runner(LOCK, "wasm-bindgen-test-runner 0.2.130\n"),
        Ok(())
    );
}

/// A runner at another version fails by name, with the install command for the locked one.
#[test]
fn a_wasm_runner_at_another_version_fails_with_the_install_command() {
    assert_eq!(
        check_wasm_runner(LOCK, "wasm-bindgen-test-runner 0.2.128\n"),
        Err("wasm-bindgen-test-runner 0.2.128 does not match Cargo.lock's wasm-bindgen 0.2.130: install it with cargo install --locked wasm-bindgen-cli --version 0.2.130".to_owned())
    );
}

/// A lock with no `wasm-bindgen` package (a similarly named one is not it) fails.
#[test]
fn a_lock_with_no_wasm_bindgen_fails() {
    let lock = "[[package]]\nname = \"wasm-bindgen-macro\"\nversion = \"0.2.99\"\n";
    assert_eq!(
        check_wasm_runner(lock, "wasm-bindgen-test-runner 0.2.99"),
        Err("Cargo.lock has no wasm-bindgen package".to_owned())
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
fn ci_runs_package() {
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
fn deslop_probes_deslop_please_first_and_reads_the_allow_list() {
    let steps = plan("deslop", &[], &ctx()).unwrap();
    assert_eq!(steps[0], Step::RequireDeslop);
    assert_eq!(
        steps[1].display(),
        "deslop-please --allow xtask/deslop.allow"
    );
    let steps = plan("write-deslop-allow", &[], &ctx()).unwrap();
    assert_eq!(steps[0], Step::RequireDeslop);
    assert_eq!(
        steps[1].display(),
        "deslop-please --update-allow xtask/deslop.allow"
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
    assert!(s[0].ends_with("render_sampler_cases -- /tmp/tutti-verify/sampler"));
    assert!(
        dry("verify-audio", &["o"])
            .last()
            .unwrap()
            .ends_with("verify_dsp.py o/dsp")
    );
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

#[test]
fn public_api_diffs_from_a_base_when_one_is_given() {
    let list = dry("public-api", &["tutti-rt"]);
    let diff = dry("public-api", &["tutti-rt", "origin/main"]);
    assert_eq!(diff, [format!("{} diff origin/main..HEAD", list[0])]);
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

/// Every step of `loom` and `check-loom` builds under `--cfg loom`, and every
/// step of `check-wasm` for `wasm32-unknown-unknown`: without them the recipe
/// checks the host build and passes.
#[test]
fn the_model_and_target_recipes_carry_their_cfg_and_target() {
    for (recipe, needle) in [
        ("loom", "--cfg loom"),
        ("check-loom", "--cfg loom"),
        ("check-wasm", "--target wasm32-unknown-unknown "),
    ] {
        let steps = dry(recipe, &[]);
        assert!(!steps.is_empty(), "{recipe}");
        for l in &steps {
            assert!(l.contains(needle), "{recipe}: {l}");
        }
    }
}
