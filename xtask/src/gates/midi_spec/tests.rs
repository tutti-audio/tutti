use super::docs::{M2_101, M2_102, M2_103, M2_104, M2_115, M2_116, M2_124, SMF, SMF_TEXT};
use super::*;

fn page(body: &str) -> String {
    format!("RUNNING HEADER\n\n{body}\n\nFOOTER\n 7\n")
}

/// A made-up text: four pages with a running header and footer.
fn doc(pages: &[&str]) -> String {
    pages
        .iter()
        .map(|p| page(p))
        .collect::<Vec<_>>()
        .join("\x0c")
}

const TOC: &str = "Contents\n1 Alpha ........ 3\n 1.1 Beta .......... 3\n";

fn ids(items: &[Item]) -> Vec<(&str, &str)> {
    items.iter().map(|i| (i.id.as_str(), i.kind)).collect()
}

fn sentences_of(body: &str) -> Vec<Item> {
    let text = doc(&[TOC, body, "", ""]);
    keyword_items(&classify(&clean(&text), &M2_104).unwrap(), &M2_104)
}

fn extracted(body: &str) -> Vec<Item> {
    extract(&M2_124, &doc(&[TOC, body, "", ""])).unwrap()
}

#[test]
fn a_shall_sentence_in_a_definition_list_is_a_row_and_a_plain_definition_is_not() {
    let items = extracted(
        "1 Alpha\nA frob shall spin.\n\nFrobId\nThis is a random value for the frob. It shall be 16 bytes long.\n\nFrobName\nThis is a label for the frob.\n",
    );
    assert_eq!(
        ids(&items),
        [("m2-124/1#1", "required"), ("m2-124/1#2", "required")]
    );
    assert_eq!(items[1].text, "It shall be 16 bytes long.");
}

#[test]
fn a_table_cell_that_copies_a_field_is_a_row_numbered_after_the_keyword_rows() {
    let items = extracted(
        "1 Alpha\nA frob sends a Nak.\nTable 1 Nak Command\n   Field        Size   Values   Description\nCode           1      0x8F\n\nSaved Header     4              Header of the original\nValue                           Command, copied directly from the\n                                received Command.\n\nText     2   string   An optional text.\n\nThe frob shall report a reason.\n",
    );
    assert_eq!(
        ids(&items),
        [("m2-124/1#1", "required"), ("m2-124/1#2", "required")]
    );
    assert_eq!(items[0].facts, "");
    assert_eq!(items[1].facts, "cell=1");
    assert!(items[1].text.contains("copied directly"));
}

#[test]
fn a_description_without_a_copy_word_and_a_copy_word_outside_a_table_are_not_rows() {
    let items = extracted(
        "1 Alpha\nThe value is copied from the request.\nTable 1 Nak Command\n   Field        Size   Values   Description\nCode           1      0x8F\n\nText     2   string   An optional text.\n\nIt may be sent.\n",
    );
    assert_eq!(ids(&items), [("m2-124/1#1", "permitted")]);
}

#[test]
fn a_table_ends_at_the_prose_after_it() {
    let items = extracted(
        "1 Alpha\nTable 1 Nak Command\n   Field        Size   Values   Description\nCode           1      0x8F\n\nSaved     4    word   The id copied from the request.\n\nThe frob then rests.\n\nOther     4    word   The id copied from the reply.\n",
    );
    assert_eq!(ids(&items), [("m2-124/1#1", "required")]);
    assert!(items[0].text.contains("request"));
}

#[test]
fn two_copy_cells_in_a_section_follow_its_keyword_rows() {
    let items = extracted(
        "1 Alpha\nA frob shall spin.\nTable 1 Nak Command\n   Field        Size   Values   Description\nCode           1      0x8F\n\nSaved     4    word   The id copied from the request.\n\nOther     4    word   The id echoed from the reply.\n",
    );
    assert_eq!(
        ids(&items),
        [
            ("m2-124/1#1", "required"),
            ("m2-124/1#2", "required"),
            ("m2-124/1#3", "required")
        ]
    );
    assert_eq!(items[0].facts, "");
    assert_eq!(items[2].facts, "cell=1");
}

