use super::*;
use crate::gates::fake::Mem;

const ROW: &str = "clap.ext.params.flush | plugin | stable | | | flush parameter changes without processing | | |";
const LOCK: &str = "[[package]]\nname = \"clap-sys\"\nversion = \"0.5.0\"\n";
const PARAMS_SRC: &str =
    "pub struct clap_plugin_params { pub flush: Option<unsafe extern \"C\" fn()>, }";

const VST3_ROW: &str = "vst3.IComponent.setActive | plugin | stable | | vst/ivstcomponent.h | host activates or deactivates the component | | |";
const VST3_HEADER_PATH: &str = "vendor/vst3-sdk/pluginterfaces/vst/ivstcomponent.h";
const VST3_SRC: &str = "/** [plug imp] */\nclass IComponent : public IPluginBase { public: virtual tresult PLUGIN_API setActive (TBool state) = 0; };\nDECLARE_CLASS_IID (IComponent, 1, 2, 3, 4)\n";

fn inv(rows: &[&str]) -> String {
    rows.join("\n")
}

fn run(files: &[(&str, &str)]) -> Report {
    let mem = Mem::with(files);
    check(
        &mem,
        Err("absent".to_owned()),
        &|_| Err("no specs".to_owned()),
        &|| Err("no sf2".to_owned()),
        Require::default(),
    )
}

fn has_out(r: &Report, needle: &str) -> bool {
    r.out.iter().any(|l| l.contains(needle))
}

fn has(r: &Report, needle: &str) -> bool {
    r.findings.iter().any(|f| f.contains(needle))
}

#[test]
fn a_tag_names_the_fn_it_documents() {
    let src = "/// Spec: a.b, c.d\n#[test]\npub async fn first<F>() {}\n\n/// Spec: e.f\nconst X: u8 = 0;\n";
    let (found, errs) = tags("p.rs", src);
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(
        found,
        [
            Tag {
                line: 1,
                ids: vec!["a.b".into(), "c.d".into()],
                item: Some("first".into()),
            },
            Tag {
                line: 5,
                ids: vec!["e.f".into()],
                item: None,
            },
        ]
    );
}

#[test]
fn a_well_formed_inventory_with_a_matching_tag_passes() {
    let r = run(&[
        ("testing/conformance/clap.txt", &inv(&["# c", "", ROW])),
        (
            "crates/a/src/lib.rs",
            "/// Does a thing.\n/// Spec: clap.ext.params.flush\n#[test]\nfn t() {}\n",
        ),
    ]);
    assert!(r.passed(), "{:?}", r.findings);
    assert!(r.out.iter().any(|l| l.contains("1 rows, 1 ids tagged")));
}

#[test]
fn midi_style_rows_parse_with_section_and_facts() {
    let row = parse_row(
        "m2-104/7.4.2#3 | receiver | | required | M2-104 §7.4.2 | note on carries velocity | mt=0x4 status=0x9 n=12 | |",
    )
    .unwrap();
    assert_eq!(row.facts, "mt=0x4 status=0x9 n=12");
    assert_eq!(row.id, "m2-104/7.4.2#3");
}

#[test]
fn malformed_rows_name_file_and_line() {
    let r = run(&[("testing/conformance/clap.txt", "# c\nonly | three | cols\n")]);
    assert!(has(
        &r,
        "testing/conformance/clap.txt:2: expected 9 or 10 columns"
    ));
}

#[test]
fn each_column_validation_can_fail() {
    let base = |i: usize, v: &str| {
        let mut c: Vec<&str> =
            "a.b | host | stable | required | s | para | x=1 | deprecated | cite"
                .split(" | ")
                .collect();
        c[i] = v;
        c.join(" | ")
    };
    assert!(parse_row(&base(0, "a.b")).is_ok());
    assert!(parse_row(&base(6, "x=-12000 y=0x2")).is_ok());
    for (i, v, why) in [
        (0, "a b", "id"),
        (0, "a.b!", "id"),
        (0, "", "id"),
        (1, "Host", "side"),
        (1, "", "side"),
        (2, "beta", "maturity"),
        (3, "must", "kind"),
        (6, "x", "not name=int"),
        (6, "x=abc", "not an integer"),
        (6, "x=0x", "not an integer"),
        (6, "x=-", "not an integer"),
        (6, "x=--1", "not an integer"),
        (6, "X=1", "lowercase identifier"),
        (6, "x=1 x=2", "twice"),
        (7, "gone", "na-kind"),
        (7, "", "both"),
        (8, "", "both"),
    ] {
        let e = parse_row(&base(i, v)).unwrap_err();
        assert!(e.contains(why), "{i} {v}: {e}");
    }
    let blank_maturity_and_kind = "a | host | | | | para | | |";
    assert!(parse_row(blank_maturity_and_kind).is_ok());
    assert!(parse_row("a | initiator | | | | p | | other-side | m2 s1").is_ok());
}

