//! [`CiAction`]: what a MIDI-CI Device and its Responders ask their driver to do.

use super::{CiMessage, Muid, ProfileId, ProfileState, ProfileTarget};

/// What the driver does next.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CiAction {
    /// Send this message to the peer(s) on the connection.
    Send(CiMessage),
    /// A peer answered the Discovery: [`CiDevice::peer`](super::CiDevice::peer) has it.
    Found(Muid),
    /// What a peer's entry holds changed (its Product Instance Id arrived).
    Updated(Muid),
    /// A peer is gone: invalidated, in collision, or dropped with the device's
    /// own MUID. Whatever the caller keyed to it ends.
    Lost(Muid),
    /// The device's MUID changed (§5.9, §3.3.1): every Transaction under the old
    /// one has ended and a new round is on its way.
    MuidChanged {
        /// The MUID that is no longer the device's.
        old: Muid,
        /// The device's MUID now.
        new: Muid,
    },
    /// The Discovery round's window ([`DISCOVERY_WINDOW`](super::DISCOVERY_WINDOW)) has ended.
    WindowClosed,
    /// A peer's Set Profile On or Off changed a Profile the device offers
    /// ([`CiDevice::offer_profiles`](super::CiDevice::offer_profiles)); the Report was sent with it.
    ProfileSwitched {
        /// The peer that asked.
        peer: Muid,
        /// The Profile.
        profile: ProfileId,
        /// Where, and the Channels it uses there.
        target: ProfileTarget,
        /// What the Profile is now.
        state: ProfileState,
    },
}