#[test]
fn a_copy_cell_that_has_a_conformance_word_is_one_row_not_two() {
    let items = extracted(
        "1 Alpha\nTable 1 Nak Command\n   Field        Size   Values   Description\nCode           1      0x8F\n\nSaved     4    word   The id shall be copied from the request.\n",
    );
    assert_eq!(ids(&items), [("m2-124/1#1", "required")]);
    assert_eq!(items[0].facts, "");
}

#[test]
fn a_dotted_top_level_heading_is_a_heading_only_in_a_document_that_writes_them() {
    let text = doc(&[
        TOC,
        "1. Alpha\nA frob shall spin.\n\n2. Beta\nA bar may ring.\n",
        "",
        "",
    ]);
    let dotted = Doc {
        dotted_top: true,
        ..M2_104
    };
    let items = keyword_items(&classify(&clean(&text), &dotted).unwrap(), &dotted);
    assert_eq!(
        ids(&items),
        [("m2-104/1#1", "required"), ("m2-104/2#1", "permitted")]
    );
    // Without the flag no heading `1` exists.
    assert!(classify(&clean(&text), &M2_104).is_err());
}

#[test]
fn a_dotted_heading_after_a_numbered_list_is_not_among_its_items() {
    let text = doc(&[
        TOC,
        "1. Alpha\nA frob shall spin.\n\n2. Beta\nA bar shall follow:\n   1. One\n   2. Two\n   3. Three\nA baz may idle.\n\n3. Gamma\nA qux should hum.\n",
        "",
        "",
    ]);
    let dotted = Doc {
        dotted_top: true,
        ..M2_104
    };
    let items = keyword_items(&classify(&clean(&text), &dotted).unwrap(), &dotted);
    assert_eq!(
        ids(&items),
        [
            ("m2-104/1#1", "required"),
            ("m2-104/2#1", "required"),
            ("m2-104/2#2", "permitted"),
            ("m2-104/3#1", "recommended"),
        ]
    );
}

#[test]
fn a_heading_title_may_open_with_a_quote() {
    let items =
        sentences_of("1 Alpha\nA frob shall spin.\n\n1.1 \"Beta\" Integration\nA bar may ring.\n");
    assert_eq!(
        ids(&items),
        [("m2-104/1#1", "required"), ("m2-104/1.1#1", "permitted")]
    );
}

#[test]
fn a_document_may_open_at_1_1_when_it_takes_implicit_levels() {
    let text = doc(&[
        "Contents\n1.1 Alpha ........ 3\n",
        "1.1 Alpha\nA frob shall spin.\n\n2 Beta\nA bar may ring.\n",
        "",
        "",
    ]);
    let implicit = Doc {
        implicit_levels: true,
        ..M2_104
    };
    let items = keyword_items(&classify(&clean(&text), &implicit).unwrap(), &implicit);
    assert_eq!(
        ids(&items),
        [("m2-104/1.1#1", "required"), ("m2-104/2#1", "permitted")]
    );
    assert!(classify(&clean(&text), &M2_104).is_err());
}

#[test]
fn an_appendix_heading_may_end_its_letter_in_a_dot() {
    let text = doc(&[
        TOC,
        "1 Alpha\nA frob shall spin.\nAppendix A. It is so.\n\nAppendix A. Table\nA bar may ring.\n3. Item\nA baz should hum.\n",
        "",
        "",
    ]);
    let items = keyword_items(&classify(&clean(&text), &M2_104).unwrap(), &M2_104);
    assert_eq!(
        ids(&items),
        [
            ("m2-104/1#1", "required"),
            ("m2-104/a#1", "permitted"),
            ("m2-104/a#2", "recommended")
        ]
    );
}

#[test]
fn an_appendix_heading_may_put_a_space_before_its_colon() {
    let text = doc(&[
        TOC,
        "1 Alpha\nA frob shall spin.\n\nAppendix A : Table\nA bar may ring.\n",
        "",
        "",
    ]);
    let items = keyword_items(&classify(&clean(&text), &M2_104).unwrap(), &M2_104);
    assert_eq!(
        ids(&items),
        [("m2-104/1#1", "required"), ("m2-104/a#1", "permitted")]
    );
}