#[test]
fn an_empty_paraphrase_fails_the_gate_but_still_parses_for_regeneration() {
    let row = "a.b | host | | | | | | |";
    assert!(parse_row(row).is_ok());
    let r = run(&[("testing/conformance/x.txt", row)]);
    assert!(has(&r, "row `a.b` has an empty paraphrase"));
    let items = [Item {
        id: "a.b".to_owned(),
        side: "host",
        maturity: "",
    }];
    assert!(regenerate(Some(row), &items).is_ok());
}

#[test]
fn a_duplicate_id_across_files_is_an_error() {
    let r = run(&[
        ("testing/conformance/clap.txt", ROW),
        ("testing/conformance/midi-a.txt", ROW),
    ]);
    assert!(has(&r, "already a row of testing/conformance/clap.txt"));
}

#[test]
fn a_tag_naming_an_unknown_id_fails() {
    let r = run(&[
        ("testing/conformance/clap.txt", ROW),
        (
            "testing/t/tests/x.rs",
            "/// Spec: clap.ext.params.flush, clap.nope\nfn t() {}\n",
        ),
    ]);
    assert!(has(
        &r,
        "testing/t/tests/x.rs:1: the Spec tag names unknown id `clap.nope`"
    ));
    assert_eq!(r.findings.len(), 1);
}

#[test]
fn a_tag_in_a_string_or_plain_comment_is_not_a_tag() {
    let r = run(&[
        ("testing/conformance/clap.txt", ROW),
        (
            "crates/a/src/lib.rs",
            "// /// Spec: nope\nconst S: &str = \"/// Spec: nope\";\n",
        ),
    ]);
    assert!(r.passed(), "{:?}", r.findings);
}

#[test]
fn an_empty_tag_or_one_that_is_not_last_fails() {
    let r = run(&[
        ("testing/conformance/clap.txt", ROW),
        (
            "crates/a/src/lib.rs",
            "/// Spec: clap.ext.params.flush,\nfn a() {}\n",
        ),
        (
            "crates/b/src/lib.rs",
            "/// Spec: clap.ext.params.flush\n/// More prose.\nfn b() {}\n",
        ),
    ]);
    assert!(has(
        &r,
        "crates/a/src/lib.rs:1: the Spec tag has an empty id"
    ));
    assert!(has(
        &r,
        "crates/b/src/lib.rs:1: the Spec tag is not the last doc line"
    ));
}

#[test]
fn missing_rows_are_never_a_finding() {
    let r = run(&[
        ("testing/conformance/clap.txt", ROW),
        ("crates/a/src/lib.rs", "fn t() {}\n"),
    ]);
    assert!(r.passed());
}

#[test]
fn the_clap_inventory_must_equal_what_clap_sys_declares() {
    let sys = Mem::with(&[("src/ext/params.rs", PARAMS_SRC)]);
    let go = |rows: &str| {
        let mem = Mem::with(&[("Cargo.lock", LOCK), ("testing/conformance/clap.txt", rows)]);
        check(
            &mem,
            Ok(&sys),
            &|_| Err("no specs".to_owned()),
            &|| Err("no sf2".to_owned()),
            Require::default(),
        )
    };
    assert!(go(ROW).passed());
    assert!(has(
        &go("x.y | host | | | | p | | |"),
        "clap-sys declares `clap.ext.params.flush`"
    ));
    let extra = format!("{ROW}\nclap.ext.params.zap | plugin | stable | | | p | | |");
    assert!(has(
        &go(&extra),
        "`clap.ext.params.zap` (plugin, stable) is not what clap-sys declares"
    ));
    let wrong_side = ROW.replace("plugin | stable", "host | stable");
    assert!(has(
        &go(&wrong_side),
        "(host, stable) is not what clap-sys declares"
    ));
}

#[test]
fn without_the_clap_sys_source_the_check_is_skipped_unless_required() {
    let mem = Mem::with(&[("Cargo.lock", LOCK), ("testing/conformance/clap.txt", ROW)]);
    let skipped = check(
        &mem,
        Err("clap-sys 0.5.0 not found".to_owned()),
        &|_| Err("no specs".to_owned()),
        &|| Err("no sf2".to_owned()),
        Require::default(),
    );
    assert!(skipped.passed());
    assert!(
        skipped
            .out
            .iter()
            .any(|l| l.contains("clap-sys 0.5.0 not found"))
    );
    let required = check(
        &mem,
        Err("clap-sys 0.5.0 not found".to_owned()),
        &|_| Err("no specs".to_owned()),
        &|| Err("no sf2".to_owned()),
        Require::parse(Some("require")),
    );
    assert!(has(&required, "clap-sys 0.5.0 not found"));
}

