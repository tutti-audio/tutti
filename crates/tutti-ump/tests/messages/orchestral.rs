//! The controllers of the Note On Orchestral Articulation Profile (M2-123 §8)
//! against Tables 19 and 20 and against words derived by hand.

use MuteType::*;
use tutti_ump::{
    Address, ControlValue, Message, MidiChannel, MidiGroup, MuteAmount, MuteMessage, MuteState,
    MuteType, NoteNumber, PlayingPosition, RegisteredPerNoteController, ReservedMuteType, U7, Ump,
    Voice2,
};

fn words(voice: Voice2) -> [u32; 4] {
    let address = Address {
        group: MidiGroup::of(0),
        channel: MidiChannel::of(0),
    };
    Ump::from(Message::Midi2(address, voice)).0
}

/// M2-123 Table 19, one row per subrange: the first and last value and the
/// type, typed from the table.
const TABLE_19: [(u32, u32, Result<MuteType, ()>); 32] = [
    (0x0000_0000, 0x07FF_FFFF, Ok(NoMute)),
    (0x0800_0000, 0x0FFF_FFFF, Ok(Straight)),
    (0x1000_0000, 0x17FF_FFFF, Ok(Practice)),
    (0x1800_0000, 0x1FFF_FFFF, Ok(Cup)),
    (0x2000_0000, 0x27FF_FFFF, Ok(WahStemIn)),
    (0x2800_0000, 0x2FFF_FFFF, Ok(WahStemExtended)),
    (0x3000_0000, 0x37FF_FFFF, Ok(WahStemRemoved)),
    (0x3800_0000, 0x3FFF_FFFF, Ok(Plunger)),
    (0x4000_0000, 0x47FF_FFFF, Ok(Bucket)),
    (0x4800_0000, 0x4FFF_FFFF, Ok(Mica)),
    (0x5000_0000, 0x57FF_FFFF, Ok(Solotone)),
    (0x5800_0000, 0x5FFF_FFFF, Ok(Whispa)),
    (0x6000_0000, 0x67FF_FFFF, Ok(Hat)),
    (0x6800_0000, 0x6FFF_FFFF, Ok(Hand)),
    (0x7000_0000, 0x77FF_FFFF, Ok(Stopped)),
    (0x7800_0000, 0x7FFF_FFFF, Ok(IntoTheStand)),
    // 0x80000000..=0xCFFFFFFF: reserved, ten subranges.
    (0x8000_0000, 0x87FF_FFFF, Err(())),
    (0x8800_0000, 0x8FFF_FFFF, Err(())),
    (0x9000_0000, 0x97FF_FFFF, Err(())),
    (0x9800_0000, 0x9FFF_FFFF, Err(())),
    (0xA000_0000, 0xA7FF_FFFF, Err(())),
    (0xA800_0000, 0xAFFF_FFFF, Err(())),
    (0xB000_0000, 0xB7FF_FFFF, Err(())),
    (0xB800_0000, 0xBFFF_FFFF, Err(())),
    (0xC000_0000, 0xC7FF_FFFF, Err(())),
    (0xC800_0000, 0xCFFF_FFFF, Err(())),
    (0xD000_0000, 0xD7FF_FFFF, Ok(Manufacturer1)),
    (0xD800_0000, 0xDFFF_FFFF, Ok(Manufacturer2)),
    (0xE000_0000, 0xE7FF_FFFF, Ok(Manufacturer3)),
    (0xE800_0000, 0xEFFF_FFFF, Ok(Manufacturer4)),
    (0xF000_0000, 0xF7FF_FFFF, Ok(Manufacturer5)),
    (0xF800_0000, 0xFFFF_FFFF, Ok(Manufacturer6)),
];

#[test]
fn table_19_mute_type_subranges() {
    for (first, last, expected) in TABLE_19 {
        let middle = first + (last - first) / 2;
        for value in [first, middle, last] {
            match (MuteType::from_value(value), expected) {
                (Ok(got), Ok(want)) => assert_eq!(got, want, "{value:#010x}"),
                (Err(ReservedMuteType(v)), Err(())) => assert_eq!(v, value),
                (got, want) => panic!("{value:#010x}: {got:?} against {want:?}"),
            }
        }
        // A sender writes the first value of the subrange.
        if let Ok(kind) = expected {
            assert_eq!(kind.value(), first);
        }
    }
    assert_eq!(MuteType::ALL.len(), 22);
    assert_eq!(MuteType::default(), NoMute);
}

