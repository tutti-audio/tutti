//! The `Midi1Translator` against M2-104 Appendix D.2 and M2-115, with figures written
//! here from the spec, never read from the code under test.

use tutti_midi_protocol::{
    MemberCount, Midi1Translator, Midi2Translator, MpeZoneConfig, PerNoteTranslation,
    PitchBendSensitivity, TranslationMode,
};
use tutti_ump::{
    Address, Bank, CCNumber, ExtremeValues, Message, Midi1Bytes, MidiChannel, MidiGroup,
    NoteNumber, NoteVelocity, U7, Ump, Voice1, Voice2,
};
use tutti_ump::{BendValue, ControlValue, MpeChannels, MpeZoneSpan};

const A: Address = Address::first(MidiChannel::FIRST);

fn feed(n: &mut Midi1Translator, message: Message) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    n.feed(message, |b: Midi1Bytes| out.push(b.as_slice().to_vec()));
    out
}

fn rpn(address: Address, bank: u8, index: u8, value: u32) -> Message {
    Message::Midi2(
        address,
        Voice2::Registered {
            bank: U7::of(bank),
            index: U7::of(index),
            value: ControlValue::new(value),
        },
    )
}

fn nrpn(address: Address, bank: u8, index: u8, value: u32) -> Message {
    Message::Midi2(
        address,
        Voice2::Assignable {
            bank: U7::of(bank),
            index: U7::of(index),
            value: ControlValue::new(value),
        },
    )
}

/// **D.2.3: one Registered Controller is four Control Changes**: CC 101 (bank)
/// and CC 100 (index), then CC 6 and CC 38 carrying the top 14 bits (Figure 97:
/// CC 6 is bits 31-25, CC 38 bits 24-18).
/// Spec: m2-104/6.2#4
#[test]
fn a_registered_controller_is_four_control_changes() {
    let mut n = Midi1Translator::new();
    // RPN 0/0 is zero-extended: 14-bit 0x1234 (MSB 0x24, LSB 0x34) in the top bits.
    assert_eq!(
        feed(&mut n, rpn(A, 0, 0, 0x1234 << 18)),
        [
            vec![0xB0, 101, 0],
            vec![0xB0, 100, 0],
            vec![0xB0, 6, 0x24],
            vec![0xB0, 38, 0x34]
        ]
    );
}

/// **D.2.3: an Assignable Controller selects with CC 99 and CC 98.**
#[test]
fn an_assignable_controller_selects_with_nrpn_controllers() {
    let mut n = Midi1Translator::new();
    let out = feed(
        &mut n,
        nrpn(
            Address::new(MidiGroup::of(0), MidiChannel::of(3)),
            5,
            9,
            0x8000_0000,
        ),
    );
    // Min-Center-Max centre (0x8000_0000) downscales by shift to 14-bit 0x2000.
    assert_eq!(
        out,
        [
            vec![0xB3, 99, 5],
            vec![0xB3, 98, 9],
            vec![0xB3, 6, 0x40],
            vec![0xB3, 38, 0x00]
        ]
    );
}

/// **D.2.3: each message generates a sequence of four MIDI 1.0 Protocol
/// messages**, however often the same parameter is sent: the select (CC 101 and
/// 100, or 99 and 98) is in every sequence, for the same parameter again, for
/// another one, for the other kind and on any channel or group.
#[test]
fn every_controller_is_the_full_sequence_of_four() {
    let mut n = Midi1Translator::new();
    let again = [
        vec![0xB0, 101, 0],
        vec![0xB0, 100, 0],
        vec![0xB0, 6, 1],
        vec![0xB0, 38, 0],
    ];
    assert_eq!(feed(&mut n, rpn(A, 0, 0, 0))[..2], again[..2]);
    assert_eq!(feed(&mut n, rpn(A, 0, 0, 1 << 25)), again, "same RPN again");
    assert_eq!(feed(&mut n, rpn(A, 0, 0, 1 << 25)), again, "and again");
    assert_eq!(feed(&mut n, rpn(A, 0, 1, 0)).len(), 4, "another index");
    let nrpn_seq = [
        vec![0xB0, 99, 0],
        vec![0xB0, 98, 1],
        vec![0xB0, 6, 0],
        vec![0xB0, 38, 0],
    ];
    assert_eq!(feed(&mut n, nrpn(A, 0, 1, 0)), nrpn_seq, "other kind");
    assert_eq!(feed(&mut n, nrpn(A, 0, 1, 0)), nrpn_seq, "NRPN again");
    assert_eq!(feed(&mut n, rpn(A, 0, 1, 0)).len(), 4, "back to the RPN");
    assert_eq!(
        feed(
            &mut n,
            rpn(Address::new(MidiGroup::of(0), MidiChannel::of(1)), 0, 1, 0)
        )
        .len(),
        4,
        "another channel"
    );
    assert_eq!(
        feed(
            &mut n,
            rpn(Address::new(MidiGroup::of(1), MidiChannel::of(0)), 0, 1, 0)
        )
        .len(),
        4,
        "another group"
    );
}

