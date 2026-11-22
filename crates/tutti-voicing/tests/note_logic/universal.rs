//! The Universal System Exclusive messages the note logic reads: Master Volume
//! and Balance (M1 p.57), Master Fine and Coarse Tuning (CA-025) and General
//! MIDI System On and Off (M1 p.52, RP-003, RP-015, GM2 section 4).

use std::num::NonZeroUsize;

use crate::support::{note_on, step};
use tutti_midi_protocol::{Addressee, GeneralMidiMode, Heard, Universal};
use tutti_ump::{Attribute, CCNumber, MidiChannel, NoteNumber, NoteVelocity, Rpn, U7, U14, Voice2};
use tutti_ump::{BendValue, ControlValue};
use tutti_units::{Normalized, Semitones};
use tutti_voicing::{VoiceAllocator, VoiceCommand, VoiceConfig};

fn say(core: &mut VoiceAllocator, to: Addressee, message: Universal) -> Vec<VoiceCommand> {
    core.universal(Heard { to, message }).to_vec()
}

fn cc(n: u8, value7: u32) -> Voice2 {
    Voice2::ControlChange {
        controller: CCNumber::of(n),
        value: ControlValue::new(value7 << 25),
    }
}

/// The pitch of each channel a list of commands retunes.
fn retunes(commands: &[VoiceCommand]) -> Vec<(u8, f64)> {
    commands
        .iter()
        .filter_map(|c| match c {
            VoiceCommand::Retune { channel, bend } => Some((channel.get(), bend.get())),
            _ => None,
        })
        .collect()
}

const FINE: fn(u16) -> Universal = |v| Universal::MasterFineTuning(U14::of(v));
const COARSE: fn(u8) -> Universal = |v| Universal::MasterCoarseTuning(U7::of(v));

/// CA-025: fine tuning is 100/8192 cents a step about `00 40`: `00 00` is -100
/// cents, `00 40` is A440 and `7F 7F` is +8191 steps; every channel moves.
#[test]
fn master_fine_tuning_is_a_hundred_over_8192_cents_a_step() {
    let mut core = VoiceAllocator::new(VoiceConfig::poly(NonZeroUsize::new(4).unwrap()));
    let all = say(&mut core, Addressee::All, FINE(0));
    let r = retunes(&all);
    assert_eq!(r.len(), 16, "every channel");
    assert!(r.iter().all(|&(_, b)| b == -1.0), "-100 cents");
    let r = retunes(&say(&mut core, Addressee::All, FINE(0x3FFF)));
    assert_eq!(r[0].1, 8191.0 / 8192.0);
    let r = retunes(&say(&mut core, Addressee::All, FINE(0x2000)));
    assert!(r.iter().all(|&(_, b)| b == 0.0), "A440");
}

/// CA-025: coarse tuning is a semitone a step about `00 40`; `00 00` is -64
/// semitones and `00 7F` +63.
#[test]
fn master_coarse_tuning_is_a_semitone_a_step() {
    let mut core = VoiceAllocator::new(VoiceConfig::poly(NonZeroUsize::new(4).unwrap()));
    for (msb, semitones) in [(0x00, -64.0), (0x40, 0.0), (0x4C, 12.0), (0x7F, 63.0)] {
        let r = retunes(&say(&mut core, Addressee::All, COARSE(msb)));
        assert!(r.iter().all(|&(_, b)| b == semitones), "{msb:#x}: {r:?}");
    }
}

/// CA-025: the total displacement is the sum of the displacement of Master Fine
/// Tuning and that of Fine Tuning using RPN, and likewise coarse; the channel's wheel adds as it always did.
#[test]
fn the_master_tuning_sums_with_the_channels_own_tuning_and_wheel() {
    let mut core = VoiceAllocator::new(VoiceConfig::poly(NonZeroUsize::new(4).unwrap()));
    // Channel Coarse Tuning (RPN 2) +3 on channel 0: the MSB is 64 + 3.
    step(
        &mut core,
        0,
        Voice2::Registered {
            bank: Rpn::CHANNEL_COARSE_TUNING.bank(),
            index: Rpn::CHANNEL_COARSE_TUNING.index(),
            value: ControlValue::new(67 << 25),
        },
    );
    let r = retunes(&say(&mut core, Addressee::All, COARSE(0x42)));
    let on = |ch: u8| r.iter().find(|&&(c, _)| c == ch).unwrap().1;
    assert_eq!(on(0), 5.0, "+2 master and +3 channel");
    assert_eq!(on(1), 2.0);
    // A full wheel up on channel 1 spans the default 2 semitones.
    let bend = step(&mut core, 1, Voice2::PitchBend(BendValue::MAX));
    assert!((retunes(&bend)[0].1 - 4.0).abs() < 1e-3);
}

