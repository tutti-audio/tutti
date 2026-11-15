//! The channels of an MPE Profile zone, and a set of zones that share none
//! (M2-120 §2.1, §3.1, §3.4).
//!
//! A Profile zone is enabled on a Manager Channel with a number of channels
//! (§3.1 steps 3 and 5): the Manager is the lowest and the Members are the
//! channels above it, so the zone ends by channel 16.

use crate::{MidiChannel, MpeChannels};

/// The channels of one MPE Profile zone: the Manager Channel, which is the
/// lowest, and the Member Channels above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MpeZoneSpan {
    manager: MidiChannel,
    channels: MpeChannels,
}

impl MpeZoneSpan {
    /// The zone of `channels` channels from `manager` up, or `None` when it
    /// would run past channel 16.
    #[must_use]
    pub const fn new(manager: MidiChannel, channels: MpeChannels) -> Option<Self> {
        if manager.get() + channels.get() <= MidiChannel::COUNT {
            Some(Self { manager, channels })
        } else {
            None
        }
    }

    /// The Manager Channel.
    #[must_use]
    pub const fn manager(self) -> MidiChannel {
        self.manager
    }

    /// How many channels the zone has, the Manager Channel included.
    #[must_use]
    pub const fn channels(self) -> MpeChannels {
        self.channels
    }

    /// The Member Channels, in order.
    pub fn members(self) -> impl Iterator<Item = MidiChannel> + Clone {
        ((self.manager.get() + 1)..self.end()).filter_map(MidiChannel::new)
    }

    /// Whether `channel` is the Manager or a Member.
    #[must_use]
    pub const fn contains(self, channel: MidiChannel) -> bool {
        channel.get() >= self.manager.get() && channel.get() < self.end()
    }

    /// Whether the two zones share a channel.
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.manager.get() < other.end() && other.manager.get() < self.end()
    }

    /// The channel number above the zone's last, `16` for one that ends on
    /// channel 16.
    const fn end(self) -> u8 {
        self.manager.get() + self.channels.get()
    }
}

/// Two zones of a [`MpeZoneSet`] that share a channel (M2-120 §3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the MPE zones of channels {} and {} overlap", .held.manager().get(), .added.manager().get())]
pub struct MpeOverlap {
    /// The zone already in the set.
    pub held: MpeZoneSpan,
    /// The zone that was added.
    pub added: MpeZoneSpan,
}

/// The MPE Profile zones of one Group: at least one, in order of Manager
/// Channel, no two sharing a channel (M2-120 §3.4). Sixteen channels hold at
/// most [`MAX`](Self::MAX) zones of two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MpeZoneSet {
    zones: [Option<MpeZoneSpan>; Self::MAX],
}

impl MpeZoneSet {
    /// The most zones: eight, each with two channels at least.
    pub const MAX: usize = 8;

    /// The set of `zone` alone.
    #[must_use]
    pub const fn single(zone: MpeZoneSpan) -> Self {
        let mut zones = [None; Self::MAX];
        zones[0] = Some(zone);
        Self { zones }
    }

    /// The set with `zone` added.
    ///
    /// # Errors
    ///
    /// [`MpeOverlap`] when `zone` shares a channel with a zone held, which
    /// every ninth zone does.
    pub fn with(self, zone: MpeZoneSpan) -> Result<Self, MpeOverlap> {
        if let Some(held) = self.zones().find(|held| held.overlaps(zone)) {
            return Err(MpeOverlap { held, added: zone });
        }
        let mut zones = [None; Self::MAX];
        let mut count = 0;
        // The zones share no channel and each has two, so at most 7 are held here.
        for z in self.zones().chain([zone]) {
            zones[count] = Some(z);
            count += 1;
        }
        zones[..count].sort_unstable_by_key(|z| z.map(|z| z.manager().get()));
        Ok(Self { zones })
    }

    /// The zones, lowest Manager Channel first.
    pub fn zones(&self) -> impl Iterator<Item = MpeZoneSpan> + Clone + '_ {
        self.zones.iter().copied().flatten()
    }

    /// The zone that has `channel`, as its Manager or as a Member.
    #[must_use]
    pub fn zone_of(&self, channel: MidiChannel) -> Option<MpeZoneSpan> {
        self.zones().find(|z| z.contains(channel))
    }
}