/// **A Control Change that passes through, or `reset`, changes nothing**: the
/// next controller is the same four messages (D.2.3).
#[test]
fn a_passing_select_or_reset_leaves_the_sequence_whole() {
    let full = || {
        [
            vec![0xB0, 101, 0],
            vec![0xB0, 100, 0],
            vec![0xB0, 6, 0],
            vec![0xB0, 38, 0],
        ]
    };
    for moved in [CCNumber::RPN_LSB, CCNumber::RESET_ALL] {
        let mut n = Midi1Translator::new();
        feed(&mut n, rpn(A, 0, 0, 0));
        feed(
            &mut n,
            Message::Midi1(
                A,
                Voice1::ControlChange {
                    controller: moved,
                    value: U7::of(9),
                },
            ),
        );
        assert_eq!(feed(&mut n, rpn(A, 0, 0, 0)), full());
    }
    let mut n = Midi1Translator::new();
    feed(&mut n, rpn(A, 0, 0, 0));
    n.reset();
    assert_eq!(feed(&mut n, rpn(A, 0, 0, 0)), full());
}

/// **M2-115 section 4.4.1 (`scaleDownRounding(v, 32, 14)`): RPN index 0 to 31
/// round, in any bank** (section 4.1: zero-extension "where the index (RPN LSB)
/// is 0-31"): add half the range `1 << 17`, shift 18, clamp to 0x3FFF.
/// 0x0001_FFFF is below the half (rounds down to 0); 0x0002_0000 is the half
/// (rounds up to 1); 0xFFFF_FFFF would be 0x4000 and clamps to 0x3FFF.
/// Spec: m2-115/4.1#1, m2-115/4.4#1, m2-115/4.4#2
#[test]
fn rpn_0_to_31_translates_with_rounding() {
    for (bank, index) in [(0, 7), (0, 31), (1, 0), (1, 31), (127, 7)] {
        let data = |value| {
            let mut n = Midi1Translator::new();
            let out = feed(&mut n, rpn(A, bank, index, value));
            (u16::from(out[2][2]) << 7) | u16::from(out[3][2])
        };
        assert_eq!(data(0x0001_FFFF), 0, "bank {bank} index {index}");
        assert_eq!(data(0x0002_0000), 1, "bank {bank} index {index}");
        assert_eq!(data(0xFFFF_FFFF), 0x3FFF, "bank {bank} index {index}");
    }
}

/// **Every other controller downscales by the plain shift** (M2-115 section 3.4):
/// RPN index 32 and up, another bank, and every NRPN keep the top 14 bits,
/// truncating where the zero-extended rule would round (0x0002_0000 is 0);
/// index 32 is the first scaled one in every bank (M2-115 section 3.1).
/// Spec: m2-115/2.2#2, m2-115/3.1#1
#[test]
fn other_controllers_translate_by_shift() {
    let data = |message: Message| {
        let mut n = Midi1Translator::new();
        let out = feed(&mut n, message);
        (u16::from(out[2][2]) << 7) | u16::from(out[3][2])
    };
    for message in [
        rpn(A, 0, 32, 0x0002_0000),
        rpn(A, 1, 32, 0x0002_0000),
        rpn(A, 1, 127, 0x0002_0000),
        nrpn(A, 0, 7, 0x0002_0000),
    ] {
        assert_eq!(data(message), 0, "{message:?}");
    }
    assert_eq!(data(rpn(A, 0, 32, 0xFFFF_FFFF)), 0x3FFF);
    assert_eq!(data(rpn(A, 1, 32, 0xFFFF_FFFF)), 0x3FFF);
}

