//! What one read from the sequencer returns, copied out of alsa-lib's buffer.

use std::time::Duration;

use crate::address::{Address, ClientId, PortId};
use crate::client::Packet;
use crate::sys;

/// One event read from a port, owned: alsa-lib's buffer is valid only until
/// the next read, so nothing here borrows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A UMP message.
    Ump {
        /// The message, its words padded with zeros to the event's fixed field.
        packet: Packet,
        /// When the port stamped the event, as the time since its queue
        /// started; `None` when the port is not stamping.
        stamp: Option<Duration>,
    },
    /// What the system announce port says.
    Announce(Announced),
    /// Any other event: the sequencer's own notice (a port subscribed to, an
    /// Endpoint changed). Its words are not a message and are not given.
    Notice,
}

impl Event {
    /// Copies what the backend needs out of `ev`.
    pub(crate) fn from_event(ev: &sys::snd_seq_ump_event_t) -> Self {
        if ev.type_ == sys::SND_SEQ_EVENT_UMP_PACKET {
            return Self::Ump {
                packet: Packet::from_field(ev.ump),
                stamp: ev
                    .real_time()
                    .map(|t| Duration::new(u64::from(t.tv_sec), t.tv_nsec)),
            };
        }
        Announced::of(ev).map_or(Self::Notice, Self::Announce)
    }
}

/// An announcement from the system announce port, with the client (and port)
/// it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Announced {
    /// A client opened: its number.
    ClientStart(ClientId),
    /// A client closed: its number.
    ClientExit(ClientId),
    /// A client set its info (its name among it): its number.
    ClientChange(ClientId),
    /// A port was created.
    PortStart(Address),
    /// A port was deleted.
    PortExit(Address),
    /// A port's info was set, its name or capabilities.
    PortChange(Address),
}

impl Announced {
    /// What `ev` announces, if it says a client or port started, exited or
    /// changed. The event's data begins with the address of the client (and
    /// port) it is about; one that is no address is no announcement.
    fn of(ev: &sys::snd_seq_ump_event_t) -> Option<Self> {
        let data = ev.ump[0].to_ne_bytes();
        let client = ClientId::new(data[0]);
        let address = || Some(Address::new(client?, PortId::new(data[1])?));
        match ev.type_ {
            sys::SND_SEQ_EVENT_CLIENT_START => client.map(Self::ClientStart),
            sys::SND_SEQ_EVENT_CLIENT_EXIT => client.map(Self::ClientExit),
            sys::SND_SEQ_EVENT_CLIENT_CHANGE => client.map(Self::ClientChange),
            sys::SND_SEQ_EVENT_PORT_START => address().map(Self::PortStart),
            sys::SND_SEQ_EVENT_PORT_EXIT => address().map(Self::PortExit),
            sys::SND_SEQ_EVENT_PORT_CHANGE => address().map(Self::PortChange),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: u8, client: u8, port: u8) -> Event {
        Event::from_event(&sys::snd_seq_ump_event_t {
            type_: kind,
            ump: [u32::from_ne_bytes([client, port, 0, 0]), 0, 0, 0],
            ..Default::default()
        })
    }

    /// Each announcement kind names its client, and a port kind its port too.
    #[test]
    fn an_announcement_names_its_client_and_port() {
        assert_eq!(
            event(sys::SND_SEQ_EVENT_CLIENT_START, 130, 9),
            Event::Announce(Announced::ClientStart(ClientId::of(130)))
        );
        assert_eq!(
            event(sys::SND_SEQ_EVENT_PORT_START, 130, 9),
            Event::Announce(Announced::PortStart(Address::of(130, 9)))
        );
        assert_eq!(
            event(sys::SND_SEQ_EVENT_PORT_EXIT, 4, 1),
            Event::Announce(Announced::PortExit(Address::of(4, 1)))
        );
        assert_eq!(
            event(sys::SND_SEQ_EVENT_CLIENT_CHANGE, 4, 0),
            Event::Announce(Announced::ClientChange(ClientId::of(4)))
        );
        assert_eq!(
            event(sys::SND_SEQ_EVENT_CLIENT_EXIT, 4, 0),
            Event::Announce(Announced::ClientExit(ClientId::of(4)))
        );
        assert_eq!(
            event(sys::SND_SEQ_EVENT_PORT_CHANGE, 4, 2),
            Event::Announce(Announced::PortChange(Address::of(4, 2)))
        );
    }

    /// Any other event (a note, say) is a notice, whatever its first word
    /// holds, and a notice carries no words to read as music.
    #[test]
    fn another_event_is_a_notice_never_a_message() {
        assert_eq!(event(sys::SND_SEQ_EVENT_NOTEON, 130, 9), Event::Notice);
        assert_eq!(event(66, 130, 9), Event::Notice);
        // An announcement whose bytes are no client or port says nothing.
        assert_eq!(
            event(sys::SND_SEQ_EVENT_PORT_START, 255, 255),
            Event::Notice
        );
        assert_eq!(event(sys::SND_SEQ_EVENT_PORT_START, 5, 254), Event::Notice);
        for kind in [
            sys::SND_SEQ_EVENT_CLIENT_START,
            sys::SND_SEQ_EVENT_CLIENT_EXIT,
            sys::SND_SEQ_EVENT_CLIENT_CHANGE,
        ] {
            assert_eq!(event(kind, 192, 0), Event::Notice, "{kind}");
        }
    }

    /// A UMP packet event carries its words, and the stamp is its real time as
    /// a duration, none when the flags say the stamp is a tick.
    #[test]
    fn a_ump_event_carries_its_words_and_real_time_stamp() {
        let mut ev = sys::snd_seq_ump_event_t {
            type_: sys::SND_SEQ_EVENT_UMP_PACKET,
            ump: [0x2090_4040, 0, 0, 0],
            ..Default::default()
        };
        ev.time[..4].copy_from_slice(&3u32.to_ne_bytes());
        ev.time[4..].copy_from_slice(&500u32.to_ne_bytes());
        ev.flags = sys::SND_SEQ_TIME_STAMP_REAL;
        let Event::Ump { packet, stamp } = Event::from_event(&ev) else {
            panic!("a UMP packet event is a message");
        };
        assert_eq!(packet.words(), [0x2090_4040, 0, 0, 0]);
        assert_eq!(stamp, Some(Duration::new(3, 500)));
        ev.flags = 0;
        assert!(matches!(
            Event::from_event(&ev),
            Event::Ump { stamp: None, .. }
        ));
    }
}
