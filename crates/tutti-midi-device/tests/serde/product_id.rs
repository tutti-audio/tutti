use tutti_midi_device::OsEndpointIdentity;

/// A Product Instance Id in a saved document is read as one: the 43rd byte and a
/// byte outside printable ASCII are refused.
#[test]
fn a_saved_product_id_is_read_as_a_product_instance_id() {
    let read = |text: &str| {
        serde_json::from_str::<OsEndpointIdentity>(&format!(r#"{{"Product":"{text}"}}"#))
    };
    assert!(read(&"a".repeat(42)).is_ok());
    assert!(read(&"a".repeat(43)).is_err());
    assert!(read("caf\u{e9}").is_err());
}
