//! The Universal System Exclusive grammar: the payload bytes of a checked
//! [`SysexMessage`](tutti_ump::SysexMessage) into a [`Universal`], built from
//! `winnow` combinators.
//!
//! ```text
//! message   := 7E device ( general_information | general_midi ) eof   -- Non-Real Time
//!            | 7F device device_control eof                           -- Real Time
//! general_information := 06 ( 01                                      -- Identity Request
//!                           | 02 manufacturer family model revision ) -- Identity Reply
//! general_midi        := 09 ( 01 | 02 )                               -- GM System On, Off
//! tuning_nrt          := 08 ( 00 u7                                   -- dump request
//!                           | 01 u7 name key_table chk                -- bulk dump
//!                           | 03 u7 u7 | 04 u7 u7 name key_table chk  -- bank request, key dump
//!                           | 05 u7 u7 name 12*u7 chk                 -- octave dump, 1 byte
//!                           | 06 u7 u7 name 12*u14 chk                -- octave dump, 2 byte
//!                           | 07 u7 u7 count change{count}            -- note change
//!                           | 08 mask 12*u7 | 09 mask 12*u14 )        -- octave tuning
//! tuning_rt           := 08 ( 02 u7 count change{count}               -- note change
//!                           | 07 u7 u7 count change{count}            -- note change, bank
//!                           | 08 mask 12*u7 | 09 mask 12*u14 )        -- octave tuning
//! change              := key frequency                                -- key, 3-byte word
//! mask                := u7(0..=3) u7 u7                              -- channels 15-16, 8-14, 1-7
//! mtc_full            := 01 01 hr mn sc fr                            -- hr is 0rrhhhhh
//! device_control      := 04 ( 01 u14 | 02 u14 | 03 u14 | 04 00 u7 )   -- volume, balance, tuning
//! manufacturer        := 00 xx yy | mm                                -- one byte or three
//! ```
//!
//! Every parser is `fn(&mut &[u8]) -> Res<T>`. A data byte is 7-bit before it
//! gets here (`SysexMessage` checked it), and a field is read as the unit type
//! that holds it, so no rule masks or shifts a byte. A message that matches no
//! rule is not an error to the caller: [`parse`] returns `None`, and
//! [`read()`](fn@super::read) says whether it was Universal at all.

use winnow::Parser;
use winnow::combinator::{alt, eof, fail, preceded, seq};
use winnow::error::ContextError;
use winnow::token::{any, literal};

use tutti_ump::{
    DeviceIdentity, ManufacturerId, NoteNumber, SoftwareRevision, U7, U14, UNIVERSAL_NON_REAL_TIME,
    UNIVERSAL_REAL_TIME,
};

use super::{
    Addressed, BadChecksum, GeneralMidiMode, MtcFullMessage, Recipient, Universal, UniversalRead,
    machine_control,
};
use crate::tuning::{
    ChannelMask, KeyChange, KeyChanges, KeyTable, MAX_KEY_CHANGES, OctaveOffsets, Timing,
    TuningFrequency, TuningMessage, TuningName,
};

type Res<T> = winnow::ModalResult<T, ContextError>;

/// A 7-bit data byte as a [`U7`].
fn u7(input: &mut &[u8]) -> Res<U7> {
    any.verify_map(U7::new).parse_next(input)
}

/// A 14-bit value sent least significant byte first (M1 p.40, p.57).
fn u14(input: &mut &[u8]) -> Res<U14> {
    (u7, u7)
        .map(|(lsb, msb)| U14::from_halves(msb, lsb))
        .parse_next(input)
}

fn device(input: &mut &[u8]) -> Res<Recipient> {
    u7.map(Recipient::of_byte).parse_next(input)
}

/// The manufacturer id of an Identity Reply: one byte, or `00` and two more
/// (M1 p.40: an id beginning with 00H extends the message by two bytes).
fn manufacturer(input: &mut &[u8]) -> Res<ManufacturerId> {
    alt((
        preceded(literal(0x00_u8), (u7, u7))
            .verify_map(|(a, b)| ManufacturerId::try_from([U7::of(0), a, b]).ok()),
        u7.verify_map(|m| ManufacturerId::new(u16::from(m.get()))),
    ))
    .parse_next(input)
}

