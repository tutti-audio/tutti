//! A peer's saved state: [`PeerState`], the bytes with the peer they came
//! from and the media type that says how to read them.
//!
//! A peer's state is its own opaque bytes, so restoring them into another peer
//! is meaningless at best. A state therefore names its [`SavedPeer`], and
//! [`PeerState::check_for`] refuses a state offered to a peer that is not that
//! one, before the peer is asked.

use std::fmt;

use tutti_midi_ci::MediaType;
#[cfg(feature = "midi-hardware")]
use tutti_midi_device::{Match, SavedDevice};
#[cfg(feature = "plugin")]
use tutti_plugin_types::PluginFormat;
#[cfg(feature = "plugin")]
use tutti_plugin_types::PluginRef;

/// The peer a state was saved from: what [`PeerState::check_for`] compares.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum SavedPeer {
    /// A hosted plugin.
    #[cfg(feature = "plugin")]
    Plugin(PluginRef),
    /// A MIDI device.
    #[cfg(feature = "midi-hardware")]
    Device(SavedDevice),
}

impl SavedPeer {
    /// Whether `other` is the same peer.
    ///
    /// A plugin is the same plugin (`PluginRef::is_same_plugin`; a version is
    /// not part of it, plugins read the states of their older versions). A
    /// device is the same model by its Device Identity (M2-104 §7.1.3), the
    /// same unit when both carry a Product Instance Id (§7.1.5): a device's
    /// fullState is per model, so another unit of the model reads it. A device
    /// with no identity on either side is another peer, and so is a peer of
    /// another kind. The OS listing (`DeviceInfo`) is not compared: it names a
    /// port, not the device.
    #[must_use]
    pub fn is_same_peer(&self, other: &Self) -> bool {
        match (self, other) {
            #[cfg(feature = "plugin")]
            (Self::Plugin(a), Self::Plugin(b)) => a.is_same_plugin(b),
            #[cfg(feature = "midi-hardware")]
            (Self::Device(a), Self::Device(b)) => {
                matches!(a.confirm(b.learned()), Match::SameUnit | Match::SameModel)
            }
            #[cfg(all(feature = "plugin", feature = "midi-hardware"))]
            _ => false,
        }
    }
}

#[cfg(feature = "plugin")]
impl From<PluginRef> for SavedPeer {
    fn from(plugin: PluginRef) -> Self {
        Self::Plugin(plugin)
    }
}

#[cfg(feature = "midi-hardware")]
impl From<SavedDevice> for SavedPeer {
    fn from(device: SavedDevice) -> Self {
        Self::Device(device)
    }
}

/// The peer as text, for an error: a plugin's id (else its file), a device's
/// name.
impl fmt::Display for SavedPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(feature = "plugin")]
            Self::Plugin(plugin) => write!(f, "{plugin}"),
            #[cfg(feature = "midi-hardware")]
            Self::Device(device) => f.write_str(&device.info().name),
        }
    }
}

/// A state offered to a peer other than the one it was saved from.
///
/// Raised before the peer is asked; nothing was sent to it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("this state belongs to {found}, not to {expected}")]
pub struct OtherPeer {
    // Boxed: a `SavedPeer` is large, and this is the `Err` of calls that
    // otherwise return small values.
    expected: Box<SavedPeer>,
    found: Box<SavedPeer>,
}

impl OtherPeer {
    /// The peer the state was offered to.
    #[must_use]
    pub fn expected(&self) -> &SavedPeer {
        &self.expected
    }

    /// The peer the state was saved from.
    #[must_use]
    pub fn found(&self) -> &SavedPeer {
        &self.found
    }
}

/// A peer's saved state and the peer it was saved from.
///
/// The bytes are the peer's own. They restore only into the peer they were
/// saved from ([`check_for`](Self::check_for)).
///
/// The [`MediaType`] says how to read the bytes; a device's is the
/// `mediaType` its State body declares (MIDI-CI M2-103 §7.5). It is not
/// checked for a plugin: a stored state may pair a plugin with any media
/// type, and a restore does not read it, because the plugin's format decides
/// how its bytes are read and the format is part of the plugin's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PeerState {
    peer: SavedPeer,
    media_type: MediaType,
    bytes: Vec<u8>,
}

impl PeerState {
    /// `bytes`, encoded as `media_type`, the state `peer` saved.
    #[must_use]
    pub fn new(peer: SavedPeer, media_type: MediaType, bytes: Vec<u8>) -> Self {
        Self {
            peer,
            media_type,
            bytes,
        }
    }

    /// The state `plugin` saved, under its format's media type
    /// (`PeerState::plugin_media_type`); `application/octet-stream` for a ref
    /// whose file no format claims.
    ///
    /// # Panics
    ///
    /// Never: each name is 1 to 75 printable ASCII characters.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn of_plugin(plugin: PluginRef, bytes: Vec<u8>) -> Self {
        // A ref of a file no format claims names no format: its state is bytes.
        let media_type = plugin.format().map_or_else(
            |_| {
                MediaType::new("application/octet-stream")
                    .expect("a constant of 1 to 75 printable ASCII characters")
            },
            Self::plugin_media_type,
        );
        Self::new(SavedPeer::Plugin(plugin), media_type, bytes)
    }

    /// The media type of a state saved by a plugin of `format`.
    ///
    /// No registered media type exists for any plugin format's state, so these
    /// are `application/x-` names, one per format; a format this crate does
    /// not know is `application/octet-stream`.
    ///
    /// # Panics
    ///
    /// Never: each name is 1 to 75 printable ASCII characters.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn plugin_media_type(format: PluginFormat) -> MediaType {
        MediaType::new(match format {
            PluginFormat::Clap => "application/x-clap-state",
            PluginFormat::Vst3 => "application/x-vst3-state",
            PluginFormat::Vst2 => "application/x-vst2-chunk",
            PluginFormat::AudioUnit => "application/x-au-state",
            _ => "application/octet-stream",
        })
        .expect("a constant of 1 to 75 printable ASCII characters")
    }

    /// The peer that saved it.
    #[must_use]
    pub fn peer(&self) -> &SavedPeer {
        &self.peer
    }

    /// How the bytes are encoded.
    #[must_use]
    pub fn media_type(&self) -> &MediaType {
        &self.media_type
    }

    /// The peer's own bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The peer's own bytes, by value.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Whether this state may be restored into `peer`.
    ///
    /// # Errors
    ///
    /// [`OtherPeer`] when it was saved from another peer.
    pub fn check_for(&self, peer: &SavedPeer) -> Result<(), OtherPeer> {
        if self.peer.is_same_peer(peer) {
            Ok(())
        } else {
            Err(OtherPeer {
                expected: Box::new(peer.clone()),
                found: Box::new(self.peer.clone()),
            })
        }
    }
}

#[cfg(test)]
mod tests;