#[test]
fn the_pinned_version_is_read_from_the_lock() {
    assert_eq!(pinned_clap_sys(LOCK).as_deref(), Some("0.5.0"));
    assert_eq!(pinned_clap_sys("name = \"other\"\n"), None);
}

#[test]
fn regeneration_keeps_hand_columns_of_surviving_ids_and_drops_the_rest() {
    let kept = "clap.ext.params.flush | plugin | stable | required | §1 | my words | n=0x2 | deprecated | clap ext/params.h\n\
                clap.gone | host | | | | old | | |\n";
    let items = [
        Item {
            id: "clap.ext.params.flush".to_owned(),
            side: "host",
            maturity: "draft",
        },
        Item {
            id: "clap.ext.params.count".to_owned(),
            side: "plugin",
            maturity: "stable",
        },
    ];
    let out = regenerate(Some(kept), &items).unwrap();
    let rows: Vec<&str> = out.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(
        rows,
        [
            "clap.ext.params.flush | host | draft | required | §1 | my words | n=0x2 | deprecated | clap ext/params.h",
            "clap.ext.params.count | plugin | stable |  |  |  |  |  |",
        ]
    );
    assert!(regenerate(Some("bad line\n"), &items).is_err());
}

const MIDI_TEXT: &str = "RUNNING\n 1\n\x0cRUNNING\n 2\n\x0cRUNNING\n 3\n\x0c1 Alpha\nA frob shall spin.\nTable 4 Allocation\n0x0 32 bits\n0x1 32 bits\n0x2 32 bits\n0x3 64 bits\n0x4 64 bits\n0x5 128 bits\n0x6 32 bits\n0x7 32 bits\n0x8 64 bits\n0x9 64 bits\n0xA 64 bits\n0xB 96 bits\n0xC 96 bits\n0xD 128 bits\n0xE 128 bits\n0xF 128 bits\nFigure 4 End\nTable 34 Messages\nUTILITY NOOP 0x0 res. 0x0 20\nFlex Data Set Tempo 0xD 0xg 0 1 0 0x00 0x00 n\nTable 16 Text\n0x01 0x00 Unknown\nFigure 5 End\n";

fn midi_rows() -> String {
    let items = midi_spec::extract(&midi_spec::M2_104, MIDI_TEXT).unwrap();
    let r = regenerate_midi(&midi_spec::M2_104, None, &items).unwrap();
    assert_eq!(r.added.len(), items.len());
    assert!(r.removed.is_empty());
    r.text
}

#[test]
fn the_midi_rows_must_equal_what_the_text_gives() {
    // Rebuild each row with a paraphrase in column six.
    let with_words: Vec<String> = midi_rows()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let mut c: Vec<&str> = l.split(" | ").collect();
            c[1] = "both";
            c[5] = "words";
            c.join(" | ")
        })
        .collect();
    let good = with_words.join("\n");
    let go = |inv: &str, text: Result<&str, String>| {
        let mem = Mem::with(&[("testing/conformance/midi-m2-104.txt", inv)]);
        let only_104 = |doc: &midi_spec::Doc| match (&text, doc.prefix) {
            (Ok(t), "m2-104") => Ok((*t).to_owned()),
            (Err(e), _) => Err(e.clone()),
            _ => Err("no M2-116 text".to_owned()),
        };
        check(
            &mem,
            Err("absent".to_owned()),
            &only_104,
            &|| Err("no sf2".to_owned()),
            Require::default(),
        )
    };
    assert!(
        go(&good, Ok(MIDI_TEXT)).passed(),
        "{:?}",
        go(&good, Ok(MIDI_TEXT)).findings
    );
    let wrong_kind = good.replacen("| required |", "| recommended |", 1);
    assert!(has(
        &go(&wrong_kind, Ok(MIDI_TEXT)),
        "the M2-104 row `m2-104/1#1` (recommended"
    ));
    assert!(has(
        &go(&wrong_kind, Ok(MIDI_TEXT)),
        "the M2-104 text gives `m2-104/1#1` (required"
    ));
    let dropped: String = good.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert!(has(&go(&dropped, Ok(MIDI_TEXT)), "gives `m2-104/1#1`"));
}

#[test]
fn without_the_midi_text_the_check_is_skipped_unless_require_all() {
    let lock = Mem::with(&[
        ("Cargo.lock", LOCK),
        ("testing/conformance/clap.txt", ROW),
        ("testing/conformance/vst3.txt", VST3_ROW),
        (VST3_HEADER_PATH, VST3_SRC),
    ]);
    let sys = Mem::with(&[("src/ext/params.rs", PARAMS_SRC)]);
    let run = |v: Option<&str>| {
        check(
            &lock,
            Ok(&sys),
            &|_| Err("TUTTI_MIDI_SPECS is not set".to_owned()),
            &|| Err("TUTTI_SF2_SPEC is not set".to_owned()),
            Require::parse(v),
        )
    };
    for v in [None, Some("require")] {
        let r = run(v);
        assert!(r.passed(), "{:?}", r.findings);
        assert!(
            r.out
                .iter()
                .any(|l| l.contains("M2-104 inventory not checked"))
        );
    }
    assert!(has(
        &run(Some("require-all")),
        "M2-104 inventory not checked: TUTTI_MIDI_SPECS"
    ));
}

