use super::*;

#[cfg(feature = "plugin")]
fn plugin(path: &str) -> PluginRef {
    PluginRef::file(path).expect("a plugin file extension")
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
