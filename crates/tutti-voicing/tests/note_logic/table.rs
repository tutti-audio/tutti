//! `VoiceTable`: the allocator's commands carried out on preallocated slots.

use std::num::NonZeroUsize;

use assert_no_alloc::assert_no_alloc;
use tutti_ump::{Attribute, CCNumber, ControlValue, MidiChannel, NoteNumber, NoteVelocity, Voice2};
use tutti_units::{Amplitude, Semitones};
use tutti_voicing::{Glide, NoteStart, NoteVoice, Order, Steal, VoiceConfig, VoiceTable};

use super::support::{One, note_on};

/// A voice that records what the table told it; its level is its key's
/// fraction of 127, so the quietest voice is the lowest key.
#[derive(Debug, Clone)]
struct Probe {
    key: u8,
    starts: u32,
    released: bool,
}

impl NoteVoice for Probe {
    type Context = ();

    fn start(&mut self, note: &NoteStart, _: &()) {
        self.key = note.key.get();
        self.starts += 1;
        self.released = false;
    }

    fn retarget(&mut self, key: NoteNumber, _: Semitones, _: NoteVelocity, _: Option<Glide>) {
        self.key = key.get();
    }

    fn tune(&mut self, _: Semitones) {}

    fn release(&mut self, _: NoteVelocity) {
        self.released = true;
    }

    fn damp(&mut self) {
        self.released = true;
    }

    fn level(&self) -> Amplitude {
        Amplitude::new(f64::from(self.key) / 127.0)
    }
}

fn table(polyphony: usize, steal: Steal) -> VoiceTable<Probe> {
    let rules = VoiceConfig::poly(NonZeroUsize::new(polyphony).unwrap()).with_steal(steal);
    VoiceTable::new(rules, || Probe {
        key: 0,
        starts: 0,
        released: false,
    })
}

fn send(table: &mut VoiceTable<Probe>, message: Voice2) {
    table.step(MidiChannel::of(0), message, &One, &());
}

fn note_off(key: u8) -> Voice2 {
    Voice2::NoteOff {
        note: NoteNumber::of(key),
        velocity: NoteVelocity::CENTER,
        attribute: Attribute::None,
    }
}

fn keys(table: &VoiceTable<Probe>) -> Vec<u8> {
    table.iter().map(|v| v.key).collect()
}

#[test]
fn a_note_on_starts_a_voice_in_a_free_slot() {
    let mut table = table(2, Steal::Never);
    assert!(table.is_empty());
    send(&mut table, note_on(60));
    send(&mut table, note_on(64));
    assert_eq!(keys(&table), [60, 64]);
    assert!(table.iter().all(|v| v.starts == 1 && !v.released));
}

#[test]
fn a_full_table_that_never_steals_drops_the_note() {
    let mut table = table(1, Steal::Never);
    send(&mut table, note_on(60));
    send(&mut table, note_on(64));
    assert_eq!(keys(&table), [60]);
}

#[test]
fn the_oldest_voice_gives_its_slot_to_the_new_note() {
    let mut table = table(2, Steal::By(Order::Oldest));
    send(&mut table, note_on(60));
    send(&mut table, note_on(64));
    send(&mut table, note_on(67));
    // Started order, the stolen slot reused: 60's voice now plays 67.
    assert_eq!(keys(&table), [64, 67]);
    assert_eq!(table.iter().map(|v| v.starts).sum::<u32>(), 3);
}

#[test]
fn the_quietest_voice_by_level_gives_way() {
    let mut table = table(3, Steal::By(Order::Quietest));
    for key in [72, 48, 60] {
        send(&mut table, note_on(key));
    }
    send(&mut table, note_on(65));
    assert_eq!(keys(&table), [72, 60, 65]);
}

#[test]
fn a_note_off_releases_and_all_sound_off_cuts() {
    let mut table = table(2, Steal::Never);
    send(&mut table, note_on(60));
    send(&mut table, note_on(64));
    send(&mut table, note_off(60));
    assert_eq!(
        table.iter().map(|v| v.released).collect::<Vec<_>>(),
        [true, false]
    );
    send(
        &mut table,
        Voice2::ControlChange {
            controller: CCNumber::ALL_SOUND_OFF,
            value: ControlValue::MIN,
        },
    );
    assert!(table.is_empty());
}

#[test]
fn a_command_for_an_ended_voice_is_ignored_and_its_slot_is_free() {
    let mut table = table(1, Steal::Never);
    send(&mut table, note_on(60));
    table.retain(|_, _| false);
    assert!(table.is_empty());
    send(&mut table, note_off(60));
    assert!(table.is_empty());
    send(&mut table, note_on(62));
    assert_eq!(keys(&table), [62]);
}

#[test]
fn retain_keeps_the_voices_it_accepts_in_order() {
    let mut table = table(3, Steal::Never);
    for key in [60, 62, 64] {
        send(&mut table, note_on(key));
    }
    table.retain(|v, _| v.key != 62);
    assert_eq!(keys(&table), [60, 64]);
}

#[test]
fn stepping_allocates_nothing_after_construction() {
    let mut table = table(2, Steal::By(Order::Oldest));
    assert_no_alloc(|| {
        for key in [60, 62, 64, 66] {
            send(&mut table, note_on(key));
        }
        send(&mut table, note_off(64));
        table.retain(|v, _| !v.released);
        table.release_all();
        table.clear();
    });
}