/// **D.2.4: with the Bank Valid bit 1, a Program Change "translate[s] to three
/// MIDI 1.0 Protocol messages in the following order": Bank Select MSB, Bank
/// Select LSB, Program Change**, every time, the same bank again included; with
/// the bit 0, "only translate the Program Change value".
#[test]
fn a_program_change_with_a_bank_is_always_three_messages() {
    let pc = |program: u8, bank: Option<(u8, u8)>| {
        Message::Midi2(
            A,
            Voice2::ProgramChange {
                program: U7::of(program),
                bank: bank.map(|(msb, lsb)| Bank {
                    msb: U7::of(msb),
                    lsb: U7::of(lsb),
                }),
            },
        )
    };
    let mut n = Midi1Translator::new();
    assert_eq!(
        feed(&mut n, pc(5, Some((1, 2)))),
        [vec![0xB0, 0, 1], vec![0xB0, 32, 2], vec![0xC0, 5]]
    );
    assert_eq!(
        feed(&mut n, pc(6, Some((1, 2)))),
        [vec![0xB0, 0, 1], vec![0xB0, 32, 2], vec![0xC0, 6]],
        "the same bank again"
    );
    assert_eq!(feed(&mut n, pc(7, None)), [vec![0xC0, 7]]);
    assert_eq!(
        feed(&mut n, pc(8, Some((1, 3)))),
        [vec![0xB0, 0, 1], vec![0xB0, 32, 3], vec![0xC0, 8]]
    );
}

/// **D.2.1: a Note On whose velocity downscales to 0 is sent as 1**, and the
/// stateless messages pass as `midi1()` sends them.
#[test]
fn a_quiet_note_on_is_velocity_1() {
    let mut n = Midi1Translator::new();
    let on = Message::Midi2(
        A,
        Voice2::NoteOn {
            note: NoteNumber::of(60),
            velocity: NoteVelocity::new(0x0100),
            attribute: tutti_ump::Attribute::None,
        },
    );
    assert_eq!(feed(&mut n, on), [vec![0x90, 60, 1]]);
}

/// **D.2.8: the relative controllers have no MIDI 1.0 form.**
#[test]
fn relative_controllers_send_nothing() {
    let mut n = Midi1Translator::new();
    let m = Message::Midi2(
        A,
        Voice2::RelativeRegistered {
            bank: U7::of(0),
            index: U7::of(0),
            delta: 5,
        },
    );
    assert!(feed(&mut n, m).is_empty());
}

/// **Midi2Translator then Midi1Translator returns the CC sequence a MIDI 1.0 sender
/// wrote** (D.3.3 then D.2.3), for the zero-extended RPNs (every 14-bit value
/// of RPN 0/0, which the rounding must not disturb) and for the scaled ones
/// (every 14-bit value of an NRPN and of RPN 0/40).
#[test]
fn the_round_trip_returns_the_controller_sequence() {
    let cc = |n: u8, c: CCNumber, v: u8| -> Ump {
        Message::Midi1(
            Address::new(MidiGroup::of(0), MidiChannel::of(n)),
            Voice1::ControlChange {
                controller: c,
                value: U7::of(v),
            },
        )
        .into()
    };
    let selects = [
        (CCNumber::RPN_MSB, 0, CCNumber::RPN_LSB, 0),
        (CCNumber::RPN_MSB, 0, CCNumber::RPN_LSB, 40),
        (CCNumber::NRPN_MSB, 3, CCNumber::NRPN_LSB, 4),
    ];
    for (hi, hv, lo, lv) in selects {
        for data in (0..=0x3FFFu16).step_by(7).chain([0x3FFF]) {
            let (msb, lsb) = ((data >> 7) as u8, (data & 0x7F) as u8);
            let sent = [
                cc(0, hi, hv),
                cc(0, lo, lv),
                cc(0, CCNumber::DATA_ENTRY, msb),
                cc(0, CCNumber::DATA_ENTRY_LSB, lsb),
            ];
            let mut t = Midi2Translator::new();
            let mut n = Midi1Translator::new();
            let mut back = Vec::new();
            for ump in sent {
                if let Some(u) = t.translate(ump) {
                    n.feed(u, |b| back.push(b.as_slice().to_vec()));
                }
            }
            let want: Vec<Vec<u8>> = [
                (hi, hv),
                (lo, lv),
                (CCNumber::DATA_ENTRY, msb),
                (CCNumber::DATA_ENTRY_LSB, lsb),
            ]
            .iter()
            .map(|&(c, v)| vec![0xB0, c.get(), v])
            .collect();
            assert_eq!(back, want, "{hi:?} {data:#x}");
        }
    }
}

