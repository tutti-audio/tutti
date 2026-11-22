//! The voices an instrument sounds, in the slots the note logic decides.
//!
//! [`VoiceTable`] owns a [`VoiceAllocator`] and the voices it commands. A message
//! goes in; the allocator's [`VoiceCommand`]s are carried out, in order, on
//! the slots, which a [`NoteVoice`] fills. The table is sized at construction
//! to the polyphony and never allocates afterwards.

use alloc::vec::Vec;

use tutti_midi_protocol::Heard;
use tutti_ump::{MidiChannel, NoteNumber, NoteVelocity, Voice2};
use tutti_units::{Amplitude, Semitones};

use crate::{
    Glide, NoteControl, Patch, VoiceAllocator, VoiceCommand, VoiceConfig, VoiceId, VoiceSpec,
};

/// What a voice is told when a note starts it: [`VoiceCommand::Start`] without
/// the slot bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteStart {
    /// Its channel.
    pub channel: MidiChannel,
    /// Its key.
    pub key: NoteNumber,
    /// How far the key sounds from its equal-tempered pitch.
    pub tune: Semitones,
    /// The strike's velocity, at least 1.
    pub velocity: NoteVelocity,
    /// What the patch said.
    pub spec: VoiceSpec,
    /// The glide it starts with, if any.
    pub glide: Option<Glide>,
}

/// A voice a note plays: what [`VoiceTable`] asks of the voices it holds.
///
/// A voice is built once, idle, and reused: `start` makes it sound a note
/// again. No method may allocate on the audio thread.
pub trait NoteVoice {
    /// What `start` reads besides the note (a SoundFont for a sampler's
    /// voices, `()` for none).
    type Context: ?Sized;

    /// Begins `note`, from the top of its envelope.
    fn start(&mut self, note: &NoteStart, context: &Self::Context);

    /// Moves to a new key without a new attack (legato, or Portamento Control).
    fn retarget(
        &mut self,
        key: NoteNumber,
        tune: Semitones,
        velocity: NoteVelocity,
        glide: Option<Glide>,
    );

    /// Moves the key's tuning off equal temperament, with no new attack.
    fn tune(&mut self, tune: Semitones);

    /// Lets the note go into its release.
    fn release(&mut self, velocity: NoteVelocity);

    /// Lets the note go into a short release of the voice's own choosing,
    /// whatever holds it.
    fn damp(&mut self);

    /// A per-note controller took a value. A voice without them ignores it.
    fn control(&mut self, _control: NoteControl) {}

    /// The per-note controllers return to their defaults.
    fn reset_controls(&mut self) {}

    /// The envelope level now: what the allocator's quietest steal asks.
    fn level(&self) -> Amplitude;
}

/// A sounding voice and the id the allocator knows it by.
#[derive(Debug, Clone)]
struct Slot<V> {
    id: VoiceId,
    voice: V,
}

/// The voices of an instrument and the [`VoiceAllocator`] that commands them.
///
/// `active` is in id order (the order the voices started, which the
/// allocator's ids follow), `free` holds the idle rest. Both are sized to the
/// polyphony at construction, so nothing here allocates afterwards.
#[derive(Debug)]
pub struct VoiceTable<V: NoteVoice> {
    allocator: VoiceAllocator,
    polyphony: usize,
    active: Vec<Slot<V>>,
    free: Vec<V>,
}

impl<V: NoteVoice + Clone> Clone for VoiceTable<V> {
    /// A copy with its lists as roomy as the original's: a derived clone would
    /// shrink them to their length, and a list that grows is an allocation on
    /// the audio thread.
    fn clone(&self) -> Self {
        let mut copy = Self {
            allocator: self.allocator.clone(),
            polyphony: self.polyphony,
            active: Vec::with_capacity(self.polyphony),
            free: Vec::with_capacity(self.polyphony),
        };
        copy.active.extend(self.active.iter().cloned());
        copy.free.extend(self.free.iter().cloned());
        copy
    }
}

impl<V: NoteVoice> VoiceTable<V> {
    /// A table of `rules.polyphony` idle voices, each made by `voice`.
    pub fn new(rules: VoiceConfig, mut voice: impl FnMut() -> V) -> Self {
        let polyphony = rules.polyphony.get();
        let mut free = Vec::with_capacity(polyphony);
        free.extend((0..polyphony).map(|_| voice()));
        Self {
            allocator: VoiceAllocator::new(rules),
            polyphony,
            active: Vec::with_capacity(polyphony),
            free,
        }
    }

    /// The note logic, for the channel state it holds.
    #[must_use]
    pub fn allocator(&self) -> &VoiceAllocator {
        &self.allocator
    }

    /// Forgets every note, pedal and held key (see
    /// [`VoiceAllocator::forget_notes`]), keeping the channels' controllers.
    /// The sounding voices are the caller's to end: [`clear`](Self::clear) or
    /// [`release_all`](Self::release_all).
    pub fn forget_notes(&mut self) {
        self.allocator.forget_notes();
    }

