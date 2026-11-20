use super::*;

#[cfg(feature = "plugin")]
fn plugin(path: &str) -> PluginRef {
    PluginRef::file(path)
}

/// **A plugin's media type is its format's constant**, one per format.
#[cfg(feature = "plugin")]
#[test]
fn a_plugin_media_type_is_its_formats_constant() {
    for (format, media_type) in [
        (PluginFormat::Clap, "application/x-clap-state"),
        (PluginFormat::Vst3, "application/x-vst3-state"),
        (PluginFormat::Vst2, "application/x-vst2-chunk"),
        (PluginFormat::AudioUnit, "application/x-au-state"),
    ] {
        assert_eq!(PeerState::plugin_media_type(format).as_str(), media_type);
    }
    let state = PeerState::of_plugin(plugin("/p/x.clap"), vec![1]);
    assert_eq!(state.media_type().as_str(), "application/x-clap-state");
}

/// **A state restores into the plugin it was saved from and into no other.**
#[cfg(feature = "plugin")]
#[test]
fn a_state_is_checked_against_the_plugin_it_is_offered_to() {
    let a = SavedPeer::Plugin(plugin("/p/a.clap"));
    let b = SavedPeer::Plugin(plugin("/p/b.clap"));
    let state = PeerState::of_plugin(plugin("/p/a.clap"), vec![1, 2, 3]);
    assert_eq!(state.check_for(&a), Ok(()));
    let refused = state.check_for(&b).expect_err("another plugin");
    assert_eq!(refused.expected(), &b);
    assert_eq!(refused.found(), &a);
    assert_eq!(
        refused.to_string(),
        "this state belongs to /p/a.clap, not to /p/b.clap"
    );
}

/// **A state survives a serde round trip whole**: peer, media type and bytes.
#[cfg(all(feature = "plugin", feature = "serde"))]
#[test]
fn a_state_round_trips_through_serde() {
    let state = PeerState::of_plugin(plugin("/p/a.clap"), vec![9, 8, 7]);
    let json = serde_json::to_string(&state).expect("serialises");
    let back: PeerState = serde_json::from_str(&json).expect("reads back");
    assert_eq!(back, state);
    assert_eq!(back.media_type().as_str(), "application/x-clap-state");
    assert_eq!(back.into_bytes(), vec![9, 8, 7]);
}

#[cfg(feature = "midi-hardware")]
mod device {
    use super::*;
    use tutti_midi_device::{
        DeviceEvent, DeviceInfo, DeviceTable, EndpointId, Identity, Learned, OsChange, OsDevice,
        PortDirection,
    };
    use tutti_ump::ProductInstanceId;
    use tutti_ump::{DeviceIdentity, ManufacturerId, SoftwareRevision, U7};

    /// A listing row of a device the OS calls `name`, under `handle`.
    fn listed(handle: u64, name: &str) -> DeviceInfo {
        let mut table = DeviceTable::default();
        let Some(DeviceEvent::Added(info)) = table.apply(OsChange::Present(OsDevice {
            handle: EndpointId::from_raw(handle),
            identity: Identity::Product(ProductInstanceId::new(&format!("os-{handle}")).unwrap()),
            name: name.into(),
            direction: PortDirection::Input,
        })) else {
            unreachable!("a new handle is added")
        };
        info
    }

    fn model(model: u8) -> DeviceIdentity {
        DeviceIdentity {
            manufacturer: ManufacturerId::of(0x0042),
            family: [1, 0].map(U7::of),
            model: [model, 0].map(U7::of),
            revision: SoftwareRevision::from([1, 2, 3, 4].map(U7::of)),
        }
    }

    fn peer(info: DeviceInfo, learned: Learned) -> SavedPeer {
        SavedPeer::Device(SavedDevice::new(info, learned))
    }

    fn state_of(peer: &SavedPeer) -> PeerState {
        PeerState::new(
            peer.clone(),
            MediaType::new("application/json").unwrap(),
            vec![1],
        )
    }

    fn unit(id: &str) -> ProductInstanceId {
        ProductInstanceId::new(id).unwrap()
    }

    /// **The same Device Identity under another listing row is the same peer**:
    /// the OS renumbered and renamed the port.
    #[test]
    fn a_device_is_the_same_peer_under_another_listing_row() {
        let saved = peer(listed(1, "Keys"), Learned::new().identified(model(7)));
        let now = peer(listed(9, "Keys 2"), Learned::new().identified(model(7)));
        assert_eq!(state_of(&saved).check_for(&now), Ok(()));
    }

    /// **Another model under the same listing row is another peer.**
    #[test]
    fn another_model_under_the_same_listing_row_is_another_peer() {
        let info = listed(1, "Keys");
        let saved = peer(info.clone(), Learned::new().identified(model(7)));
        let now = peer(info, Learned::new().identified(model(8)));
        let refused = state_of(&saved).check_for(&now).expect_err("another model");
        assert_eq!(refused.found(), &saved);
        assert_eq!(refused.expected(), &now);
        assert_eq!(
            refused.to_string(),
            "this state belongs to Keys, not to Keys"
        );
    }

    /// **The same model with different Product Instance Ids is another peer**,
    /// and with the same one is the same.
    #[test]
    fn a_unit_is_told_from_another_unit_of_its_model() {
        let with = |id: &str| {
            peer(
                listed(1, "Keys"),
                Learned::new().identified(model(7)).instanced(unit(id)),
            )
        };
        let state = state_of(&with("SN-1"));
        assert_eq!(state.check_for(&with("SN-1")), Ok(()));
        assert!(state.check_for(&with("SN-2")).is_err());
    }

    /// **A device that said nothing is another peer**, on either side.
    #[test]
    fn a_device_with_no_identity_on_one_side_is_another_peer() {
        let known = peer(listed(1, "Keys"), Learned::new().identified(model(7)));
        let unknown = peer(listed(1, "Keys"), Learned::new());
        assert!(state_of(&known).check_for(&unknown).is_err());
        assert!(state_of(&unknown).check_for(&known).is_err());
        assert!(state_of(&unknown).check_for(&unknown).is_err());
    }

    /// **A device and a plugin are other peers.**
    #[cfg(feature = "plugin")]
    #[test]
    fn a_device_and_a_plugin_are_other_peers() {
        let device = peer(listed(1, "Keys"), Learned::new().identified(model(7)));
        let other = SavedPeer::Plugin(plugin("/p/a.clap"));
        assert!(state_of(&device).check_for(&other).is_err());
        assert!(
            PeerState::of_plugin(plugin("/p/a.clap"), vec![])
                .check_for(&device)
                .is_err()
        );
    }
}
