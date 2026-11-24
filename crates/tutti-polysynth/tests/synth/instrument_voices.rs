//! The instrument node and its four voices, heard at the output: each sounds at its key's
//! frequency, a released note falls silent, velocity scales the level, and the voice table
//! follows its `VoiceConfig`.

use std::f64::consts::TAU;

use tutti_fixtures::midi;
use tutti_polysynth::{InstrumentVoice, NoteInstrument};
use tutti_ump::MidiChannel;
use tutti_units::{SampleRate, Seconds};
use tutti_voicing::{Order, Steal, VoiceConfig};

use crate::support::instruments::{each_voice, hand, poly};
use crate::support::{BLOCK, Hand};

const RATE: SampleRate = SampleRate::SR_48K;
const CH: MidiChannel = MidiChannel::FIRST;
const FULL: u16 = u16::MAX;

type Played<V> = Hand<NoteInstrument<V>>;

/// The left output over `blocks` blocks of the hand.
fn render<V: InstrumentVoice<Float = f32>>(hand: &mut Played<V>, blocks: usize) -> Vec<f32> {
    (0..blocks)
        .flat_map(|_| hand.block(BLOCK).0.to_vec())
        .collect()
}

/// The frames of `s` seconds.
fn frames(s: f64) -> usize {
    Seconds::new(s).to_samples_ceil(RATE).get()
}

/// The blocks of `s` seconds.
fn seconds(s: f64) -> usize {
    frames(s) / BLOCK
}

/// The frequency within 3 % of `near` whose Hann-windowed Goertzel power is greatest, found in
/// steps of 0.04 %.
fn peak_near(signal: &[f32], near: f64) -> f64 {
    let n = signal.len() as f64;
    let power = |f: f64| {
        let w = TAU * f / RATE.get();
        let (mut re, mut im) = (0.0, 0.0);
        for (i, &x) in signal.iter().enumerate() {
            let hann = 0.5 - 0.5 * (TAU * i as f64 / n).cos();
            let (s, c) = (w * i as f64).sin_cos();
            re += f64::from(x) * hann * c;
            im += f64::from(x) * hann * s;
        }
        re * re + im * im
    };
    (-75..=75)
        .map(|step| near * (1.0 + f64::from(step) * 4.0e-4))
        .max_by(|a, b| power(*a).total_cmp(&power(*b)))
        .unwrap()
}

fn peak(signal: &[f32]) -> f64 {
    f64::from(tutti_fixtures::signal::peak(signal))
}

#[test]
fn each_voice_sounds_at_its_keys_frequency_within_half_a_percent() {
    fn check<V: InstrumentVoice<Float = f32>>(name: &str, config: V::Config) {
        for key in [45_u8, 60, 72] {
            let mut played: Played<V> = hand(config.clone(), poly(4));
            played.queue_midi(&[midi::note_on(CH, key, FULL)]);
            let heard = render(&mut played, seconds(0.4));
            // After the strike's burst, a quarter second of the ring.
            let window = &heard[frames(0.1)..frames(0.35)];
            let want = 440.0 * 2.0_f64.powf((f64::from(key) - 69.0) / 12.0);
            let got = peak_near(window, want);
            assert!(
                (got / want - 1.0).abs() < 0.005,
                "{name}: key {key} sounds at {got} Hz, not {want} Hz"
            );
        }
    }
    each_voice!(check);
}

#[test]
fn a_released_note_falls_off_and_is_silent_once_its_release_ends() {
    fn check<V: InstrumentVoice<Float = f32>>(name: &str, config: V::Config) {
        let mut played: Played<V> = hand(config, poly(4));
        played.queue_midi(&[midi::note_on(CH, 60, FULL)]);
        render(&mut played, seconds(0.3));
        played.queue_midi(&[midi::note_off(CH, 60, 0)]);
        let tail = render(&mut played, seconds(0.5));
        let window = |from: f64, to: f64| peak(&tail[frames(from)..frames(to)]);
        assert!(
            window(0.0, 0.05) > window(0.1, 0.15) && window(0.1, 0.15) > window(0.2, 0.25),
            "{name}: a release falls"
        );
        // The 0.25 s release has ended: the voice is free and the output exact silence.
        assert_eq!(played.sounding(), 0, "{name}: the voice ended");
        assert_eq!(peak(&tail[frames(0.3)..]), 0.0, "{name}: silence");
    }
    each_voice!(check);
}

#[test]
fn velocity_scales_the_level_in_proportion() {
    fn check<V: InstrumentVoice<Float = f32>>(name: &str, config: V::Config) {
        let level = |velocity: u16| {
            let mut played: Played<V> = hand(config.clone(), poly(4));
            played.queue_midi(&[midi::note_on(CH, 60, velocity)]);
            peak(&render(&mut played, seconds(0.1)))
        };
        let (soft, medium, hard) = (level(FULL / 4), level(FULL / 2), level(FULL));
        assert!(
            soft < medium && medium < hard,
            "{name}: {soft} {medium} {hard}"
        );
        // A strike is linear in its velocity: a quarter of the velocity is a quarter of the level.
        assert!((soft / hard - 0.25).abs() < 1e-3, "{name}: {}", soft / hard);
    }
    each_voice!(check);
}

#[test]
fn a_full_table_follows_its_steal_rule() {
    fn check<V: InstrumentVoice<Float = f32>>(name: &str, config: V::Config) {
        // Three keys fill the table, a fourth finds it full; then the keys 60 and 65 go up and
        // the voices left after each are counted once their releases have ended.
        let after_releases = |rules: VoiceConfig| {
            let mut played: Played<V> = hand(config.clone(), rules);
            for key in [60, 62, 64, 65] {
                played.queue_midi(&[midi::note_on(CH, key, FULL)]);
                played.block(BLOCK);
            }
            let mut left = [0; 2];
            for (count, key) in left.iter_mut().zip([60, 65]) {
                played.queue_midi(&[midi::note_off(CH, key, 0)]);
                render(&mut played, seconds(0.4));
                *count = played.sounding();
            }
            left
        };
        let steal = |order| VoiceConfig {
            steal: Steal::By(order),
            ..poly(3)
        };
        // Oldest: 60 was stolen, so its note off frees nothing. Never: 65 was dropped. Newest:
        // 64 was stolen and 65 sounds.
        assert_eq!(
            after_releases(steal(Order::Oldest)),
            [3, 2],
            "{name}: oldest"
        );
        let never = VoiceConfig {
            steal: Steal::Never,
            ..poly(3)
        };
        assert_eq!(after_releases(never), [2, 2], "{name}: never");
        assert_eq!(
            after_releases(steal(Order::Newest)),
            [2, 1],
            "{name}: newest"
        );
    }
    each_voice!(check);
}