fn avoiding() -> Midi1Translator {
    Midi1Translator::with_mode(TranslationMode {
        extremes: ExtremeValues::Avoided,
        per_note: PerNoteTranslation::Dropped,
    })
}

fn cc_value(value: u32) -> Message {
    Message::Midi2(
        A,
        Voice2::ControlChange {
            controller: CCNumber::of(7),
            value: ControlValue::new(value),
        },
    )
}

fn note(on: bool, velocity: u16) -> Message {
    let (note, velocity, attribute) = (
        NoteNumber::of(60),
        NoteVelocity::new(velocity),
        tutti_ump::Attribute::None,
    );
    Message::Midi2(
        A,
        if on {
            Voice2::NoteOn {
                note,
                velocity,
                attribute,
            }
        } else {
            Voice2::NoteOff {
                note,
                velocity,
                attribute,
            }
        },
    )
}

/// **D.1.5: near-zero data translates to 1 and near-maximum to maximum - 1**,
/// and the minimum and maximum themselves stay (the default shift sends
/// 0x0000_0001 to 0 and 0xFFFF_FFFE to 127, D.1.4's `value >> 25`).
/// Spec: m2-104/d.1.5#1
#[test]
fn avoided_extremes_move_near_values_off_the_minimum_and_maximum() {
    let mut default = Midi1Translator::new();
    let mut avoided = avoiding();
    for (value, scaled, kept) in [
        (0x0000_0000, 0, 0),
        (0x0000_0001, 0, 1),
        (0x01FF_FFFF, 0, 1),
        (0x0200_0000, 1, 1),
        (0xFDFF_FFFF, 126, 126),
        (0xFE00_0000, 127, 126),
        (0xFFFF_FFFE, 127, 126),
        (0xFFFF_FFFF, 127, 127),
    ] {
        assert_eq!(
            feed(&mut default, cc_value(value)),
            [vec![0xB0, 7, scaled]],
            "{value:#x}"
        );
        assert_eq!(
            feed(&mut avoided, cc_value(value)),
            [vec![0xB0, 7, kept]],
            "{value:#x}"
        );
    }
}

/// **D.1.5 on every kind of value**: a 16-bit velocity (Note Off 0x0001 is 1
/// where the shift gives 0; Note On 0xFFFE is 126), a 14-bit Pitch Bend, pressure
/// and the data of an Assignable Controller.
/// Spec: m2-104/d.1.5#1
#[test]
fn avoided_extremes_cover_velocity_bend_pressure_and_controller_data() {
    let mut n = avoiding();
    assert_eq!(feed(&mut n, note(false, 0x0001)), [vec![0x80, 60, 1]]);
    assert_eq!(feed(&mut n, note(false, 0x0000)), [vec![0x80, 60, 0]]);
    assert_eq!(feed(&mut n, note(true, 0xFFFE)), [vec![0x90, 60, 126]]);
    assert_eq!(feed(&mut n, note(true, 0xFFFF)), [vec![0x90, 60, 127]]);
    let bend = |v: u32| Message::Midi2(A, Voice2::PitchBend(BendValue::new(v)));
    assert_eq!(feed(&mut n, bend(0x0000_0001)), [vec![0xE0, 0x01, 0x00]]);
    assert_eq!(feed(&mut n, bend(0xFFFF_FFFE)), [vec![0xE0, 0x7E, 0x7F]]);
    assert_eq!(feed(&mut n, bend(0xFFFF_FFFF)), [vec![0xE0, 0x7F, 0x7F]]);
    let pressure = Message::Midi2(A, Voice2::ChannelPressure(ControlValue::new(1)));
    assert_eq!(feed(&mut n, pressure), [vec![0xD0, 1]]);
    // NRPN 0/0 at 0xFFFF_FFFE: 14 bits of 0x3FFF is the maximum, so 0x3FFE:
    // MSB 0x7F, LSB 0x7E.
    assert_eq!(
        feed(&mut n, nrpn(A, 0, 0, 0xFFFF_FFFE)),
        [
            vec![0xB0, 99, 0],
            vec![0xB0, 98, 0],
            vec![0xB0, 6, 0x7F],
            vec![0xB0, 38, 0x7E]
        ]
    );
}