#[test]
fn the_vst3_rows_are_held_to_the_sdk_headers() {
    let go = |row: &str| {
        check(
            &Mem::with(&[
                ("testing/conformance/vst3.txt", row),
                (VST3_HEADER_PATH, VST3_SRC),
            ]),
            Err("absent".to_owned()),
            &|_| Err("no specs".to_owned()),
            &|| Err("no sf2".to_owned()),
            Require::default(),
        )
    };
    assert!(go(VST3_ROW).passed());
    let wrong_side = VST3_ROW.replace("| plugin |", "| host |");
    assert!(has(&go(&wrong_side), "is not what the VST3 SDK declares"));
    assert!(has(&go(""), "declares `vst3.IComponent.setActive`"));
    let r = run(&[("testing/conformance/vst3.txt", VST3_ROW)]);
    assert!(r.passed() && has_out(&r, "vst3 inventory not checked"));
}

#[test]
fn vst3_regeneration_keeps_hand_columns_and_reports_added_and_removed() {
    let items = vst3_sdk::extract(&Mem::with(&[(VST3_HEADER_PATH, VST3_SRC)])).unwrap();
    let kept = format!(
        "{} | host | draft | required | stale | my words | | |\nvst3.IGone.x | core | stable | | | old | | |\n",
        items[0].id
    );
    let (out, added, removed) = regenerate_vst3(Some(&kept), &items).unwrap();
    let row = out.lines().find(|l| l.starts_with(&items[0].id)).unwrap();
    assert_eq!(
        row,
        "vst3.IComponent.setActive | plugin | stable | required | vst/ivstcomponent.h | my words |  |  |"
    );
    assert!(added.is_empty());
    assert_eq!(removed, ["vst3.IGone.x"]);
    let (_, added, _) = regenerate_vst3(None, &items).unwrap();
    assert_eq!(added, ["vst3.IComponent.setActive"]);
}

#[test]
fn midi_regeneration_keeps_hand_columns_flags_new_ids_and_reports_removed() {
    let items = midi_spec::extract(&midi_spec::M2_104, MIDI_TEXT).unwrap();
    let first = &items[0];
    let kept = format!(
        "{} | sender | | required | stale | my words | | deprecated | cite\nm2-104/gone#1 | both | | | | old | | |\n",
        first.id
    );
    let Regen {
        text: out,
        added,
        orphans,
        ..
    } = regenerate_midi(&midi_spec::M2_104, Some(&kept), &items).unwrap();
    let row = out.lines().find(|l| l.starts_with(&first.id)).unwrap();
    assert_eq!(
        row,
        format!(
            "{} | sender |  | required | {} | my words |  | deprecated | cite | {}",
            first.id,
            first.section,
            carry::digest(&first.text)
        )
    );
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].id, "m2-104/gone#1");
    assert_eq!(added.len(), items.len() - 1);
    let new = out.lines().find(|l| l.starts_with("m2-104/mt/0")).unwrap();
    assert!(new.contains("| todo |"));
    let r = run(&[("testing/conformance/midi-m2-104.txt", &out)]);
    assert!(has(&r, "has an empty paraphrase"));
}