#[test]
fn numbered_items_in_an_appendix_are_not_top_level_headings() {
    let text = doc(&[
        TOC,
        "1 Alpha\nA frob shall spin.\n\n2 Beta\nA bar may ring.\n\nAppendix A: Table\nA baz should hum.\n\n3 Item\nA qux shall stay.\n",
        "",
        "",
    ]);
    let items = keyword_items(&classify(&clean(&text), &M2_104).unwrap(), &M2_104);
    assert_eq!(items.last().unwrap().id, "m2-104/a#2");
}

#[test]
fn keywords_map_to_kinds_and_count_per_section() {
    let items = sentences_of(
        "1 Alpha\nA frob shall spin. A frob should hum. A frob may idle.\n\n1.1 Beta\nA bar shall not ring. Plain.\n",
    );
    assert_eq!(
        ids(&items),
        [
            ("m2-104/1#1", "required"),
            ("m2-104/1#2", "recommended"),
            ("m2-104/1#3", "permitted"),
            ("m2-104/1.1#1", "required"),
        ]
    );
    assert_eq!(items[3].facts, "neg=1");
    assert_eq!(items[0].facts, "");
    assert_eq!(items[0].section, "M2-104 \u{a7}1");
}

#[test]
fn the_strongest_word_of_a_sentence_decides_and_case_is_ignored() {
    let items = sentences_of("1 Alpha\nA may idle but a frob SHALL NOT spin.\n");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, "required");
    assert_eq!(items[0].facts, "neg=1");
}

#[test]
fn a_quoted_word_and_a_defining_table_row_are_not_uses() {
    let items = sentences_of(
        "1 Alpha\nshall      Statements of requirement     All \u{2018}shall\u{2019} statements.\nA rule about \u{2018}should\u{2019} words.\nA rule about \u{2019}may\u{2019} words.\nA frob shall spin.\n",
    );
    assert_eq!(ids(&items), [("m2-104/1#1", "required")]);
}

#[test]
fn a_sentence_that_wraps_hyphenates_and_crosses_a_page_is_one() {
    let text = doc(&[
        TOC,
        "1 Alpha\nA frob with a long-\nrunning spin shall turn when the\n",
        "wheel moves.\n\nAnother frob shall stop.\n",
        "",
    ]);
    let items = keyword_items(&classify(&clean(&text), &M2_104).unwrap(), &M2_104);
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].text,
        "A frob with a long-running spin shall turn when the wheel moves."
    );
}

#[test]
fn bullets_and_numbered_items_are_sentences_of_their_own() {
    let items = sentences_of(
        "1 Alpha\nIn particular:\n  \u{2022} a frob shall spin\n  \u{2022} a frob shall hum\n1. The sender should wait\n2. The sender may go\n",
    );
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].text, "a frob shall spin");
    assert_eq!(items[2].kind, "recommended");
}

#[test]
fn an_abbreviation_does_not_end_a_sentence() {
    let items = sentences_of("1 Alpha\nA frob (e.g. A big one) shall spin. B.\n");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text, "A frob (e.g. A big one) shall spin.");
}

#[test]
fn only_the_next_sibling_or_first_child_is_a_heading() {
    let items = sentences_of(
        "1 Alpha\n1.1 Beta\nA frob shall spin.\n16 Groups want it. The frob should hum.\n1.2 Gamma\nA frob may idle.\nAppendix A: Delta\nA.1 Epsilon\nA frob shall stop.\nB.1 Stray\nA frob shall go.\n",
    );
    assert_eq!(
        ids(&items),
        [
            ("m2-104/1.1#1", "required"),
            ("m2-104/1.1#2", "recommended"),
            ("m2-104/1.2#1", "permitted"),
            ("m2-104/a.1#1", "required"),
            ("m2-104/a.1#2", "required"),
        ]
    );
}