    /// A table of idle voices, each made by `voice`, that keeps this one's
    /// channel state (controllers, wheels, tunings) and none of its notes.
    /// The allocator is never moved under sounding voices: the copy has none.
    #[must_use]
    pub fn fork(&self, mut voice: impl FnMut() -> V) -> Self {
        let mut copy = Self {
            allocator: self.allocator.clone(),
            polyphony: self.polyphony,
            active: Vec::with_capacity(self.polyphony),
            free: Vec::with_capacity(self.polyphony),
        };
        copy.free.extend((0..self.polyphony).map(|_| voice()));
        copy.allocator.forget_notes();
        copy
    }

    /// Hands one channel voice message to the allocator and carries out what
    /// it says.
    pub fn step<P: Patch + ?Sized>(
        &mut self,
        channel: MidiChannel,
        message: Voice2,
        patch: &P,
        context: &V::Context,
    ) {
        let active = &self.active;
        let commands = self
            .allocator
            .step(channel, message, patch, |id| level(active, id));
        execute(&mut self.active, &mut self.free, commands, context);
    }

    /// Hands a Universal System Exclusive message to the allocator and carries
    /// out what it says.
    pub fn universal(&mut self, heard: Heard, context: &V::Context) {
        let commands = self.allocator.universal(heard);
        execute(&mut self.active, &mut self.free, commands, context);
    }

    /// Whether no voice is sounding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// The sounding voices, in the order they started.
    pub fn iter(&self) -> impl Iterator<Item = &V> {
        self.active.iter().map(|s| &s.voice)
    }

    /// Calls `keep` on each sounding voice, with the allocator's state to
    /// render it by, and returns the ones it answers `false` for to the free
    /// list, the others in order.
    pub fn retain(&mut self, mut keep: impl FnMut(&mut V, &VoiceAllocator) -> bool) {
        let mut i = 0;
        while i < self.active.len() {
            if keep(&mut self.active[i].voice, &self.allocator) {
                i += 1;
            } else {
                let slot = self.active.remove(i);
                self.free.push(slot.voice);
            }
        }
    }

    /// Lets every voice go into its release, whatever holds it.
    pub fn release_all(&mut self) {
        for slot in &mut self.active {
            slot.voice.release(NoteVelocity::CENTER);
        }
    }

    /// Silences every voice at once.
    pub fn clear(&mut self) {
        while let Some(slot) = self.active.pop() {
            self.free.push(slot.voice);
        }
    }
}

/// The level of voice `id`, `None` once it has ended. `active` is in id order,
/// so it is a binary search.
fn level<V: NoteVoice>(active: &[Slot<V>], id: VoiceId) -> Option<Amplitude> {
    let i = active.binary_search_by_key(&id, |s| s.id).ok()?;
    Some(active[i].voice.level())
}

fn slot<V>(active: &mut [Slot<V>], id: VoiceId) -> Option<&mut V> {
    let i = active.binary_search_by_key(&id, |s| s.id).ok()?;
    Some(&mut active[i].voice)
}

/// Takes the voice out of play, keeping the others in order.
fn take<V>(active: &mut Vec<Slot<V>>, id: VoiceId) -> Option<V> {
    let i = active.binary_search_by_key(&id, |s| s.id).ok()?;
    Some(active.remove(i).voice)
}

/// Carries out what the allocator said, in order. A command that names a voice
/// that has ended is ignored, as the allocator's contract says.
fn execute<V: NoteVoice>(
    active: &mut Vec<Slot<V>>,
    free: &mut Vec<V>,
    commands: &[VoiceCommand],
    context: &V::Context,
) {
    for command in commands {
        match *command {
            VoiceCommand::Start {
                id,
                channel,
                key,
                tune,
                velocity,
                spec,
                steal,
                glide,
            } => {
                let voice = steal.and_then(|v| take(active, v)).or_else(|| free.pop());
                let Some(mut voice) = voice else { continue };
                voice.start(
                    &NoteStart {
                        channel,
                        key,
                        tune,
                        velocity,
                        spec,
                        glide,
                    },
                    context,
                );
                active.push(Slot { id, voice });
            }
            VoiceCommand::Retarget {
                id,
                key,
                tune,
                velocity,
                glide,
            } => {
                if let Some(voice) = slot(active, id) {
                    voice.retarget(key, tune, velocity, glide);
                }
            }
            VoiceCommand::Tune { id, tune } => {
                if let Some(voice) = slot(active, id) {
                    voice.tune(tune);
                }
            }
            VoiceCommand::Release { id, velocity } => {
                if let Some(voice) = slot(active, id) {
                    voice.release(velocity);
                }
            }
            VoiceCommand::Damp(id) => {
                if let Some(voice) = slot(active, id) {
                    voice.damp();
                }
            }
            VoiceCommand::Cut(id) => {
                if let Some(voice) = take(active, id) {
                    free.push(voice);
                }
            }
            VoiceCommand::Control { id, control } => {
                if let Some(voice) = slot(active, id) {
                    voice.control(control);
                }
            }
            VoiceCommand::ResetControls(id) => {
                if let Some(voice) = slot(active, id) {
                    voice.reset_controls();
                }
            }
            // A channel's bend is the allocator's: an instrument reads it back
            // each chunk (`VoiceAllocator::channel_pitch`), so there is
            // nothing to mirror in a voice.
            VoiceCommand::Retune { .. } => {}
        }
    }
}