fn sf2_rows() -> String {
    let spec = sf2_spec::extract(sf2_spec::SAMPLE).unwrap();
    let r = regenerate_sf2("", &spec.items).unwrap();
    assert_eq!(r.added.len(), spec.items.len());
    assert!(r.removed.is_empty());
    r.text
        .lines()
        .map(|l| {
            let mut c: Vec<&str> = l.split(" | ").collect();
            c[1] = "player";
            c[5] = "words";
            c.join(" | ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sf2_run(inv: &str, text: Result<&str, String>, require: Require) -> Report {
    let mem = Mem::with(&[("testing/conformance/sf2.txt", inv)]);
    check(
        &mem,
        Err("absent".to_owned()),
        &|_| Err("no specs".to_owned()),
        &|| text.clone().map(str::to_owned),
        require,
    )
}

#[test]
fn the_sf2_extracted_rows_must_equal_what_the_text_gives() {
    let good = sf2_rows();
    let ok = sf2_run(&good, Ok(sf2_spec::SAMPLE), Require::default());
    assert!(ok.passed(), "{:?}", ok.findings);
    let wrong_fact = good.replacen("def=0 inst_only=1", "def=1 inst_only=1", 1);
    let r = sf2_run(&wrong_fact, Ok(sf2_spec::SAMPLE), Require::default());
    assert!(has(&r, "the SF2 row `sf2.gen.alpha`"));
    assert!(has(&r, "the SF2 text gives `sf2.gen.alpha`"));
    let wrong_kind = good.replacen("| recommended |", "| required |", 1);
    assert!(has(
        &sf2_run(&wrong_kind, Ok(sf2_spec::SAMPLE), Require::default()),
        "the SF2 row `sf2.gen.alpha` (required"
    ));
    let dropped: String = good.lines().skip(1).collect::<Vec<_>>().join("\n");
    let r = sf2_run(&dropped, Ok(sf2_spec::SAMPLE), Require::default());
    assert!(has(&r, "the SF2 text gives `sf2.gen.alpha`"));
    let extra = format!("{good}\nsf2.gen.omega | player | | | SF2 §8.1.2 | words | num=99 | |");
    assert!(has(
        &sf2_run(&extra, Ok(sf2_spec::SAMPLE), Require::default()),
        "the SF2 row `sf2.gen.omega`"
    ));
}

#[test]
fn a_hand_written_sf2_row_must_name_a_section_the_text_has() {
    let hand = |section: &str| {
        format!(
            "{}\nsf2.riff.form | reader | | | {section} | words | | |",
            sf2_rows()
        )
    };
    for section in ["SF2 §1.1", "SF2 §4.5"] {
        let r = sf2_run(&hand(section), Ok(sf2_spec::SAMPLE), Require::default());
        assert!(r.passed(), "{section}: {:?}", r.findings);
    }
    for section in ["SF2 §9.9", "§1.1", "1.1", ""] {
        let r = sf2_run(&hand(section), Ok(sf2_spec::SAMPLE), Require::default());
        assert!(
            has(&r, "the SF2 row `sf2.riff.form` has section"),
            "{section}"
        );
    }
}

#[test]
fn a_hand_written_sf2_section_is_well_formed_even_without_the_text() {
    let hand = |section: &str| {
        format!(
            "{}\nsf2.riff.form | reader | | | {section} | words | | |",
            sf2_rows()
        )
    };
    let absent = || Err("TUTTI_SF2_SPEC is not set".to_owned());
    for section in ["SF2 §1.1", "SF2 §9.9", "SF2 §8.1.2"] {
        let r = sf2_run(&hand(section), absent(), Require::default());
        assert!(r.passed(), "{section}: {:?}", r.findings);
    }
    for section in ["SF2 §", "SF2 §1.", "SF2 §x.1", "SF2 §1..2", "§1.1", ""] {
        let r = sf2_run(&hand(section), absent(), Require::default());
        assert!(
            has(&r, "the SF2 row `sf2.riff.form` has section"),
            "{section}"
        );
    }
}

#[test]
fn without_the_sf2_text_the_check_is_skipped_unless_require_all() {
    let inv = sf2_rows();
    let absent = || Err("TUTTI_SF2_SPEC is not set".to_owned());
    let r = sf2_run(&inv, absent(), Require::default());
    assert!(r.passed(), "{:?}", r.findings);
    assert!(has_out(&r, "SF2 inventory not checked: TUTTI_SF2_SPEC"));
    // `require` demands the other sources, never the SoundFont text.
    let r = sf2_run(&inv, absent(), Require::parse(Some("require")));
    assert!(!has(&r, "SF2"), "{:?}", r.findings);
    assert!(has_out(&r, "SF2 inventory not checked: TUTTI_SF2_SPEC"));
    let r = sf2_run(&inv, absent(), Require::parse(Some("require-all")));
    assert!(has(&r, "SF2 inventory not checked: TUTTI_SF2_SPEC"));
}

#[test]
fn sf2_regeneration_keeps_hand_rows_and_columns_and_reports_added_and_removed() {
    let spec = sf2_spec::extract(sf2_spec::SAMPLE).unwrap();
    let existing = "# header\nsf2.riff.form | reader | | | SF2 §1.1 | mine | | |\nsf2.gen.alpha | reader | | required | stale | my words | num=9 | |\nsf2.gen.gone | player | | | | old | | |\n";
    let Regen {
        text: out,
        added,
        orphans,
        ..
    } = regenerate_sf2(existing, &spec.items).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "# header");
    assert_eq!(
        lines[1],
        "sf2.riff.form | reader | | | SF2 §1.1 | mine | | |"
    );
    assert_eq!(
        lines[2],
        format!(
            "sf2.gen.alpha | reader |  | recommended | SF2 §8.1.2 | my words | num=0 min=0 def=0 inst_only=1 |  |  | {}",
            carry::digest(&spec.items[0].text)
        )
    );
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].id, "sf2.gen.gone");
    assert_eq!(added.len(), spec.items.len() - 1);
    assert!(lines.iter().any(|l| l.starts_with("sf2.gen.beta | todo |")));
    assert!(regenerate_sf2("bad line\n", &spec.items).is_err());
}

