use super::super::testing::{DST, header};
use super::super::*;

fn text(s: &str) -> MessageText {
    MessageText::new(s).expect("a text within the bound")
}

/// The wire bytes of a text as the NAK of `text` carries them.
fn wire(s: &str) -> String {
    let mut out = Vec::new();
    text(s).encode(&mut out);
    String::from_utf8(out).expect("the wire text is ASCII")
}

fn unescaped(bytes: &[u8]) -> String {
    MessageText::decode(bytes).as_str().to_owned()
}

fn nak_of(bytes: &[u8]) -> Nak {
    match CiMessage::decode(bytes) {
        Decoded::Message(CiMessage::Nak { nak, .. }) => nak,
        other => panic!("expected a NAK, got {other:?}"),
    }
}

#[test]
fn nak_round_trips() {
    let msg = CiMessage::Nak {
        header: header(),
        to: Muid::of(DST),
        nak: Nak::new(U7::of(SUB_ID2_DISCOVERY), NakStatus::VersionNotSupported),
    };
    assert_eq!(CiMessage::decode(&msg.encode()), Decoded::Message(msg));
}

#[test]
fn nak_carries_its_version_2_details_and_text() {
    // Table 15's Version-2 trailer: 5 detail bytes, a 2-byte LSB-first
    // length, then that many text bytes. All of it must survive.
    let nak = Nak::new(
        U7::of(SUB_ID2_DISCOVERY),
        NakStatus::Busy(TimeoutWait::of(9)),
    )
    .with_details([1, 2, 3, 4, 5].map(U7::of))
    .with_message(text("body one byte short"));
    let msg = CiMessage::Nak {
        header: header(),
        to: Muid::of(DST),
        nak: nak.clone(),
    };
    assert_eq!(nak_of(&msg.encode()), nak);

    // The length prefix is two 7-bit LSB-first bytes, so text longer than
    // 127 bytes survives (a peer that declared a larger receive size may send
    // it): read from the wire, whole, and written back as it was.
    let long = "e".repeat(300);
    let mut body = vec![0x34, NakStatus::Retry.code().get(), 0, 0, 0, 0, 0, 0];
    body.extend_from_slice(&[(300 & 0x7F) as u8, (300 >> 7) as u8]);
    body.extend_from_slice(long.as_bytes());
    let nak = Nak::decode_body(&body).expect("decodes");
    assert_eq!(nak.message.as_str(), long);
    let mut again = Vec::new();
    nak.encode_body(&mut again);
    assert_eq!(again, body);
}

#[test]
fn an_empty_pre_1_2_nak_still_decodes() {
    // Older devices sent a NAK with no body at all; every Version-2 field
    // defaults rather than the message being rejected.
    let nak = Nak::decode_body(&[]).expect("empty NAK decodes");
    assert_eq!(nak.nak_sub_id2, U7::of(0));
    assert_eq!(nak.status, NakStatus::Nak);
    assert_eq!(nak.details, [U7::of(0); 5]);
    assert!(nak.message.is_empty());

    // ...and a v1-length NAK (the three original bytes) too.
    let nak = Nak::decode_body(&[SUB_ID2_DISCOVERY, 0x04, 0x00]).expect("v1 NAK decodes");
    assert_eq!(nak.status, NakStatus::ProfileNotSupported);
    assert!(nak.message.is_empty());
}