#[test]
fn implicit_levels_take_a_heading_whose_ancestors_are_each_the_first() {
    let text = doc(&[
        TOC,
        "1 Alpha\n1.1 Beta\nA frob shall spin.\n1.1.1.1 Deep\nA frob should hum.\n1.2 Gamma\nA frob may idle.\n\n1.2.2.1 Stray\nA frob shall go.\n",
        "",
        "",
    ]);
    let lines = classify(&clean(&text), &M2_116).unwrap();
    assert_eq!(
        ids(&keyword_items(&lines, &M2_116)),
        [
            ("m2-116/1.1#1", "required"),
            ("m2-116/1.1.1.1#1", "recommended"),
            ("m2-116/1.2#1", "permitted"),
            ("m2-116/1.2#2", "required"),
        ]
    );
    // Without the flag a heading with a missing parent is text.
    let lines = classify(&clean(&text), &M2_104).unwrap();
    let plain = keyword_items(&lines, &M2_104);
    assert!(plain.iter().all(|i| !i.id.contains("1.1.1.1")));
    assert_eq!(plain[1].id, "m2-104/1.2#1");
}

#[test]
fn a_document_with_no_tables_gives_keyword_rows_alone() {
    for d in [&M2_101, &M2_102, &M2_103, &M2_115] {
        let body = "1 Alpha\nA frob shall spin.\n";
        let items = extract(d, &doc(&[TOC, body, "", ""])).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].id.starts_with(d.prefix));
        assert_eq!(items[0].section, format!("{} \u{a7}1", d.label));
    }
}

#[test]
fn each_document_has_its_own_prefix_and_file() {
    let prefixes: BTreeSet<_> = DOCS.iter().map(|d| d.prefix).collect();
    let files: BTreeSet<_> = DOCS.iter().map(|d| d.file).collect();
    assert_eq!(prefixes.len(), DOCS.len());
    assert_eq!(files.len(), DOCS.len());
    assert!(
        DOCS.iter()
            .all(|d| d.file == format!("midi-{}.txt", d.prefix))
    );
}

#[test]
fn the_file_header_rows_come_from_the_value_line() {
    let text = doc(&[
        TOC,
        "1 Alpha\nTable 5 Header\nByte 1  Byte 2\nValue  0x53 0x4D 0x46 0x32 0x43 0x4C 0x49 0x50\nText  S M F 2 C L I P\nFigure 1 End\n",
        "",
        "",
    ]);
    let items = extract(&M2_116, &text).unwrap();
    let by = |id: &str| items.iter().find(|i| i.id == id).map(|i| i.facts.as_str());
    assert_eq!(by("m2-116/hdr/1"), Some("byte=1 value=0x53"));
    assert_eq!(by("m2-116/hdr/8"), Some("byte=8 value=0x50"));
    let short = text.replace(" 0x50", "");
    assert!(extract(&M2_116, &short).unwrap_err().contains("not 8"));
}

#[test]
fn the_cover_and_contents_are_skipped() {
    let text = doc(&[
        "Cover. A frob shall spin.\n1 Alpha ........ 3\n",
        "1 Alpha\nA frob shall spin.\n",
        "",
        "",
    ]);
    let items = keyword_items(&classify(&clean(&text), &M2_104).unwrap(), &M2_104);
    assert_eq!(ids(&items), [("m2-104/1#1", "required")]);
    assert!(classify(&clean("no headings here"), &M2_104).is_err());
}

const TABLES: &str = "Table 4 Allocation\n  0x0   32 bits   Utility\n  0x1   32 bits   Real Time\n  0x2   32 bits\n  0x3   64 bits\n  0x4   64 bits\n  0x5   128 bits\n  0x6   32 bits   Reserved\n  0x7   32 bits\n  0x8   64 bits\n  0x9   64 bits\n  0xA   64 bits\n  0xB   96 bits\n  0xC   96 bits\n  0xD   128 bits   Flex\n  0xE   128 bits\n  0xF   128 bits   Stream\nFigure 4 Example\n";