#[test]
fn each_document_is_held_to_its_own_text() {
    let text_116 = "RUNNING\n 1\n\x0cRUNNING\n 2\n\x0cRUNNING\n 3\n\x0c1 Alpha\nA frob shall spin.\nTable 5 Header\nByte 1  Byte 2\nValue  0x53 0x4D 0x46 0x32 0x43 0x4C 0x49 0x50\nFigure 1 End\n";
    let items = midi_spec::extract(&midi_spec::M2_116, text_116).unwrap();
    let inv = regenerate_midi(&midi_spec::M2_116, None, &items)
        .unwrap()
        .text;
    assert!(inv.starts_with("# MIDI Clip File (M2-116 v1.0) inventory."));
    assert!(inv.contains("m2-116/hdr/<n>"));
    assert!(!inv.contains("m2-116/msg"));
    let rows: String = inv
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let mut c: Vec<&str> = l.split(" | ").collect();
            c[1] = "both";
            c[5] = "words";
            c.join(" | ") + "\n"
        })
        .collect();
    let mem = Mem::with(&[("testing/conformance/midi-m2-116.txt", &rows)]);
    let go = |t: &'static str| {
        check(
            &mem,
            Err("absent".to_owned()),
            &move |d: &midi_spec::Doc| {
                if d.prefix == "m2-116" {
                    Ok(t.to_owned())
                } else {
                    Err("no M2-104 text".to_owned())
                }
            },
            &|| Err("no sf2".to_owned()),
            Require::default(),
        )
    };
    assert!(go(text_116).passed(), "{:?}", go(text_116).findings);
    let other = text_116.replace("0x50", "0x51");
    let other: &'static str = Box::leak(other.into_boxed_str());
    assert!(has(&go(other), "the M2-116 text gives `m2-116/hdr/8`"));
}

fn sentence(n: u32, text: &str) -> midi_spec::Item {
    midi_spec::Item {
        id: format!("m2-104/1#{n}"),
        kind: "required",
        section: "M2-104 \u{a7}1".to_owned(),
        facts: String::new(),
        text: text.to_owned(),
    }
}

/// A file for `texts` with `hand` as (side, paraphrase) per sentence.
fn hand_file(texts: &[&str], hand: &[(&str, &str)]) -> String {
    let items: Vec<_> = (1..).zip(texts).map(|(n, t)| sentence(n, t)).collect();
    let mut file = regenerate_midi(&midi_spec::M2_104, None, &items)
        .unwrap()
        .text;
    for (n, (side, words)) in (1..).zip(hand) {
        let id = format!("m2-104/1#{n}");
        let at = file.find(&format!("\n{id} | ")).unwrap() + 1;
        let end = at + file[at..].find('\n').unwrap();
        let mut cols: Vec<String> = file[at..end].split(" | ").map(str::to_owned).collect();
        cols[1] = (*side).to_owned();
        cols[5] = (*words).to_owned();
        file.replace_range(at..end, &cols.join(" | "));
    }
    file
}

fn rows_of(text: &str) -> Vec<Row> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| parse_row(l).unwrap())
        .collect()
}

#[test]
fn an_inserted_sentence_keeps_the_hand_columns_on_its_sentence_under_the_new_id() {
    let old = hand_file(
        &["A shall x.", "B shall y."],
        &[("host", "ay"), ("client", "bee")],
    );
    let new: Vec<_> = [(1, "A shall x."), (2, "Z shall z."), (3, "B shall y.")]
        .iter()
        .map(|&(n, t)| sentence(n, t))
        .collect();
    let r = regenerate_midi(&midi_spec::M2_104, Some(&old), &new).unwrap();
    let rows = rows_of(&r.text);
    assert_eq!(
        (
            rows[0].side.to_string().as_str(),
            rows[0].paraphrase.as_str()
        ),
        ("host", "ay")
    );
    assert_eq!(
        (
            rows[1].side.to_string().as_str(),
            rows[1].paraphrase.as_str()
        ),
        ("todo", "")
    );
    assert_eq!(
        (
            rows[2].side.to_string().as_str(),
            rows[2].paraphrase.as_str()
        ),
        ("client", "bee")
    );
    assert_eq!(r.added, ["m2-104/1#2"]);
    assert!(r.changed.is_empty() && r.orphans.is_empty() && r.removed.is_empty());
}

