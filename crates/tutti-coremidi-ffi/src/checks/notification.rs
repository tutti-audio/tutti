//! What a CoreMIDI client notification says, and whether it changes the device list.
//!
//! The mapping from `MIDINotificationMessageID` and the decision are pure, so they
//! are tested on every target; the client that receives the notifications is
//! `SharedClient`'s.

/// A `MIDINotification` of a CoreMIDI client, by its `messageID`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Notification {
    /// `kMIDIMsgSetupChanged`: some aspect of the MIDI setup changed.
    SetupChanged,
    /// `kMIDIMsgObjectAdded`: a device, entity or endpoint was added.
    ObjectAdded,
    /// `kMIDIMsgObjectRemoved`: a device, entity or endpoint was removed.
    ObjectRemoved,
    /// `kMIDIMsgPropertyChanged`: an object's property changed.
    PropertyChanged,
    /// `kMIDIMsgThruConnectionsChanged`.
    ThruConnectionsChanged,
    /// `kMIDIMsgSerialPortOwnerChanged`.
    SerialPortOwnerChanged,
    /// `kMIDIMsgIOError`.
    IoError,
    /// Any other `messageID`, which this crate does not read.
    Other,
}

impl Notification {
    /// The notification with `MIDINotificationMessageID` `id`.
    #[must_use]
    pub const fn from_message_id(id: i32) -> Self {
        match id {
            1 => Self::SetupChanged,
            2 => Self::ObjectAdded,
            3 => Self::ObjectRemoved,
            4 => Self::PropertyChanged,
            5 => Self::ThruConnectionsChanged,
            6 => Self::SerialPortOwnerChanged,
            7 => Self::IoError,
            _ => Self::Other,
        }
    }

    /// Whether the list of sources and destinations, or what it reports of one,
    /// may have changed: a watch lists again on such a notification and reports
    /// the difference.
    #[must_use]
    pub const fn changes_devices(self) -> bool {
        matches!(
            self,
            Self::SetupChanged | Self::ObjectAdded | Self::ObjectRemoved | Self::PropertyChanged
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `MIDINotificationMessageID` values of `MIDIServices.h`.
    #[test]
    fn message_ids_are_the_values_of_midi_services_h() {
        let ids = [
            (1, Notification::SetupChanged),
            (2, Notification::ObjectAdded),
            (3, Notification::ObjectRemoved),
            (4, Notification::PropertyChanged),
            (5, Notification::ThruConnectionsChanged),
            (6, Notification::SerialPortOwnerChanged),
            (7, Notification::IoError),
            (0x1000, Notification::Other),
            (0, Notification::Other),
            (-1, Notification::Other),
            (i32::MAX, Notification::Other),
        ];
        for (id, expected) in ids {
            assert_eq!(Notification::from_message_id(id), expected, "id {id}");
        }
    }

    /// A setup change, an object added or removed and a property change reach the
    /// device list; a thru, serial port or I/O notification does not.
    #[test]
    fn only_the_device_list_notifications_change_devices() {
        let changing: Vec<i32> = (-1..=0x1001)
            .filter(|id| Notification::from_message_id(*id).changes_devices())
            .collect();
        assert_eq!(changing, [1, 2, 3, 4]);
    }
}