/// The Identity Reply's fields after its sub-ids (M1 p.40).
fn identity(input: &mut &[u8]) -> Res<DeviceIdentity> {
    seq!(DeviceIdentity {
        manufacturer: manufacturer,
        family: (u7, u7).map(|(a, b)| [a, b]),
        model: (u7, u7).map(|(a, b)| [a, b]),
        revision: (u7, u7, u7, u7).map(|(a, b, c, d)| SoftwareRevision::from([a, b, c, d])),
    })
    .parse_next(input)
}

/// `06`, General Information (M1 p.40).
fn general_information(input: &mut &[u8]) -> Res<Universal> {
    preceded(
        literal(0x06_u8),
        alt((
            literal(0x01_u8).value(Universal::IdentityRequest),
            preceded(literal(0x02_u8), identity).map(Universal::IdentityReply),
        )),
    )
    .parse_next(input)
}

/// `09`, General MIDI (M1 p.52).
fn general_midi(input: &mut &[u8]) -> Res<Universal> {
    preceded(
        literal(0x09_u8),
        alt((
            literal(0x01_u8).value(GeneralMidiMode::On),
            literal(0x02_u8).value(GeneralMidiMode::Off),
        )),
    )
    .map(Universal::GeneralMidi)
    .parse_next(input)
}

/// `04`, Device Control: Master Volume and Balance (M1 p.57), Master Fine and
/// Coarse Tuning (CA-025). Coarse Tuning's data is `00 msb`, the LSB always being
/// 0, so only the MSB is read.
fn device_control(input: &mut &[u8]) -> Res<Universal> {
    preceded(
        literal(0x04_u8),
        alt((
            preceded(literal(0x01_u8), u14).map(Universal::MasterVolume),
            preceded(literal(0x02_u8), u14).map(Universal::MasterBalance),
            preceded(literal(0x03_u8), u14).map(Universal::MasterFineTuning),
            preceded(literal(0x04_u8), preceded(u7, u7)).map(Universal::MasterCoarseTuning),
        )),
    )
    .parse_next(input)
}

/// `01 01 hr mn sc fr`, the MTC Full Message (M1 p.53 and T-9): the position's
/// four bytes, the hours byte carrying the rate code above the hours.
fn mtc_full(input: &mut &[u8]) -> Res<Universal> {
    preceded(
        (literal(0x01_u8), literal(0x01_u8)),
        (any, any, any, any)
            .verify_map(|(hr, mn, sc, fr)| MtcFullMessage::from_wire(hr, mn, sc, fr)),
    )
    .map(Universal::MtcFullMessage)
    .parse_next(input)
}

/// A 14-bit value sent most significant byte first, as MTS sends its
/// fractions and offsets (`0abcdefg 0hijklmn`, `40 00` is the middle).
fn u14_msb_first(input: &mut &[u8]) -> Res<U14> {
    (u7, u7)
        .map(|(msb, lsb)| U14::from_halves(msb, lsb))
        .parse_next(input)
}

/// Frequency data: `xx yy zz` (MTS, Frequency Data Format).
fn frequency(input: &mut &[u8]) -> Res<TuningFrequency> {
    (u7, u14_msb_first)
        .map(|(semitone, fraction)| TuningFrequency::from_parts(semitone, fraction))
        .parse_next(input)
}

fn note(input: &mut &[u8]) -> Res<NoteNumber> {
    any.verify_map(NoteNumber::new).parse_next(input)
}

fn name(input: &mut &[u8]) -> Res<TuningName> {
    array::<_, 16, _>(u7).map(TuningName::new).parse_next(input)
}

fn array<'i, T: Copy + Default, const N: usize, P>(
    mut item: P,
) -> impl FnMut(&mut &'i [u8]) -> Res<[T; N]>
where
    P: Parser<&'i [u8], T, winnow::error::ErrMode<ContextError>>,
{
    move |input: &mut &'i [u8]| {
        let mut out = [T::default(); N];
        for slot in &mut out {
            *slot = item.parse_next(input)?;
        }
        Ok(out)
    }
}

/// 128 frequency words, key 0 first.
fn key_table(input: &mut &[u8]) -> Res<KeyTable> {
    let mut words = [TuningFrequency::NO_CHANGE; 128];
    for word in &mut words {
        *word = frequency.parse_next(input)?;
    }
    Ok(KeyTable::new(words))
}

