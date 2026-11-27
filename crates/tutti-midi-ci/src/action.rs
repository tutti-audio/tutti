//! [`CiAction`]: what a MIDI-CI Device and its Responders ask their driver to do.

use super::{CiMessage, DataControl, Muid, ProfileId, ProfileState, ProfileTarget, ReportScope};

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
    /// A peer asked for a MIDI Message Report (M2-101 §9.5) of a device that
    /// declares Process Inquiry. The device answers none of it: the caller owns the
    /// state to report, and sends the Reply to MIDI Message Report, the messages
    /// and the End of MIDI Message Report, in that order (§9.6 to §9.8), on the
    /// scope's Device ID.
    ReportAsked {
        /// The peer that asked, the one the report goes to.
        peer: Muid,
        /// How much to report.
        control: DataControl,
        /// Where, and which kinds of message.
        scope: ReportScope,
    },
}
