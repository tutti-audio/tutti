//! The peer a handle of MIDI-CI names ([`PeerRef`]), and why a request made from
//! a handle is refused ([`PeerError`]).
//!
//! The handles of Profile Configuration ([`PeerProfiles`](crate::PeerProfiles),
//! [`ProfileSwitch`](crate::ProfileSwitch)), Process Inquiry
//! ([`Reporter`](crate::Reporter)) and Property Exchange
//! ([`PropertyPeer`](crate::PropertyPeer)) are snapshots of one peer of one
//! open input; each names it with a [`PeerRef`].

use tutti_midi_device::DeviceId;

use crate::ci::PeerId;

/// One peer of one input: the input device it was found on, and its Group and
/// MUID there.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeerRef {
    device: DeviceId,
    peer: PeerId,
}

impl PeerRef {
    pub(crate) const fn new(device: DeviceId, peer: PeerId) -> Self {
        Self { device, peer }
    }

    /// The input device the peer was found on.
    #[must_use]
    pub const fn device(&self) -> &DeviceId {
        &self.device
    }

    /// The peer: the Group of the connection it answered on, and its MUID.
    #[must_use]
    pub const fn peer(&self) -> PeerId {
        self.peer
    }

    /// The handle of `handles` that names this peer, `at` being how a handle
    /// names one.
    ///
    /// # Errors
    ///
    /// [`PeerError::NoSuchPeer`] when none does.
    pub(crate) fn find<'a, H>(
        &self,
        handles: impl IntoIterator<Item = &'a H>,
        at: impl Fn(&H) -> &Self,
    ) -> Result<&'a H, PeerError> {
        handles
            .into_iter()
            .find(|handle| at(handle) == self)
            .ok_or(PeerError::NoSuchPeer)
    }
}

/// A request made from the handle of a peer that cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum PeerError {
    /// The device the request was made from is no longer an open input.
    #[error("the device is not an open input")]
    NotOpen,
    /// The peer is not one that input lists for that category now: it was lost,
    /// or has not (or no longer) answered what the category asks of it.
    #[error("no such MIDI-CI peer for that category on that input")]
    NoSuchPeer,
}