const MESSAGES: &str = "Table 34 All Messages\nUTILITY   NOOP   0x0   res.   0x0   20 bit\nSYSTEM COMMON   MIDI Time Code   0x1   0xg   0xF1   7 bit\nMIDI 2.0 CHANNEL VOICE   Note On   0x4   0xg   0x9   0xn   7 bit\nUMP Stream   Endpoint Discovery   0xF   0   status 0x0   UMP ver.\nUMP Stream   Function Block Discovery   0xF   1   status 0x10   fb\nFlex Data   Set Tempo   0xD   0xg   0   1   0   0x00   0x00   Number\nFlex Data   Text Message Common   0xD   0xg   0-3   0xa   0xn   0x01-0x02   0xvv   data\nColor Key   Reserved for future use\nTable 16 Text\nBank   Status   Message\n0x01   0x00   Unknown\n0x01   0x0A   Accompanying\n0x02   0x01   Lyrics\nFigure 5 End\n";

fn spec(extra: &str) -> String {
    doc(&[
        TOC,
        "1 Alpha\nA frob shall spin.\n",
        &format!("{TABLES}\n{MESSAGES}"),
        extra,
    ])
}

#[test]
fn message_type_and_message_rows_come_from_the_configured_tables() {
    let items = extract(&M2_104, &spec("")).unwrap();
    let by = |id: &str| items.iter().find(|i| i.id == id).map(|i| i.facts.as_str());
    assert_eq!(by("m2-104/mt/b"), Some("mt=0xb bytes=12"));
    assert_eq!(by("m2-104/msg/0/0"), Some("mt=0x0 status=0x0 bytes=4"));
    assert_eq!(by("m2-104/msg/1/f1"), Some("mt=0x1 status=0xf1 bytes=4"));
    assert_eq!(by("m2-104/msg/4/9"), Some("mt=0x4 status=0x9 bytes=8"));
    assert_eq!(by("m2-104/msg/f/10"), Some("mt=0xf status=0x10 bytes=16"));
    assert_eq!(
        by("m2-104/msg/d/00.00"),
        Some("mt=0xd bank=0x0 status=0x0 bytes=16")
    );
    assert_eq!(
        by("m2-104/msg/d/01.0a"),
        Some("mt=0xd bank=0x1 status=0xa bytes=16")
    );
    assert!(by("m2-104/msg/d/02.01").is_some());
    // The common-format row has no single status; the status table
    // supplies the messages.
    assert_eq!(
        items
            .iter()
            .filter(|i| i.id.starts_with("m2-104/msg/d/"))
            .count(),
        4
    );
    assert_eq!(
        items
            .iter()
            .filter(|i| i.id.starts_with("m2-104/mt/"))
            .count(),
        16
    );
    assert_eq!(items[0].id, "m2-104/1#1");
}

#[test]
fn a_missing_table_or_size_is_an_error() {
    let no_msgs = doc(&[TOC, &format!("1 Alpha\n{TABLES}"), "", ""]);
    assert!(
        extract(&M2_104, &no_msgs)
            .unwrap_err()
            .contains("Table 34 not found")
    );
    let short = spec("").replace("  0xE   128 bits\n", "");
    assert!(extract(&M2_104, &short).unwrap_err().contains("not 16"));
}

#[test]
fn extraction_is_deterministic() {
    let text = spec("1.1 Beta\nA bar should ring.\n");
    assert_eq!(
        extract(&M2_104, &text).unwrap(),
        extract(&M2_104, &text).unwrap()
    );
}

/// A made-up Standard MIDI Files text: the heading lines, a footer cutting a
/// sentence, a meta-event heading line with its bytes before it and the
/// closing line.
fn smf_text() -> String {
    let mut text = String::from("Cover\n\n");
    text.push_str(SMF_TEXT.headings[0].0);
    text.push_str("\n\n");
    for (heading, _) in &SMF_TEXT.headings[1..] {
        let line = if *heading == "Time Signature" {
            "FF 58 04 nn dd cc bb Time Signature"
        } else {
            heading
        };
        text.push_str(&format!("{line}\nPlain words.\n\n"));
    }
    text.push_str(
        "Programs must ignore a chunk they\n\nStandard MIDI Files 1.0 5\n\nknow nothing of.\n",
    );
    text.push_str("It will be stored in four bytes.\n");
    text.push_str(SMF_TEXT.end);
    text.push_str("\nProgram must not appear.\n");
    text
}