/// `ll` and then `ll` changes (`kk xx yy zz`): the count is the message's own
/// length (8 + (ll x 4) bytes in all), so a
/// message whose count disagrees with its bytes does not match.
fn key_changes(input: &mut &[u8]) -> Res<KeyChanges> {
    let count = u7.parse_next(input)?.get();
    if usize::from(count) > MAX_KEY_CHANGES {
        return fail.parse_next(input);
    }
    let mut changes = KeyChanges::new();
    for _ in 0..count {
        let (key, frequency) = (note, frequency).parse_next(input)?;
        changes.push(KeyChange { key, frequency });
    }
    Ok(changes)
}

/// The three channel bytes of Scale/Octave Tuning, `ff gg hh`: channels 15 and
/// 16 in the low two bits of `ff`, 8 to 14 in `gg` and 1 to 7 in `hh`. The
/// five other bits of `ff` are reserved and must be 0, so a message with one set does not match.
fn channel_mask(input: &mut &[u8]) -> Res<ChannelMask> {
    (any.verify(|&f| f <= 0b11), u7, u7)
        .map(|(f, g, h)| {
            ChannelMask::from_bits(
                u16::from(f) << 14 | u16::from(g.get()) << 7 | u16::from(h.get()),
            )
        })
        .parse_next(input)
}

fn octave_offsets(form: u8) -> impl FnMut(&mut &[u8]) -> Res<OctaveOffsets> {
    move |input: &mut &[u8]| {
        if form == 0x08 {
            array::<_, 12, _>(u7)
                .map(OctaveOffsets::Cents)
                .parse_next(input)
        } else {
            array::<_, 12, _>(u14_msb_first)
                .map(OctaveOffsets::Fine)
                .parse_next(input)
        }
    }
}

/// What a tuning message read, and the checksum byte it carried if the spec
/// gives the message one to check.
pub(super) struct Tuned {
    pub(super) message: TuningMessage,
    pub(super) checksum: Option<U7>,
}

/// `08`, the MIDI Tuning Standard, as `timing` carries it.
fn tuning(timing: Timing) -> impl FnMut(&mut &[u8]) -> Res<Tuned> {
    move |input: &mut &[u8]| {
        literal(0x08_u8).parse_next(input)?;
        let sub = any.parse_next(input)?;
        let plain = |message| Tuned {
            message,
            checksum: None,
        };
        match (timing, sub) {
            (Timing::RealTime, 0x02) => {
                let program = u7.parse_next(input)?;
                let changes = key_changes.parse_next(input)?;
                Ok(plain(TuningMessage::RealTimeNoteChange {
                    bank: None,
                    program,
                    changes,
                }))
            }
            (_, 0x07) => {
                let (bank, program) = (u7, u7).parse_next(input)?;
                let changes = key_changes.parse_next(input)?;
                Ok(plain(match timing {
                    Timing::RealTime => TuningMessage::RealTimeNoteChange {
                        bank: Some(bank),
                        program,
                        changes,
                    },
                    Timing::NonRealTime => TuningMessage::NoteChange {
                        bank,
                        program,
                        changes,
                    },
                }))
            }
            (_, 0x08 | 0x09) => {
                let channels = channel_mask.parse_next(input)?;
                let offsets = octave_offsets(sub).parse_next(input)?;
                Ok(plain(TuningMessage::OctaveTuning {
                    timing,
                    channels,
                    offsets,
                }))
            }
            (Timing::NonRealTime, 0x00) => {
                let program = u7.parse_next(input)?;
                Ok(plain(TuningMessage::DumpRequest {
                    bank: None,
                    program,
                }))
            }
            (Timing::NonRealTime, 0x03) => {
                let (bank, program) = (u7, u7).parse_next(input)?;
                Ok(plain(TuningMessage::DumpRequest {
                    bank: Some(bank),
                    program,
                }))
            }
            // The spec lets receivers ignore the checksum of that message: the
            // original Bulk Tuning Dump carries none that is checked.
            (Timing::NonRealTime, 0x01) => {
                let program = u7.parse_next(input)?;
                let (name, table) = (name, key_table).parse_next(input)?;
                u7.parse_next(input)?;
                Ok(plain(TuningMessage::KeyDump {
                    bank: None,
                    program,
                    name,
                    table,
                }))
            }
            (Timing::NonRealTime, 0x04) => {
                let (bank, program) = (u7, u7).parse_next(input)?;
                let (name, table) = (name, key_table).parse_next(input)?;
                let checksum = u7.parse_next(input)?;
                Ok(Tuned {
                    message: TuningMessage::KeyDump {
                        bank: Some(bank),
                        program,
                        name,
                        table,
                    },
                    checksum: Some(checksum),
                })
            }
            (Timing::NonRealTime, 0x05 | 0x06) => {
                let (bank, program) = (u7, u7).parse_next(input)?;
                let name = name.parse_next(input)?;
                let offsets = octave_offsets(sub + 3).parse_next(input)?;
                let checksum = u7.parse_next(input)?;
                Ok(Tuned {
                    message: TuningMessage::OctaveDump {
                        bank,
                        program,
                        name,
                        offsets,
                    },
                    checksum: Some(checksum),
                })
            }
            _ => fail.parse_next(input),
        }
    }
}

