//! Mono Mode On, Control Change 126: its channel count (M2-104 §7.4.6.1, and
//! the MIDI 1.0 Detailed Specification's Omni-Off/Mono).

use tutti_ump::{CCNumber, ControlValue, Message, MonoChannels, U7, Ump, Voice2};

/// The value of the MIDI 2.0 Control Change 126 packet `words` carry.
fn cc126_value(value: u32) -> ControlValue {
    let packet = Ump([0x40B0_7E00, value, 0, 0]);
    match packet.decode() {
        Ok(Message::Midi2(_, Voice2::ControlChange { controller, value }))
            if controller == CCNumber::MONO_MODE_ON =>
        {
            value
        }
        other => panic!("{other:?}"),
    }
}

/// The channel count is the top 7 bits of the 32-bit value and the other 25
/// are ignored, whatever they hold; a count the MIDI 1.0 specification does
/// not define (past 16) is no count.
/// Spec: m2-104/7.4.6.1#2
#[test]
fn the_channel_count_is_the_top_seven_bits_of_the_value() {
    for count in 0..=127u32 {
        for low in [0, 1, 0x00FF_FFFF, 0x01FF_FFFF] {
            let read = MonoChannels::from_value(cc126_value(count << 25 | low));
            let want = u8::try_from(count).ok().and_then(MonoChannels::new);
            assert_eq!(read, want, "count {count}, low bits {low:#x}");
            assert_eq!(read.is_some(), count <= 16);
        }
    }
}

/// A count writes as the top 7 bits with the low 25 zero, and the MIDI 1.0
/// data byte is the count itself; 0 is the special case that gives every voice
/// a channel.
/// Spec: m2-104/7.4.6.1#2
#[test]
fn a_channel_count_writes_as_the_top_seven_bits_and_the_data_byte() {
    for count in 0..=16u8 {
        let channels = MonoChannels::of(count);
        assert_eq!(channels.to_value().get(), u32::from(count) << 25);
        assert_eq!(
            MonoChannels::from_value(channels.to_value()),
            Some(channels)
        );
        assert_eq!(channels.to_data_byte().get(), count);
        assert_eq!(
            MonoChannels::from_data_byte(channels.to_data_byte()),
            Some(channels)
        );
    }
    assert_eq!(MonoChannels::ALL_VOICES.get(), 0);
    assert_eq!(MonoChannels::from_data_byte(U7::of(17)), None);
    assert_eq!(MonoChannels::new(17), None);
}