#[test]
fn a_reworded_sentence_under_hand_columns_fails_the_gate_naming_the_row() {
    let old = hand_file(
        &["A shall x.", "B shall y."],
        &[("host", "ay"), ("client", "bee")],
    );
    let new: Vec<_> = [(1, "A shall x."), (2, "B shall not y.")]
        .iter()
        .map(|&(n, t)| sentence(n, t))
        .collect();
    let r = regenerate_midi(&midi_spec::M2_104, Some(&old), &new).unwrap();
    assert_eq!(r.changed.len(), 1);
    assert_eq!(r.changed[0].0, "m2-104/1#2");
    assert_eq!(r.changed[0].1.paraphrase, "bee");
    assert!(r.orphans.is_empty());
    let rows = rows_of(&r.text);
    assert_eq!(
        (
            rows[1].side.to_string().as_str(),
            rows[1].paraphrase.as_str()
        ),
        ("changed", "")
    );
    assert_eq!(rows[0].paraphrase, "ay");
    let report = run(&[("testing/conformance/midi-m2-104.txt", &r.text)]);
    assert!(has(
        &report,
        "requirement text changed under hand columns: row `m2-104/1#2`"
    ));
    assert!(!has(&report, "row `m2-104/1#1`"));
    // Regenerating again does not forget the row.
    let again = regenerate_midi(&midi_spec::M2_104, Some(&r.text), &new).unwrap();
    assert_eq!(again.text, r.text);
}

#[test]
fn a_deleted_sentence_with_hand_columns_is_an_orphan_and_one_without_is_not() {
    let old = hand_file(
        &["A shall x.", "B shall y.", "C shall w."],
        &[("host", "ay"), ("client", "bee"), ("todo", "")],
    );
    let new = [sentence(1, "A shall x.")];
    let r = regenerate_midi(&midi_spec::M2_104, Some(&old), &new).unwrap();
    assert_eq!(r.orphans.len(), 1);
    assert_eq!(r.orphans[0].id, "m2-104/1#2");
    assert_eq!(r.orphans[0].paraphrase, "bee");
    assert_eq!(r.removed, ["m2-104/1#3"]);
    assert!(r.changed.is_empty());
}

#[test]
fn duplicate_sentences_in_a_section_keep_their_own_hand_columns_by_occurrence() {
    let old = hand_file(
        &["A shall x.", "A  shall\nx."],
        &[("host", "first"), ("client", "second")],
    );
    let new: Vec<_> = [(1, "Z shall z."), (2, "A shall x."), (3, "A shall x.")]
        .iter()
        .map(|&(n, t)| sentence(n, t))
        .collect();
    let r = regenerate_midi(&midi_spec::M2_104, Some(&old), &new).unwrap();
    let rows = rows_of(&r.text);
    assert_eq!(rows[0].side, Side::Spec("todo".to_owned()));
    assert_eq!(rows[1].paraphrase, "first");
    assert_eq!(rows[2].paraphrase, "second");
    assert_eq!(rows[1].digest, rows[2].digest);
}

#[test]
fn the_digest_ignores_whitespace_and_is_the_fnv_1a_64_of_the_words() {
    assert_eq!(carry::digest("A  shall\n x."), carry::digest("A shall x."));
    assert_ne!(carry::digest("A shall x."), carry::digest("A shall y."));
    assert_eq!(carry::digest(" \n"), "");
    // FNV-1a 64 of "a" is af63dc4c8601ec8c.
    assert_eq!(carry::digest("a"), "af63dc4c8601ec8c");
}