/// **A zero-extended Registered Controller is no scaled value** (D.1.5 names
/// no property; M2-115 section 4.1 makes these fixed-point numbers or pairs of
/// fields): both ends downscale as in the default. 1 rounds to 0 and 0xFFFF_FFFE
/// clamps to the 14-bit maximum, MSB 127, LSB 127.
#[test]
fn avoided_extremes_leave_zero_extended_registered_controllers() {
    for (value, msb, lsb) in [(1, 0, 0), (0xFFFF_FFFE, 0x7F, 0x7F)] {
        let want = [
            vec![0xB0, 101, 0],
            vec![0xB0, 100, 0],
            vec![0xB0, 6, msb],
            vec![0xB0, 38, lsb],
        ];
        assert_eq!(
            feed(&mut avoiding(), rpn(A, 0, 0, value)),
            want,
            "{value:#x}"
        );
        assert_eq!(
            feed(&mut Midi1Translator::new(), rpn(A, 0, 0, value)),
            want,
            "{value:#x}"
        );
    }
}

fn zone() -> MpeZoneConfig {
    MpeZoneConfig::lower(
        MemberCount::of(2),
        PitchBendSensitivity::MPE_MASTER_DEFAULT,
        PitchBendSensitivity::MPE_MEMBER_DEFAULT,
    )
}

fn mpe(zone: MpeZoneConfig) -> Midi1Translator {
    Midi1Translator::with_mode(TranslationMode {
        extremes: ExtremeValues::Scaled,
        per_note: PerNoteTranslation::Mpe(zone),
    })
}

fn note_on(note: u8) -> Message {
    Message::Midi2(
        A,
        Voice2::NoteOn {
            note: NoteNumber::of(note),
            velocity: NoteVelocity::new(0x8000),
            attribute: tutti_ump::Attribute::None,
        },
    )
}

fn per_note_bend(note: u8, value: u32) -> Message {
    Message::Midi2(
        A,
        Voice2::PerNotePitchBend {
            note: NoteNumber::of(note),
            value: BendValue::new(value),
        },
    )
}

/// **D.2.8: Per-Note Pitch Bend has no default translation, and an Alternate
/// Translation Mode may translate it**: MPE gives each note a member channel
/// (here Ch2 and Ch3 of the lower zone), and the note's bend is that
/// channel's Pitch Bend (centre 0x2000 for 0x8000_0000). The default sends
/// nothing.
/// Spec: m2-104/d.2.8#1, m2-104/d.4.2#1
#[test]
fn mpe_gives_each_note_a_member_channel_for_its_per_note_bend() {
    let mut n = mpe(zone());
    assert_eq!(feed(&mut n, note_on(60)), [vec![0x91, 60, 0x40]]);
    assert_eq!(feed(&mut n, note_on(64)), [vec![0x92, 64, 0x40]]);
    assert_eq!(
        feed(&mut n, per_note_bend(60, 0x8000_0000)),
        [vec![0xE1, 0x00, 0x40]]
    );
    assert_eq!(
        feed(&mut n, per_note_bend(64, 0xFFFF_FFFF)),
        [vec![0xE2, 0x7F, 0x7F]]
    );
    assert!(feed(&mut Midi1Translator::new(), per_note_bend(60, 0x8000_0000)).is_empty());
}

