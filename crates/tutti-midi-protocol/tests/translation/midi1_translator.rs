//! The `Midi1Translator` against M2-104 Appendix D.2 and M2-115, with figures written
//! here from the spec, never read from the code under test.

use tutti_midi_protocol::{Midi1Translator, Midi2Translator};
use tutti_ump::{
    Address, Bank, CCNumber, ExtremeValues, Message, Midi1Bytes, MidiChannel, MidiGroup,
    NoteNumber, NoteVelocity, U7, Ump, Voice1, Voice2,
};
use tutti_ump::{BendValue, ControlValue};

const A: Address = Address::first(MidiChannel::FIRST);

fn feed(n: &Midi1Translator, message: Message) -> Vec<Vec<u8>> {
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
    let n = Midi1Translator::new();
    // RPN 0/0 is zero-extended: 14-bit 0x1234 (MSB 0x24, LSB 0x34) in the top bits.
    assert_eq!(
        feed(&n, rpn(A, 0, 0, 0x1234 << 18)),
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
    let n = Midi1Translator::new();
    let out = feed(
        &n,
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
    let n = Midi1Translator::new();
    let again = [
        vec![0xB0, 101, 0],
        vec![0xB0, 100, 0],
        vec![0xB0, 6, 1],
        vec![0xB0, 38, 0],
    ];
    assert_eq!(feed(&n, rpn(A, 0, 0, 0))[..2], again[..2]);
    assert_eq!(feed(&n, rpn(A, 0, 0, 1 << 25)), again, "same RPN again");
    assert_eq!(feed(&n, rpn(A, 0, 0, 1 << 25)), again, "and again");
    assert_eq!(feed(&n, rpn(A, 0, 1, 0)).len(), 4, "another index");
    let nrpn_seq = [
        vec![0xB0, 99, 0],
        vec![0xB0, 98, 1],
        vec![0xB0, 6, 0],
        vec![0xB0, 38, 0],
    ];
    assert_eq!(feed(&n, nrpn(A, 0, 1, 0)), nrpn_seq, "other kind");
    assert_eq!(feed(&n, nrpn(A, 0, 1, 0)), nrpn_seq, "NRPN again");
    assert_eq!(feed(&n, rpn(A, 0, 1, 0)).len(), 4, "back to the RPN");
    assert_eq!(
        feed(
            &n,
            rpn(Address::new(MidiGroup::of(0), MidiChannel::of(1)), 0, 1, 0)
        )
        .len(),
        4,
        "another channel"
    );
    assert_eq!(
        feed(
            &n,
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
        let n = Midi1Translator::new();
        feed(&n, rpn(A, 0, 0, 0));
        feed(
            &n,
            Message::Midi1(
                A,
                Voice1::ControlChange {
                    controller: moved,
                    value: U7::of(9),
                },
            ),
        );
        assert_eq!(feed(&n, rpn(A, 0, 0, 0)), full());
    }
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
            let n = Midi1Translator::new();
            let out = feed(&n, rpn(A, bank, index, value));
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
        let n = Midi1Translator::new();
        let out = feed(&n, message);
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
    let n = Midi1Translator::new();
    assert_eq!(
        feed(&n, pc(5, Some((1, 2)))),
        [vec![0xB0, 0, 1], vec![0xB0, 32, 2], vec![0xC0, 5]]
    );
    assert_eq!(
        feed(&n, pc(6, Some((1, 2)))),
        [vec![0xB0, 0, 1], vec![0xB0, 32, 2], vec![0xC0, 6]],
        "the same bank again"
    );
    assert_eq!(feed(&n, pc(7, None)), [vec![0xC0, 7]]);
    assert_eq!(
        feed(&n, pc(8, Some((1, 3)))),
        [vec![0xB0, 0, 1], vec![0xB0, 32, 3], vec![0xC0, 8]]
    );
}

/// **D.2.1: a Note On whose velocity downscales to 0 is sent as 1**, and the
/// stateless messages pass as `midi1()` sends them.
#[test]
fn a_quiet_note_on_is_velocity_1() {
    let n = Midi1Translator::new();
    let on = Message::Midi2(
        A,
        Voice2::NoteOn {
            note: NoteNumber::of(60),
            velocity: NoteVelocity::new(0x0100),
            attribute: tutti_ump::Attribute::None,
        },
    );
    assert_eq!(feed(&n, on), [vec![0x90, 60, 1]]);
}

/// **D.2.8: the relative controllers have no MIDI 1.0 form.**
#[test]
fn relative_controllers_send_nothing() {
    let n = Midi1Translator::new();
    let m = Message::Midi2(
        A,
        Voice2::RelativeRegistered {
            bank: U7::of(0),
            index: U7::of(0),
            delta: 5,
        },
    );
    assert!(feed(&n, m).is_empty());
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
            let n = Midi1Translator::new();
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
    Midi1Translator::new().extremes(ExtremeValues::Avoided)
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
    let default = Midi1Translator::new();
    let avoided = avoiding();
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
            feed(&default, cc_value(value)),
            [vec![0xB0, 7, scaled]],
            "{value:#x}"
        );
        assert_eq!(
            feed(&avoided, cc_value(value)),
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
    let n = avoiding();
    assert_eq!(feed(&n, note(false, 0x0001)), [vec![0x80, 60, 1]]);
    assert_eq!(feed(&n, note(false, 0x0000)), [vec![0x80, 60, 0]]);
    assert_eq!(feed(&n, note(true, 0xFFFE)), [vec![0x90, 60, 126]]);
    assert_eq!(feed(&n, note(true, 0xFFFF)), [vec![0x90, 60, 127]]);
    let bend = |v: u32| Message::Midi2(A, Voice2::PitchBend(BendValue::new(v)));
    assert_eq!(feed(&n, bend(0x0000_0001)), [vec![0xE0, 0x01, 0x00]]);
    assert_eq!(feed(&n, bend(0xFFFF_FFFE)), [vec![0xE0, 0x7E, 0x7F]]);
    assert_eq!(feed(&n, bend(0xFFFF_FFFF)), [vec![0xE0, 0x7F, 0x7F]]);
    let pressure = Message::Midi2(A, Voice2::ChannelPressure(ControlValue::new(1)));
    assert_eq!(feed(&n, pressure), [vec![0xD0, 1]]);
    // NRPN 0/0 at 0xFFFF_FFFE: 14 bits of 0x3FFF is the maximum, so 0x3FFE:
    // MSB 0x7F, LSB 0x7E.
    assert_eq!(
        feed(&n, nrpn(A, 0, 0, 0xFFFF_FFFE)),
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
        assert_eq!(feed(&avoiding(), rpn(A, 0, 0, value)), want, "{value:#x}");
        assert_eq!(
            feed(&Midi1Translator::new(), rpn(A, 0, 0, value)),
            want,
            "{value:#x}"
        );
    }
}

/// The longest sequence one message makes, a controller's four Control
/// Changes, is `MAX_MESSAGES` and `MAX_BYTES`.
#[test]
fn a_controllers_sequence_is_the_longest() {
    let sent = feed(&Midi1Translator::new(), rpn(A, 0, 0, 12 << 25));
    assert_eq!(sent.len(), Midi1Translator::MAX_MESSAGES);
    let bytes: usize = sent.iter().map(Vec::len).sum();
    assert_eq!(bytes, Midi1Translator::MAX_BYTES);
}