// §5.11.2 Table 16 and §5.10.2 Table 14, every status with its code and data.
#[test]
fn the_statuses_are_the_tables_codes_and_data() {
    let nak = [
        (NakStatus::Nak, 0x00, 0x00),
        (NakStatus::MessageNotSupported, 0x01, 0x00),
        (NakStatus::VersionNotSupported, 0x02, 0x00),
        (NakStatus::NotInUse, 0x03, 0x00),
        (NakStatus::ProfileNotSupported, 0x04, 0x00),
        (NakStatus::ResendLastChunk, 0x12, 0x00),
        (NakStatus::Terminate(PeTerminator::Responder), 0x20, 0x00),
        (NakStatus::Terminate(PeTerminator::Initiator), 0x20, 0x01),
        (NakStatus::ChunksOutOfSequence, 0x21, 0x00),
        (NakStatus::Retry, 0x40, 0x00),
        (NakStatus::Malformed, 0x41, 0x00),
        (NakStatus::Timeout, 0x42, 0x00),
        (NakStatus::Busy(TimeoutWait::of(127)), 0x43, 127),
    ];
    for (status, code, data) in nak {
        assert_eq!((status.code().get(), status.data().get()), (code, data));
        assert_eq!(NakStatus::from_wire(U7::of(code), U7::of(data)), status);
    }
    let ack = [
        (AckStatus::Ack, 0x00, 0x00),
        (AckStatus::TimeoutWait(TimeoutWait::of(25)), 0x10, 25),
        (AckStatus::FlowControlNextChunks, 0x11, 0x00),
    ];
    for (status, code, data) in ack {
        assert_eq!((status.code().get(), status.data().get()), (code, data));
        assert_eq!(AckStatus::from_wire(U7::of(code), U7::of(data)), status);
    }
}

// M1: a status the tables do not name, or name with other data, is kept as it
// came: every code and data pair reads back as itself.
#[test]
fn every_code_and_data_pair_reads_back_as_itself() {
    for code in 0..=127 {
        for data in 0..=127 {
            let (code, data) = (U7::of(code), U7::of(data));
            let nak = NakStatus::from_wire(code, data);
            assert_eq!((nak.code(), nak.data()), (code, data));
            let ack = AckStatus::from_wire(code, data);
            assert_eq!((ack.code(), ack.data()), (code, data));
        }
    }
}

// Table 16, 0x20: only data 0 and 1 name a terminator; another data is no
// Terminate.
#[test]
fn a_terminate_nak_with_other_data_is_not_a_terminate() {
    assert_eq!(
        NakStatus::from_wire(U7::of(0x20), U7::of(2)),
        NakStatus::Other {
            code: U7::of(0x20),
            data: U7::of(2)
        }
    );
}

// §5.10, Table 13: sub-ID#2 0x7D, then the body NAK has.
#[test]
fn an_ack_round_trips_with_its_sub_id_and_body() {
    let ack = Ack::new(U7::of(0x34), AckStatus::TimeoutWait(TimeoutWait::of(25))) // 2.5 s
        .with_details([7, 0, 3, 0, 0].map(U7::of))
        .with_message(text("Accepted Beat"));
    let msg = CiMessage::Ack {
        header: header(),
        to: Muid::of(DST),
        ack: ack.clone(),
    };
    let bytes = msg.encode();
    assert_eq!(bytes[3], 0x7D);
    assert_eq!(&bytes[13..13 + 3], [0x34, 0x10, 25]);
    assert_eq!(CiMessage::decode(&bytes), Decoded::Message(msg));
}

// §5.10.4 and §5.11.4: the Message Length counts bytes of text, and a
// length that runs past the message is not a message.
#[test]
fn a_text_that_runs_past_the_message_is_malformed() {
    let mut body = Vec::new();
    Nak::new(U7::of(1), NakStatus::Nak)
        .with_message(text("abc"))
        .encode_body(&mut body);
    assert!(Nak::decode_body(&body).is_some());
    body.pop();
    assert_eq!(Nak::decode_body(&body), None);
}

// §5.10.4, rule 2, with its two examples, "Accepted Beat ♪" and
// "残りわずか5バイト" (the spec writes the escape digits
// in both cases; the writer writes lower case, the reader takes either).
/// Spec: m2-101/5.10.4#4
#[test]
fn non_ascii_text_is_escaped_as_utf16_and_unescaped() {
    assert_eq!(wire("Accepted Beat ♪"), "Accepted Beat \\u266a");
    assert_eq!(
        wire("残りわずか5バイト"),
        "\\u6b8b\\u308a\\u308f\\u305a\\u304b5\\u30d0\\u30a4\\u30c8"
    );
    assert_eq!(unescaped(b"Accepted Beat \\u266a"), "Accepted Beat ♪");
    assert_eq!(
        unescaped(b"\\u6B8B\\u308A\\u308F\\u305A\\u304B5\\u30D0\\u30A4\\u30C8"),
        "残りわずか5バイト"
    );
    // A character past the Basic Multilingual Plane is two UTF-16 units, as in
    // JSON, and reads back as one character.
    assert_eq!(wire("\u{1F600}"), "\\ud83d\\ude00");
    assert_eq!(unescaped(b"\\ud83d\\ude00"), "\u{1F600}");
    // A lone surrogate is no character.
    assert_eq!(unescaped(b"a\\ud83db"), "a\u{FFFD}b");
    // Every byte on the wire is 7-bit ASCII.
    assert!(wire("é♪\u{1F600}").is_ascii());
}