#[test]
fn a_named_spec_gives_rows_under_its_headings_and_rejoins_a_sentence_a_footer_cut() {
    let items = extract(&SMF, &smf_text()).unwrap();
    let rows: Vec<(&str, &str, &str)> = items
        .iter()
        .map(|i| (i.id.as_str(), i.kind, i.facts.as_str()))
        .collect();
    // The last heading's section holds the sentence the footer cut, whole,
    // and the one after it that only defines; nothing after the closing
    // line is read.
    assert_eq!(
        rows,
        [
            ("smf/delta-times#1", "required", ""),
            ("smf/delta-times#2", "required", "def=1"),
        ]
    );
    assert_eq!(
        items[0].text,
        "Programs must ignore a chunk they know nothing of."
    );
    assert_eq!(items[0].section, "SMF 1.0 \u{a7}delta-times");
}

#[test]
fn a_named_spec_with_a_heading_missing_is_an_error() {
    let text = smf_text().replace("Key Signature", "Key Sig");
    let err = extract(&SMF, &text).unwrap_err();
    assert!(err.contains("`Key Signature` not found"), "{err}");
}

#[test]
fn a_meta_event_line_is_a_heading_and_a_sentence_line_is_not() {
    let heading = |l: &str, h: &str| is_named_heading(l, h);
    assert!(heading(
        "FF 58 04 nn dd cc bb Time Signature",
        "Time Signature"
    ));
    assert!(heading(
        "51 03 tttttt Set Tempo, in microseconds per MIDI quarter-note",
        "Set Tempo, in microseconds per MIDI quarter-note"
    ));
    assert!(heading("Chunks", "Chunks"));
    assert!(!heading("Header Chunks", "Chunks"));
    assert!(!heading("A Time Signature", "Time Signature"));
}

#[test]
fn a_named_spec_reads_not_required_as_may_and_not_optional_as_must() {
    let kind = |s: &str| smf_kind(s, &SMF_TEXT);
    assert_eq!(
        kind("It is not required here."),
        Some(("permitted", false, false))
    );
    assert_eq!(
        kind("This event is not optional."),
        Some(("required", false, false))
    );
    assert_eq!(kind("It must not move."), Some(("required", true, false)));
    assert_eq!(
        kind("Delta-times are a/ways present."),
        Some(("required", false, false))
    );
    assert_eq!(
        kind("The length is stored first."),
        Some(("required", false, true))
    );
    assert_eq!(kind("A plain remark."), None);
}

#[test]
fn a_named_spec_reads_a_phrase_in_any_case_and_a_modal_with_always_as_the_modal() {
    let kind = |s: &str| smf_kind(s, &SMF_TEXT);
    assert_eq!(
        kind("Not required here."),
        Some(("permitted", false, false))
    );
    assert_eq!(kind("A/ways present."), Some(("required", false, false)));
    assert_eq!(
        kind("Tempo should always be stored first."),
        Some(("recommended", false, false))
    );
    assert_eq!(
        kind("It is important to honor the length."),
        Some(("required", false, false))
    );
}

#[test]
fn a_named_spec_splits_a_sentence_at_a_semicolon() {
    let text = smf_text().replace(
        "Programs must ignore a chunk they",
        "A reader should skip it; a writer must not drop it. Programs must ignore a chunk they",
    );
    let items = extract(&SMF, &text).unwrap();
    let kinds: Vec<_> = items
        .iter()
        .map(|i| (i.id.as_str(), i.kind, i.text.as_str()))
        .collect();
    assert_eq!(
        kinds[0],
        (
            "smf/delta-times#1",
            "recommended",
            "A reader should skip it"
        )
    );
    assert_eq!(
        kinds[1],
        (
            "smf/delta-times#2",
            "required",
            "a writer must not drop it."
        )
    );
}

#[test]
fn a_named_spec_with_a_heading_repeated_or_out_of_order_is_an_error() {
    let text = smf_text().replace("Marker\nPlain", "Lyric\nPlain");
    let err = extract(&SMF, &text).unwrap_err();
    assert!(err.contains("out of order or repeated"), "{err}");
    let again = smf_text().replace(
        "Plain words.\n\nSMPTE Offset",
        "Plain words.\n\nLyric\nSMPTE Offset",
    );
    assert!(extract(&SMF, &again).unwrap_err().contains("`Lyric`"));
}