/// CA-025 and GM2 section 4.2: the percussion channel keeps its pitch, for a key
/// there is a different sound and not a different pitch.
#[test]
fn the_percussion_channel_is_not_retuned() {
    let mut core = VoiceAllocator::new(
        VoiceConfig::poly(NonZeroUsize::new(4).unwrap()).with_percussion(MidiChannel::of(9)),
    );
    let r = retunes(&say(&mut core, Addressee::All, COARSE(0x4C)));
    assert_eq!(r.len(), 15);
    assert!(r.iter().all(|&(c, b)| c != 9 && b == 12.0));
    let bend = step(&mut core, 9, Voice2::PitchBend(BendValue::MAX));
    assert!((retunes(&bend)[0].1 - 2.0).abs() < 1e-3, "the wheel alone");
}

fn gain(core: &VoiceAllocator) -> (f64, f64) {
    let g = core.master_gain();
    (g.left.get(), g.right.get())
}

/// GM2 section 4.1: the volume is proportional to the square of the value,
/// default 7F/7F, and `00 00` is off (M1 p.57).
#[test]
fn master_volume_is_the_square_of_the_value() {
    let mut core = VoiceAllocator::new(VoiceConfig::poly(NonZeroUsize::new(4).unwrap()));
    assert_eq!(gain(&core), (1.0, 1.0));
    let volume = |v| Universal::MasterVolume(U14::of(v));
    assert!(say(&mut core, Addressee::All, volume(0x2000)).is_empty());
    let (l, r) = gain(&core);
    let expected = (8192.0_f64 / 16383.0).powi(2);
    assert!((l - expected).abs() < 1e-15 && (r - expected).abs() < 1e-15);
    say(&mut core, Addressee::All, volume(0));
    assert_eq!(gain(&core), (0.0, 0.0));
}

/// M1 p.57: the volume received on the device's own ID and the one received on the
/// All Call ID are tracked apart and multiply, so each level is kept.
#[test]
fn the_own_and_all_call_volumes_are_kept_apart_and_multiply() {
    let mut core = VoiceAllocator::new(VoiceConfig::poly(NonZeroUsize::new(4).unwrap()));
    let volume = |v| Universal::MasterVolume(U14::of(v));
    say(&mut core, Addressee::Own, volume(0x2000));
    say(&mut core, Addressee::All, volume(0x2000));
    let quarter = (8192.0_f64 / 16383.0).powi(2);
    assert!((gain(&core).0 - quarter * quarter).abs() < 1e-15);
    // Bringing the all-call back up leaves the own level where it was.
    say(&mut core, Addressee::All, volume(0x3FFF));
    assert!((gain(&core).0 - quarter).abs() < 1e-15);
}

/// M1 p.57: balance `00 00` is hard left, `7F 7F` hard right, and the centre
/// (`40 00`) leaves both sides alone; the knob attenuates the side it turns
/// away from.
#[test]
fn master_balance_attenuates_the_side_it_turns_from() {
    let mut core = VoiceAllocator::new(VoiceConfig::poly(NonZeroUsize::new(4).unwrap()));
    let balance = |v| Universal::MasterBalance(U14::of(v));
    say(&mut core, Addressee::All, balance(0x2000));
    assert_eq!(gain(&core), (1.0, 1.0));
    say(&mut core, Addressee::All, balance(0));
    assert_eq!(gain(&core), (1.0, 0.0));
    say(&mut core, Addressee::All, balance(0x3FFF));
    assert_eq!(gain(&core), (0.0, 1.0));
    say(&mut core, Addressee::All, balance(0x1000));
    assert_eq!(gain(&core), (1.0, 0.5));
    // Two knobs, both a quarter of the way left: the right side halves twice.
    say(&mut core, Addressee::Own, balance(0x1000));
    assert_eq!(gain(&core), (1.0, 0.25));
}

fn dirty(core: &mut VoiceAllocator, bend_range: u32) {
    step(core, 0, cc(7, 20));
    step(core, 0, cc(11, 50));
    step(core, 0, cc(1, 90));
    step(core, 0, cc(10, 0));
    step(core, 0, cc(64, 127));
    step(core, 0, Voice2::PitchBend(BendValue::MAX));
    step(
        core,
        0,
        Voice2::Registered {
            bank: Rpn::PITCH_BEND_SENSITIVITY.bank(),
            index: Rpn::PITCH_BEND_SENSITIVITY.index(),
            value: ControlValue::new(bend_range << 25),
        },
    );
}

