use super::*;
use tutti_midi_protocol::SysexDeviceId;

fn every_value() -> impl Iterator<Item = SyncMessages> {
    let recipients = std::iter::once(None)
        .chain(std::iter::once(Some(Recipient::AllCall)))
        .chain((0..0x7F).map(|id| Some(Recipient::Device(SysexDeviceId::of(id)))));
    recipients.flat_map(|mmc| {
        MtcRate::ALL.into_iter().flat_map(move |rate| {
            (0..16).map(move |bits| SyncMessages {
                clock: bits & 1 != 0,
                mtc: bits & 2 != 0,
                flex: bits & 4 != 0,
                metronome: bits & 8 != 0,
                mmc,
                rate,
            })
        })
    })
}

/// A word `to_word` wrote reads back as the value, for every value: the
/// two switches, the rate and every recipient (the all-call and each of the 127
/// devices).
#[test]
fn every_value_round_trips_through_its_word() {
    let mut n = 0;
    for messages in every_value() {
        assert_eq!(
            SyncMessages::from_word(messages.to_word()),
            Some(messages),
            "{messages:?}"
        );
        n += 1;
    }
    assert_eq!(n, 16 * 4 * 129);
}

/// The word keeps the layout of the clock's old settings word (bit 0 clock,
/// bit 1 MTC, bits 8 to 15 the rate's code) and adds bit 2 for MMC, bit 3 for
/// Flex Data, bit 4 for the Flex metronome, with the device byte in bits 16 to 22.
#[test]
fn the_word_is_the_documented_layout() {
    let messages = SyncMessages {
        clock: true,
        mtc: true,
        mmc: Some(Recipient::Device(SysexDeviceId::of(5))),
        flex: true,
        metronome: true,
        rate: MtcRate::Fps2997Df,
    };
    assert_eq!(
        messages.to_word(),
        0b11 | 1 << 2 | 1 << 3 | 1 << 4 | 2 << 8 | 5 << 16
    );
    let all_call = SyncMessages {
        mmc: Some(Recipient::AllCall),
        ..SyncMessages::OFF
    };
    assert_eq!(all_call.to_word(), 1 << 2 | 1 << 8 | 0x7F << 16);
    assert_eq!(SyncMessages::OFF.to_word(), 1 << 8);
}

/// Any word reads as some value, and the value it reads is read from the
/// bits the layout names: the rate from the low two bits of its byte, the
/// device from the low seven bits of its field, the unnamed bits ignored.
#[test]
fn every_word_decodes() {
    let mut words = tutti_rt::SplitMix64::new(0x9E37_79B9_7F4A_7C15);
    for _ in 0..10_000 {
        // A spread of words with every bit position exercised.
        let word = words.next_u64();
        let read = SyncMessages::from_word(word).expect("decoding is total");
        assert_eq!(read.clock, word & 1 != 0);
        assert_eq!(read.mtc, word & 2 != 0);
        assert_eq!(read.flex, word & 8 != 0);
        assert_eq!(read.rate.code(), ((word >> 8) & 3) as u8);
        assert_eq!(read.mmc.is_some(), word & 4 != 0);
        if let Some(to) = read.mmc {
            assert_eq!(to.device_byte(), ((word >> 16) & 0x7F) as u8);
        }
    }
    // The extremes.
    assert!(SyncMessages::from_word(0).is_some());
    assert!(SyncMessages::from_word(u64::MAX).is_some());
}

/// A word with the MMC bit set and the device field `0x7F` reads as the
/// all-call, never as a device.
#[test]
fn device_byte_7f_reads_as_the_all_call() {
    let read = SyncMessages::from_word(1 << 2 | 0x7F << 16).unwrap();
    assert_eq!(read.mmc, Some(Recipient::AllCall));
}
