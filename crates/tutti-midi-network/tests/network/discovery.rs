//! Discovery: the TXT keys and the service instance name.

use tutti_midi_network::{
    Advertisement, DiscoveryError, InstanceName, InstanceNameError, KEY_ENDPOINT_NAME,
    KEY_PRODUCT_INSTANCE_ID, SERVICE_TYPE, identity_from_txt, instance_name, txt_properties,
};

use crate::sim::identity;

#[test]
fn the_service_type_and_txt_keys_are_the_registered_ones() {
    assert_eq!(SERVICE_TYPE, "_midi2._udp.local.");
    assert_eq!(KEY_ENDPOINT_NAME, "UMPEndpointName");
    assert_eq!(KEY_PRODUCT_INSTANCE_ID, "ProductInstanceId");
}

#[test]
fn the_txt_record_holds_the_name_and_the_instance_id_of_the_invitation_replies() {
    // M2-124 section 4.4's example record.
    let props = txt_properties(&identity("MIDI Workbench", "173ACEHDI3nVAUiFH"));
    assert_eq!(
        props,
        [
            ("UMPEndpointName", "MIDI Workbench".to_owned()),
            ("ProductInstanceId", "173ACEHDI3nVAUiFH".to_owned())
        ]
    );
}

#[test]
fn the_instance_name_is_the_id_a_dash_and_the_name() {
    assert_eq!(
        instance_name(&identity("MIDIWorkbench", "173ACE"))
            .unwrap()
            .as_str(),
        "173ACE-MIDIWorkbench"
    );
}

/// RFC 6763 section 4.1.1: an instance name is a DNS label of at most 63
/// octets; a longer one is refused, never cut.
#[test]
fn an_instance_name_holds_63_octets_not_64() {
    assert!(InstanceName::new("i".repeat(63)).is_some());
    assert_eq!(InstanceName::new("i".repeat(64)), None);
    // RFC 1035 section 2.3.4: a label is not empty.
    assert_eq!(InstanceName::new(String::new()), None);
    assert_eq!(
        InstanceName::try_from(String::new()),
        Err(InstanceNameError { octets: 0 })
    );
    // Octets, not characters: 31 two-octet characters and a one-octet one
    // are 63; one more two-octet character is 65.
    let at_edge = format!("{}x", "é".repeat(31));
    assert_eq!(at_edge.len(), 63);
    assert!(InstanceName::new(at_edge).is_some());
    let past = format!("{}x", "é".repeat(32));
    assert_eq!(past.len(), 65);
    assert_eq!(
        InstanceName::try_from(past),
        Err(InstanceNameError { octets: 65 })
    );
}

#[test]
fn an_identity_whose_instance_name_passes_63_octets_has_none() {
    // The id (42 octets at most), a dash and the name: 42 + 1 + 21 = 64
    // octets is one past the label, and 20 name octets make 63.
    let id = "i".repeat(42);
    assert_eq!(
        instance_name(&identity(&"n".repeat(21), &id)),
        Err(InstanceNameError { octets: 64 })
    );
    assert_eq!(
        instance_name(&identity(&"n".repeat(20), &id))
            .unwrap()
            .as_str()
            .len(),
        63
    );
    // Advertising refuses it, before any daemon starts.
    assert!(matches!(
        Advertisement::start(&identity(&"n".repeat(21), &id), "host", &[], 5004),
        Err(DiscoveryError::InstanceName(InstanceNameError {
            octets: 64
        }))
    ));
}

/// Spec: m2-124/4.4#5, m2-124/4.4#10
#[test]
fn a_txt_record_with_an_empty_name_or_instance_id_is_not_a_host() {
    assert_eq!(
        identity_from_txt("MIDI Workbench", "173ACE").unwrap(),
        identity("MIDI Workbench", "173ACE")
    );
    assert!(matches!(
        identity_from_txt("", "173ACE"),
        Err(DiscoveryError::Empty(KEY_ENDPOINT_NAME))
    ));
    assert!(matches!(
        identity_from_txt("MIDI Workbench", ""),
        Err(DiscoveryError::Empty(KEY_PRODUCT_INSTANCE_ID))
    ));
}