/// `06` and its commands, or `07` and its responses from `to`, the rest of the
/// message (RP-013 p.2).
fn machine_control(to: Recipient) -> impl FnMut(&mut &[u8]) -> Res<UniversalRead> {
    move |input: &mut &[u8]| {
        let sub_id = u7.parse_next(input)?.get();
        let rest = take_rest(input);
        match (sub_id, to) {
            (0x06, _) => machine_control::commands(rest)
                .map(|commands| {
                    UniversalRead::Universal(Addressed {
                        to,
                        message: Universal::MachineControl(commands),
                    })
                })
                .or_else(|| {
                    machine_control::truncated_segment(rest)
                        .map(|before| UniversalRead::MachineControlTruncated { to, before })
                }),
            (0x07, Recipient::Device(from)) => machine_control::fields(rest)
                .map(|fields| UniversalRead::MachineControlResponse { from, fields }),
            _ => None,
        }
        .ok_or_else(|| winnow::error::ErrMode::Backtrack(ContextError::new()))
    }
}

fn take_rest<'a>(input: &mut &'a [u8]) -> &'a [u8] {
    core::mem::take(input)
}

/// The addressed form: what it is, and the checksum byte of a dump that has one
/// to check.
fn addressed(input: &mut &[u8]) -> Res<(UniversalRead, Option<U7>)> {
    let class = any.parse_next(input)?;
    let to = device.parse_next(input)?;
    let sent = |message| UniversalRead::Universal(Addressed { to, message });
    let plain = |message| (sent(message), None);
    let tuned = |t: Tuned| (sent(Universal::Tuning(t.message)), t.checksum);
    let read = match class {
        UNIVERSAL_NON_REAL_TIME => alt((
            alt((general_information, general_midi)).map(plain),
            tuning(Timing::NonRealTime).map(tuned),
        ))
        .parse_next(input)?,
        UNIVERSAL_REAL_TIME => alt((
            mtc_full.map(plain),
            machine_control(to).map(|read| (read, None)),
            device_control.map(plain),
            tuning(Timing::RealTime).map(tuned),
        ))
        .parse_next(input)?,
        _ => return fail.parse_next(input),
    };
    eof.parse_next(input)?;
    Ok(read)
}

/// The payload of a Universal SysEx message (between `F0` and `F7`) as what it
/// is: a message and who it names, a Machine Control message only a receiver
/// makes, a dump that fails its checksum, or `Unread`.
///
/// MTS's checksum: the XOR of the bytes of the message, leaving out the F0, F7
/// and the checksum field, masked with 7F.
pub(super) fn parse(payload: &[u8]) -> UniversalRead {
    let mut input = payload;
    let Ok((read, checksum)) = addressed.parse_next(&mut input) else {
        return UniversalRead::Unread;
    };
    // Only a tuning dump has a checksum, and it is an `Addressed` message.
    if let (Some(found), UniversalRead::Universal(Addressed { to, .. }), Some((_, body))) =
        (checksum, read, payload.split_last())
    {
        let expected = U7::masked(body.iter().fold(0, |acc, b| acc ^ b));
        if expected != found {
            return UniversalRead::BadChecksum(BadChecksum {
                to,
                expected,
                found,
            });
        }
    }
    read
}