// §5.10.4, rule 1: the backslash has the special meaning it has in JSON, and a
// slash may optionally be escaped, so either spelling is read.
/// Spec: m2-101/5.10.4#3
#[test]
fn the_backslash_is_escaped_and_a_slash_is_either() {
    assert_eq!(wire("a\\b"), "a\\\\b");
    assert_eq!(unescaped(b"a\\\\b"), "a\\b");
    assert_eq!(wire("a/b"), "a/b");
    assert_eq!(unescaped(b"a/b"), "a/b");
    assert_eq!(unescaped(b"a\\/b"), "a/b");
    // An escape JSON does not define keeps its backslash.
    assert_eq!(unescaped(b"a\\qb"), "a\\qb");
    assert_eq!(unescaped(b"a\\u12"), "a\\u12");
}

// §5.10.4, rule 3: a message uses no ASCII below 0x20 except 0x0A (line feed);
// other such characters, tab and carriage return among them, are ignored.
/// Spec: m2-101/5.10.4#5, m2-101/5.10.4#6
#[test]
fn control_characters_are_escaped_on_write_and_ignored_raw_on_read() {
    // The line feed is the one raw control character.
    assert_eq!(wire("a\nb"), "a\nb");
    // Any other is written as an escape, so no raw byte below 0x20 is sent...
    assert_eq!(wire("a\tb\r\u{7f}"), "a\\u0009b\\u000d\u{7f}");
    // ...and the escape reads back as the character it stands for.
    assert_eq!(unescaped(b"a\\u0009b"), "a\tb");
    // A raw one is ignored.
    assert_eq!(unescaped(b"a\tb\rc\x08d\ne"), "abcd\ne");
}

// §5.10.4 is not a JSON document: a quote is an ordinary 7-bit character and
// is sent as it is, where a Property Exchange string value escapes it.
#[test]
fn a_quote_in_message_text_is_sent_raw_and_read_back() {
    assert_eq!(wire("say \"hi\""), "say \"hi\"");
    assert_eq!(unescaped(b"say \"hi\""), "say \"hi\"");
}

// §5.10.4: up to 103 bytes of Message text is safe to send. The
// bound counts the escaped bytes.
#[test]
fn a_text_is_held_to_103_escaped_bytes() {
    assert!(MessageText::new(&"a".repeat(103)).is_some());
    assert!(MessageText::new(&"a".repeat(104)).is_none());
    // Each ♪ is six bytes on the wire: 17 of them are 102, 18 are 108.
    assert!(MessageText::new(&"♪".repeat(17)).is_some());
    assert!(MessageText::new(&"♪".repeat(18)).is_none());
    // A backslash is two.
    assert!(MessageText::new(&"\\".repeat(51)).is_some());
    assert!(MessageText::new(&"\\".repeat(52)).is_none());
    // The surrogate pair is twelve.
    assert!(MessageText::new(&"\u{1F600}".repeat(8)).is_some());
    assert!(MessageText::new(&"\u{1F600}".repeat(9)).is_none());
}

// A text written is the text read, whatever it holds.
#[test]
fn a_text_round_trips_through_a_nak() {
    for s in ["", "plain", "Pièce ♪ 5\n\\ \u{1F600} \t", "/\\u0041"] {
        let nak = Nak::new(U7::of(1), NakStatus::Nak).with_message(text(s));
        let msg = CiMessage::Nak {
            header: header(),
            to: Muid::of(DST),
            nak,
        };
        assert_eq!(nak_of(&msg.encode()).message.as_str(), s);
    }
}