/// **D.4.1: a device that understands the MIDI-CI MPE Profile uses its
/// translation**: a Profile zone with its Manager on Ch4 (index 3) and two
/// Members puts the notes on Ch5 and Ch6.
/// Spec: m2-104/d.4.1#1
#[test]
fn a_profile_zone_puts_notes_on_the_members_above_its_manager() {
    let span = MpeZoneSpan::new(MidiChannel::of(3), MpeChannels::of(3)).unwrap();
    let zone = MpeZoneConfig::profile(span, PitchBendSensitivity::MPE_PROFILE_DEFAULT);
    let mut n = mpe(zone);
    assert_eq!(feed(&mut n, note_on(60)), [vec![0x94, 60, 0x40]]);
    assert_eq!(feed(&mut n, note_on(61)), [vec![0x95, 61, 0x40]]);
}

/// **A note's pressure, slide and Note Off stay on its channel** (M1-100
/// section 2.2: MPE gives a member channel its note's expression), and the
/// channel is free again after the Note Off: the next note takes the
/// other idle member first, the one idle longest.
/// Spec: m2-104/d.2.8#1
#[test]
fn mpe_expression_and_release_follow_the_note_channel() {
    let mut n = mpe(zone());
    let _ = feed(&mut n, note_on(60));
    let pressure = Message::Midi2(
        A,
        Voice2::PolyPressure {
            note: NoteNumber::of(60),
            value: ControlValue::new(0x8000_0000),
        },
    );
    assert_eq!(feed(&mut n, pressure), [vec![0xD1, 0x40]]);
    let slide = Message::Midi2(
        A,
        Voice2::AssignablePerNote {
            note: NoteNumber::of(60),
            controller: 74,
            value: ControlValue::new(0xFFFF_FFFF),
        },
    );
    assert_eq!(feed(&mut n, slide), [vec![0xB1, 74, 0x7F]]);
    assert_eq!(feed(&mut n, note(false, 0)), [vec![0x81, 60, 0]]);
    // Ch2 was just freed; Ch3 has been idle longer.
    assert_eq!(feed(&mut n, note_on(62)), [vec![0x92, 62, 0x40]]);
}

/// **With every member busy the oldest note is ended on its channel and the
/// channel is given to the new one**, and the new note starts from a centred
/// bend when the channel's last note moved it.
/// Spec: m2-104/d.2.8#1
#[test]
fn mpe_steals_the_oldest_member_and_resets_its_bend() {
    let mut n = mpe(zone());
    let _ = feed(&mut n, note_on(60));
    let _ = feed(&mut n, note_on(64));
    let _ = feed(&mut n, per_note_bend(60, 0xFFFF_FFFF));
    assert_eq!(
        feed(&mut n, note_on(67)),
        [
            vec![0x81, 60, 0],
            vec![0xE1, 0x00, 0x40],
            vec![0x91, 67, 0x40]
        ]
    );
}

/// **The longest sequence one message makes is `MAX_MESSAGES` and
/// `MAX_BYTES`**: a key struck again on a member whose note was bent, pressed
/// and slid ends that note, centres the three, then sounds.
#[test]
fn a_restruck_mpe_note_is_the_longest_sequence() {
    let mut n = mpe(zone());
    let _ = feed(&mut n, note_on(60));
    let _ = feed(&mut n, per_note_bend(60, 0xFFFF_FFFF));
    let pressure = Message::Midi2(
        A,
        Voice2::PolyPressure {
            note: NoteNumber::of(60),
            value: ControlValue::new(0x8000_0000),
        },
    );
    let _ = feed(&mut n, pressure);
    let slide = Message::Midi2(
        A,
        Voice2::AssignablePerNote {
            note: NoteNumber::of(60),
            controller: 74,
            value: ControlValue::new(0xFFFF_FFFF),
        },
    );
    let _ = feed(&mut n, slide);
    let sent = feed(&mut n, note_on(60));
    assert_eq!(
        sent,
        [
            vec![0x81, 60, 0],
            vec![0xE1, 0x00, 0x40],
            vec![0xD1, 0],
            vec![0xB1, 74, 0x40],
            vec![0x91, 60, 0x40],
        ]
    );
    assert_eq!(sent.len(), Midi1Translator::MAX_MESSAGES);
    assert_eq!(
        sent.iter().map(Vec::len).sum::<usize>(),
        Midi1Translator::MAX_BYTES
    );
}