#[test]
fn a_digest_that_disagrees_with_the_text_fails_the_gate() {
    let good = midi_rows();
    let mem_for = |inv: &str| {
        let mem = Mem::with(&[("testing/conformance/midi-m2-104.txt", inv)]);
        check(
            &mem,
            Err("absent".to_owned()),
            &|doc: &midi_spec::Doc| {
                if doc.prefix == "m2-104" {
                    Ok(MIDI_TEXT.to_owned())
                } else {
                    Err("no text".to_owned())
                }
            },
            &|| Err("no sf2".to_owned()),
            Require::default(),
        )
    };
    let filled: String = good
        .lines()
        .map(|l| {
            if l.starts_with('#') {
                l.to_owned()
            } else {
                let mut c: Vec<&str> = l.split(" | ").collect();
                c[1] = "both";
                c[5] = "words";
                c.join(" | ")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let ok = mem_for(&filled);
    assert!(ok.passed(), "{:?}", ok.findings);
    let row = filled
        .lines()
        .find(|l| l.starts_with("m2-104/1#1 | "))
        .unwrap();
    let old = row.rsplit(" | ").next().unwrap();
    let bad = mem_for(&filled.replace(old, "0000000000000000"));
    assert!(has(
        &bad,
        "the M2-104 row `m2-104/1#1` stores digest `0000000000000000`"
    ));
}

fn sentence_in(section: u32, n: u32, text: &str) -> midi_spec::Item {
    let mut i = sentence(n, text);
    i.id = format!("m2-104/{section}#{n}");
    i
}

#[test]
fn an_orphan_stays_in_the_file_and_fails_every_run() {
    let old = hand_file(
        &["A shall x.", "B shall y."],
        &[("host", "ay"), ("client", "bee")],
    );
    let new = [sentence(1, "A shall x.")];
    let first = regenerate_midi(&midi_spec::M2_104, Some(&old), &new).unwrap();
    let second = regenerate_midi(&midi_spec::M2_104, Some(&first.text), &new).unwrap();
    for r in [&first, &second] {
        assert_eq!(r.orphans.len(), 1);
        let row = rows_of(&r.text).pop().unwrap();
        assert_eq!(
            (
                row.id.as_str(),
                row.side.to_string().as_str(),
                row.paraphrase.as_str()
            ),
            ("m2-104/1#2", "orphan:client", "bee")
        );
        assert_eq!(row.digest, carry::digest("B shall y."));
    }
    assert_eq!(first.text, second.text);
    let report = run(&[("testing/conformance/midi-m2-104.txt", &second.text)]);
    assert!(has(&report, "orphan row `m2-104/1#2`"));
}

#[test]
fn a_sentence_moved_between_sections_is_an_orphan_and_a_todo_row_and_fails() {
    let old = hand_file(
        &["A shall x.", "B shall y."],
        &[("host", "ay"), ("client", "bee")],
    );
    let new = [
        sentence_in(1, 1, "A shall x."),
        sentence_in(2, 1, "B shall y."),
    ];
    let r = regenerate_midi(&midi_spec::M2_104, Some(&old), &new).unwrap();
    assert_eq!(r.orphans.len(), 1);
    assert_eq!(r.orphans[0].paraphrase, "bee");
    assert_eq!(r.added, ["m2-104/2#1"]);
    let rows = rows_of(&r.text);
    let ids: Vec<(&str, String)> = rows
        .iter()
        .map(|r| (r.id.as_str(), r.side.to_string()))
        .collect();
    assert_eq!(
        ids,
        [
            ("m2-104/1#1", "host".to_owned()),
            ("m2-104/1#2", "orphan:client".to_owned()),
            ("m2-104/2#1", "todo".to_owned())
        ]
    );
    assert!(!run(&[("testing/conformance/midi-m2-104.txt", &r.text)]).passed());
}

#[test]
fn a_row_without_a_sentence_carries_by_id_beside_digest_rows() {
    let table = midi_spec::Item {
        id: "m2-104/mt/0".to_owned(),
        kind: "",
        section: "M2-104 Table 4".to_owned(),
        facts: "mt=0x0 bytes=4".to_owned(),
        text: String::new(),
    };
    let old_items = [sentence(1, "A shall x."), table.clone()];
    let file = regenerate_midi(&midi_spec::M2_104, None, &old_items)
        .unwrap()
        .text;
    let file: String = file
        .lines()
        .map(|l| match parse_row(l) {
            Ok(mut r) if r.id == "m2-104/mt/0" => {
                r.side = Side::Spec("util".to_owned());
                r.paraphrase = "my table".to_owned();
                assert!(r.digest.is_empty());
                r.render() + "\n"
            }
            _ => l.to_owned() + "\n",
        })
        .collect();
    let new = [sentence(1, "Z shall z."), sentence(2, "A shall x."), table];
    let r = regenerate_midi(&midi_spec::M2_104, Some(&file), &new).unwrap();
    let rows = rows_of(&r.text);
    assert_eq!(rows[2].side, Side::Spec("util".to_owned()));
    assert_eq!(rows[2].paraphrase, "my table");
    assert!(rows[2].digest.is_empty() && !rows[1].digest.is_empty());
    assert!(r.orphans.is_empty() && r.changed.is_empty());
}

#[test]
fn a_reworded_sf2_sentence_under_hand_columns_becomes_changed_and_fails() {
    let spec = sf2_spec::extract(sf2_spec::SAMPLE).unwrap();
    let blank = regenerate_sf2("", &spec.items).unwrap().text;
    let filled = blank.replacen("sf2.gen.alpha | todo |", "sf2.gen.alpha | reader |", 1);
    let filled = filled
        .lines()
        .map(|l| {
            if l.starts_with("sf2.gen.alpha | ") {
                let mut c: Vec<&str> = l.split(" | ").collect();
                c[5] = "my words";
                c.join(" | ")
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut items = spec.items.clone();
    let at = items.iter().position(|i| i.id == "sf2.gen.alpha").unwrap();
    items[at].text.push_str(" and now more");
    let r = regenerate_sf2(&filled, &items).unwrap();
    assert_eq!(r.changed.len(), 1);
    assert_eq!(r.changed[0].0, "sf2.gen.alpha");
    let row = rows_of(&r.text)
        .into_iter()
        .find(|w| w.id == "sf2.gen.alpha")
        .unwrap();
    assert_eq!(
        (row.side.to_string().as_str(), row.paraphrase.as_str()),
        ("changed", "")
    );
    let report = sf2_run(&r.text, Err("absent".to_owned()), Require::default());
    assert!(has(
        &report,
        "requirement text changed under hand columns: row `sf2.gen.alpha`"
    ));
}
