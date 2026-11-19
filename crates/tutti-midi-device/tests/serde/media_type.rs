use tutti_midi_device::MediaType;

#[test]
fn a_media_type_is_its_text_in_a_document() {
    let media_type = MediaType::new("application/x-thing; v=1").unwrap();
    let json = serde_json::to_string(&media_type).unwrap();
    assert_eq!(json, r#""application/x-thing; v=1""#);
    assert_eq!(
        serde_json::from_str::<MediaType>(&json).unwrap(),
        media_type
    );
}

#[test]
fn a_document_text_that_breaks_the_limit_is_refused() {
    for bad in [String::new(), "caf\u{e9}".into(), "a".repeat(76)] {
        let json = serde_json::to_string(&bad).unwrap();
        assert!(serde_json::from_str::<MediaType>(&json).is_err(), "{json}");
    }
}