#[test]
fn the_mute_controls_are_registered_controllers_20_22_and_20_23() {
    // Bits 31-28 type 4, group 0, status 2 (Registered Controller), channel 0,
    // then bank in 15-8 and index in 7-0; the value is the second word.
    assert_eq!(
        words(MuteMessage::Type(Cup).to_voice()),
        [0x4020_2022, 0x1800_0000, 0, 0]
    );
    assert_eq!(
        words(MuteMessage::Amount(MuteAmount::CLOSED).to_voice()),
        [0x4020_2023, 0xFFFF_FFFF, 0, 0]
    );
    assert_eq!(MuteType::RC.bank().get(), 0x20);
    assert_eq!(MuteType::RC.index().get(), 0x22);
    assert_eq!(MuteAmount::RC.bank().get(), 0x20);
    assert_eq!(MuteAmount::RC.index().get(), 0x23);
    let Ok(Message::Midi2(_, voice)) = Ump([0x4020_2022, 0x1800_0000, 0, 0]).decode() else {
        panic!("not a Midi2 message")
    };
    assert_eq!(
        MuteMessage::from_voice(&voice),
        Some(Ok(MuteMessage::Type(Cup)))
    );
}

#[test]
fn a_reserved_mute_type_is_refused_and_a_mute_amount_is_any_value() {
    // M2-123 Table 19: 0x80000000 to 0xCFFFFFFF are reserved.
    for value in [0x8000_0000, 0xCFFF_FFFF] {
        let voice = Voice2::registered(MuteType::RC, ControlValue::new(value));
        assert_eq!(
            MuteMessage::from_voice(&voice),
            Some(Err(ReservedMuteType(value)))
        );
    }
    let amount = Voice2::registered(MuteAmount::RC, ControlValue::new(0x1234_5678));
    assert_eq!(
        MuteMessage::from_voice(&amount),
        Some(Ok(MuteMessage::Amount(MuteAmount::new(0x1234_5678))))
    );
}

#[test]
fn another_registered_controller_is_not_a_mute_message() {
    for (bank, index) in [(0, 7), (0x20, 0x21), (0x20, 0x24), (0x21, 0x22)] {
        let voice = Voice2::Registered {
            bank: U7::of(bank),
            index: U7::of(index),
            value: ControlValue::new(0),
        };
        assert_eq!(
            MuteMessage::from_voice(&voice),
            None,
            "{bank:#x}/{index:#x}"
        );
    }
    assert_eq!(
        MuteMessage::from_voice(&Voice2::note_on(
            NoteNumber::of(60),
            tutti_ump::NoteVelocity::new(0)
        )),
        None
    );
}

/// M2-123 §8.1.1: selecting a Mute Type sets the Mute Amount to 0xFFFFFFFF;
/// §8.1.2: the default Amount is 0xFFFFFFFF and a later Amount changes only it.
#[test]
fn selecting_a_mute_type_closes_the_amount() {
    let start = MuteState::default();
    assert_eq!((start.mute_type, start.amount.get()), (NoMute, 0xFFFF_FFFF));
    let open = start.apply(MuteMessage::Amount(MuteAmount::OPEN));
    assert_eq!((open.mute_type, open.amount.get()), (NoMute, 0));
    let plunger = open.apply(MuteMessage::Type(Plunger));
    assert_eq!(
        (plunger.mute_type, plunger.amount.get()),
        (Plunger, 0xFFFF_FFFF)
    );
    let half = plunger.apply(MuteMessage::Amount(MuteAmount::new(0x8000_0000)));
    assert_eq!((half.mute_type, half.amount.get()), (Plunger, 0x8000_0000));
    // Selecting the same type again closes it again.
    assert_eq!(half.apply(MuteMessage::Type(Plunger)), plunger);
}

/// M2-123 §8.2, Table 20.
#[test]
fn table_20_playing_position_anchors() {
    assert_eq!(PlayingPosition::BRIDGE.get(), 0x0000_0000);
    assert_eq!(PlayingPosition::NORMAL.get(), 0x8000_0000);
    assert_eq!(PlayingPosition::NUT.get(), 0xFFFF_FFFF);
    assert_eq!(PlayingPosition::default(), PlayingPosition::NORMAL);
    assert_eq!(PlayingPosition::CONTROLLER, 0x0C);
}

#[test]
fn playing_position_is_registered_per_note_controller_0c() {
    // Status 0 (Registered Per-Note Controller), note 60, index 0x0C.
    assert_eq!(
        words(PlayingPosition::NORMAL.to_voice(NoteNumber::of(60))),
        [0x4000_3C0C, 0x8000_0000, 0, 0]
    );
    let Ok(Message::Midi2(_, voice)) = Ump([0x4000_3C0C, 0x0000_0000, 0, 0]).decode() else {
        panic!("not a Midi2 message")
    };
    assert_eq!(
        PlayingPosition::from_voice(&voice),
        Some((NoteNumber::of(60), PlayingPosition::BRIDGE))
    );
    assert!(matches!(
        voice,
        Voice2::RegisteredPerNote {
            controller: RegisteredPerNoteController::PlayingPosition,
            ..
        }
    ));
    assert_eq!(RegisteredPerNoteController::from(12).number(), 12);
    assert_eq!(
        RegisteredPerNoteController::from(12),
        RegisteredPerNoteController::PlayingPosition
    );
    assert_eq!(
        PlayingPosition::from_voice(&Voice2::RegisteredPerNote {
            note: NoteNumber::of(60),
            controller: RegisteredPerNoteController::Pan,
            value: ControlValue::new(0),
        }),
        None
    );
}