/// The Default Translation's longest sequence, a controller's four Control
/// Changes, stays under the bounds.
#[test]
fn the_default_translation_stays_under_the_bounds() {
    let sent = feed(&mut Midi1Translator::new(), rpn(A, 0, 0, 12 << 25));
    assert_eq!(sent.len(), 4);
    assert_eq!(sent.iter().map(Vec::len).sum::<usize>(), 12);
}

/// **A steal is as long as a re-strike**: the oldest member's note, bent,
/// pressed and slid, ends, the three centre, and the new note sounds there.
#[test]
fn a_stealing_mpe_note_is_the_longest_sequence() {
    let one = one_member_zone();
    let mut n = mpe(one);
    let _ = feed(&mut n, note_on(60));
    let _ = feed(&mut n, per_note_bend(60, 0xFFFF_FFFF));
    let _ = feed(
        &mut n,
        Message::Midi2(
            A,
            Voice2::PolyPressure {
                note: NoteNumber::of(60),
                value: ControlValue::new(0x8000_0000),
            },
        ),
    );
    let _ = feed(
        &mut n,
        Message::Midi2(
            A,
            Voice2::AssignablePerNote {
                note: NoteNumber::of(60),
                controller: 74,
                value: ControlValue::new(0xFFFF_FFFF),
            },
        ),
    );
    let sent = feed(&mut n, note_on(64));
    assert_eq!(sent.len(), Midi1Translator::MAX_MESSAGES);
    assert_eq!(
        sent.iter().map(Vec::len).sum::<usize>(),
        Midi1Translator::MAX_BYTES
    );
    assert_eq!(sent[0], [sent[4][0] & 0xEF, 60, 0]);
    assert_eq!(sent[4][1], 64);
}

/// A mode is alternate unless it is the Default Translation, so a product can
/// tell its user (D.4).
#[test]
fn the_default_mode_is_the_default_translation() {
    assert!(!TranslationMode::default().is_alternate());
    assert_eq!(Midi1Translator::new().mode(), TranslationMode::default());
    assert!(mpe(zone()).mode().is_alternate());
    assert!(avoiding().mode().is_alternate());
}

fn mpe_avoiding() -> Midi1Translator {
    Midi1Translator::with_mode(TranslationMode {
        extremes: ExtremeValues::Avoided,
        per_note: PerNoteTranslation::Mpe(zone()),
    })
}

/// **Every message that is not per-note goes on the Manager Channel** (M2-120
/// sections 4.1 to 4.6): a Pitch Bend, a Program Change with its Bank Select
/// and a Control Change from source Ch6 (index 5) go on Ch1 (index 0), the
/// lower zone's Manager, not on Ch6 nor on a member.
/// Spec: m2-104/d.2.8#1, m2-120/4.6#1
#[test]
fn mpe_sends_channel_messages_on_the_manager_channel() {
    let mut n = mpe(zone());
    let from = Address::first(MidiChannel::of(5));
    let bend = Message::Midi2(from, Voice2::PitchBend(BendValue::new(0x8000_0000)));
    assert_eq!(feed(&mut n, bend), [vec![0xE0, 0x00, 0x40]]);
    let program = Message::Midi2(
        from,
        Voice2::ProgramChange {
            program: U7::of(8),
            bank: Some(Bank {
                msb: U7::of(1),
                lsb: U7::of(3),
            }),
        },
    );
    assert_eq!(
        feed(&mut n, program),
        [vec![0xB0, 0, 1], vec![0xB0, 32, 3], vec![0xC0, 8]]
    );
    let volume = Message::Midi2(
        from,
        Voice2::ControlChange {
            controller: CCNumber::of(7),
            value: ControlValue::new(0xFFFF_FFFF),
        },
    );
    assert_eq!(feed(&mut n, volume), [vec![0xB0, 7, 0x7F]]);
}