/// GM2 section 4.9.2 and RP-015: General MIDI 1 On mutes every sounding note and
/// returns the device to its initial status, which includes Reset All
/// Controllers (modulation 0, expression 127, the pedals up, the wheel centred)
/// and the General MIDI defaults (RP-003: volume 100, a 2 semitone bend range;
/// GM2 section 3.3: pan centre).
#[test]
fn general_midi_on_mutes_and_returns_every_channel_to_its_defaults() {
    let mut core = VoiceAllocator::new(
        VoiceConfig::poly(NonZeroUsize::new(4).unwrap()).with_bend_range(Semitones::new(12.0)),
    );
    dirty(&mut core, 24);
    let started = step(&mut core, 0, note_on(60));
    let Some(VoiceCommand::Start { id, .. }) = started.first().copied() else {
        panic!("{started:?}");
    };
    step(&mut core, 5, note_on(62));
    let on = say(
        &mut core,
        Addressee::All,
        Universal::GeneralMidi(GeneralMidiMode::On),
    );
    assert_eq!(
        on.iter()
            .filter(|c| matches!(c, VoiceCommand::Cut(_)))
            .count(),
        2,
        "each sounding note is cut"
    );
    assert!(on.contains(&VoiceCommand::Cut(id)));
    // The wheel is centred and every channel is told.
    let r = retunes(&on);
    assert_eq!(r.len(), 16);
    assert!(r.iter().all(|&(_, b)| b == 0.0));
    let c = core.channel(MidiChannel::of(0));
    assert_eq!(c.modulation(), Normalized::of(0.0));
    assert_eq!(c.volume(), Normalized::clamped(100.0 * 128.0 / 16383.0));
    assert_eq!(c.expression(), Normalized::clamped(127.0 * 128.0 / 16383.0));
    assert_eq!(c.pan(), core.channel(MidiChannel::of(7)).pan());
    // The range is General MIDI's, not the 12 the instrument was built with
    // nor the 24 a message set: a full wheel is 2 semitones.
    let bend = step(&mut core, 0, Voice2::PitchBend(BendValue::MAX));
    assert!((retunes(&bend)[0].1 - 2.0).abs() < 1e-3);
    // The pedal is up: a key down and up releases at once.
    step(&mut core, 0, note_on(64));
    let off = step(
        &mut core,
        0,
        Voice2::NoteOff {
            note: NoteNumber::of(64),
            velocity: NoteVelocity::CENTER,
            attribute: Attribute::None,
        },
    );
    assert!(
        off.iter()
            .any(|c| matches!(c, VoiceCommand::Release { .. }))
    );
}

/// GM2 section 4.9.3: General MIDI Off returns a device to its initialized
/// mode: here the bend range the instrument's rules were built with.
#[test]
fn general_midi_off_returns_to_the_instruments_own_status() {
    let mut core = VoiceAllocator::new(
        VoiceConfig::poly(NonZeroUsize::new(4).unwrap()).with_bend_range(Semitones::new(12.0)),
    );
    dirty(&mut core, 24);
    say(
        &mut core,
        Addressee::All,
        Universal::GeneralMidi(GeneralMidiMode::Off),
    );
    let bend = step(&mut core, 0, Voice2::PitchBend(BendValue::MAX));
    assert!((retunes(&bend)[0].1 - 12.0).abs() < 1e-3);
}

/// General MIDI's initial status is what the all-call messages return to
/// their defaults; a device's own knob (the own-ID volume, M1 p.57) is not a
/// message and stays.
#[test]
fn general_midi_resets_the_master_but_not_the_own_knob() {
    let mut core = VoiceAllocator::new(VoiceConfig::poly(NonZeroUsize::new(4).unwrap()));
    say(&mut core, Addressee::All, COARSE(0x4C));
    say(
        &mut core,
        Addressee::All,
        Universal::MasterVolume(U14::of(0)),
    );
    say(
        &mut core,
        Addressee::Own,
        Universal::MasterVolume(U14::of(0x2000)),
    );
    let on = say(
        &mut core,
        Addressee::All,
        Universal::GeneralMidi(GeneralMidiMode::On),
    );
    assert!(retunes(&on).iter().all(|&(_, b)| b == 0.0), "A440 again");
    let quarter = (8192.0_f64 / 16383.0).powi(2);
    assert!((gain(&core).0 - quarter).abs() < 1e-15);
}

/// The Device Inquiry's messages are for whoever owns the wire: the note logic
/// takes nothing from them.
#[test]
fn the_identity_messages_change_nothing() {
    let mut core = VoiceAllocator::new(VoiceConfig::poly(NonZeroUsize::new(4).unwrap()));
    step(&mut core, 0, note_on(60));
    let before = (core.channels().to_vec(), gain(&core));
    assert!(say(&mut core, Addressee::All, Universal::IdentityRequest).is_empty());
    assert_eq!((core.channels().to_vec(), gain(&core)), before);
}