/// **Both modes together**: a per-note bend of 0x0000_0001 is the 14-bit 1
/// (LSB 1, MSB 0) where the shift gives 0, and a Poly Pressure of 0xFFFF_FFFE
/// is Channel Pressure 126 where the shift gives 127.
/// Spec: m2-104/d.1.5#1
#[test]
fn mpe_and_avoided_extremes_combine() {
    let mut n = mpe_avoiding();
    let _ = feed(&mut n, note_on(60));
    assert_eq!(feed(&mut n, per_note_bend(60, 1)), [vec![0xE1, 0x01, 0x00]]);
    let pressure = Message::Midi2(
        A,
        Voice2::PolyPressure {
            note: NoteNumber::of(60),
            value: ControlValue::new(0xFFFF_FFFE),
        },
    );
    assert_eq!(feed(&mut n, pressure), [vec![0xD1, 126]]);
    let slide = Message::Midi2(
        A,
        Voice2::AssignablePerNote {
            note: NoteNumber::of(60),
            controller: 74,
            value: ControlValue::new(1),
        },
    );
    assert_eq!(feed(&mut n, slide), [vec![0xB1, 74, 1]]);
}

/// **The same key struck twice ends its first note** on the same member, so a
/// single Note Off later ends the only one sounding.
/// Spec: m2-104/d.2.8#1
#[test]
fn mpe_retriggers_a_key_on_its_member_after_a_note_off() {
    let mut n = mpe(zone());
    let _ = feed(&mut n, note_on(60));
    assert_eq!(
        feed(&mut n, note_on(60)),
        [vec![0x81, 60, 0], vec![0x91, 60, 0x40]]
    );
}

/// **A reused member starts from reset expression**: a slide sent to the first
/// note is centred to 0x40 (M1-100 A.4.3: CC 74 at Note On is 0x40, and the
/// section 2.2 example's initial timbre is 0xB2 0x4A 0x40) before the next
/// note on that channel.
/// Spec: m2-104/d.2.8#1
#[test]
fn mpe_centres_a_reused_members_slide() {
    let mut n = mpe(one_member_zone());
    let _ = feed(&mut n, note_on(60));
    let slide = Message::Midi2(
        A,
        Voice2::AssignablePerNote {
            note: NoteNumber::of(60),
            controller: 74,
            value: ControlValue::new(0xFFFF_FFFF),
        },
    );
    let _ = feed(&mut n, slide);
    let _ = feed(&mut n, note(false, 0));
    // One member only: the next note reuses Ch2.
    assert_eq!(
        feed(&mut n, note_on(62)),
        [vec![0xB1, 74, 0x40], vec![0x91, 62, 0x40]]
    );
}

fn one_member_zone() -> MpeZoneConfig {
    MpeZoneConfig::lower(
        MemberCount::of(1),
        PitchBendSensitivity::MPE_MASTER_DEFAULT,
        PitchBendSensitivity::MPE_MEMBER_DEFAULT,
    )
}

/// **The Manager Channel is the zone's, not index 0**: the upper zone's Manager
/// is Ch16 (index 15, RP-053), and a Profile Manager on Ch4 (index 3) is its
/// own; a bend from source Ch1 goes there.
/// Spec: m2-104/d.2.8#1
#[test]
fn mpe_manager_channel_is_the_zones_own() {
    let bend = Message::Midi2(A, Voice2::PitchBend(BendValue::new(0x8000_0000)));
    let upper = MpeZoneConfig::upper(
        MemberCount::of(2),
        PitchBendSensitivity::MPE_MASTER_DEFAULT,
        PitchBendSensitivity::MPE_MEMBER_DEFAULT,
    );
    assert_eq!(feed(&mut mpe(upper), bend), [vec![0xEF, 0x00, 0x40]]);
    let span = MpeZoneSpan::new(MidiChannel::of(3), MpeChannels::of(3)).unwrap();
    let profile = MpeZoneConfig::profile(span, PitchBendSensitivity::MPE_PROFILE_DEFAULT);
    assert_eq!(feed(&mut mpe(profile), bend), [vec![0xE3, 0x00, 0x40]]);
}
