//! Polyphonic voice allocator with stealing strategies, mono/legato modes,
//! and sustain/sostenuto pedal handling. RT-safe after construction.
//!
//! Voices are addressed by [`NoteId`] — the MIDI 2.0 per-note identity — not by
//! bare note number. Two notes that share a note number (same pitch on different
//! channels, or two same-pitch notes distinguished by per-note addressing on one
//! channel) therefore occupy independent voices. On the classic-MPE / MIDI-1 path
//! the id is `NoteId::from_channel_note(channel, note)`, so behaviour is unchanged;
//! native MIDI 2.0 per-note messages address the exact voice by the same id.

use std::num::NonZeroUsize;

use crate::note_id::PerNoteMap;
use tutti_midi_protocol::Heard;
use tutti_ump::Voice2;
use tutti_ump::{MidiChannel, NoteId, NoteNumber, NoteVelocity};
use tutti_units::{Amplitude, Semitones, Velocity};
use tutti_voicing::{
    ChannelState, Glide, MasterGain, Mode, NoteControl, Order, Patch, Steal, VoiceAllocator,
    VoiceCommand, VoiceConfig, VoiceSpec,
};

use tutti_ump::ControlValue;
pub(crate) use tutti_voicing::VoiceId;

/// How many notes may sound at once, and whether a new note restarts the
/// envelope.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq, Default)]
#[non_exhaustive]
pub enum VoiceMode {
    /// Polyphonic: notes sound simultaneously up to the polyphony, then the
    /// [`Steal`] decides. The default.
    #[default]
    Poly,
    /// Monophonic: one note at a time on a channel. A new note releases the
    /// sounding one and retriggers the envelope from attack. Only slot 0 is
    /// used, however large the polyphony is.
    Mono,
    /// Monophonic with legato: a new note while one is already sounding takes
    /// over the same voice without retriggering the envelope, so the note
    /// glides or steps in at the envelope's current level. The first note of a
    /// phrase (nothing sounding) triggers normally. This is what makes
    /// [`PortamentoMode::LegatoOnly`](crate::PortamentoMode::LegatoOnly)
    /// meaningful.
    Legato,
}

/// Where a slot is in its life. What holds an active voice open is the core's
/// to say (`SynthVoices::holds_of`), not copied here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum VoiceState {
    #[default]
    Idle,
    Active,
    Releasing,
}

#[derive(Debug, Clone)]
pub(crate) struct VoiceSlot {
    voice_id: VoiceId,
    /// MIDI 2.0 per-note identity for this voice (the allocation key).
    id: NoteId,
    note: NoteNumber,
    channel: MidiChannel,
    velocity: Velocity,
    envelope_level: Amplitude,
    state: VoiceState,
}

impl Default for VoiceSlot {
    fn default() -> Self {
        Self {
            voice_id: VoiceId::new(0),
            id: NoteId::default(),
            note: NoteNumber::default(),
            channel: MidiChannel::FIRST,
            velocity: Velocity::SILENT,
            envelope_level: Amplitude::SILENT,
            state: VoiceState::Idle,
        }
    }
}

impl VoiceSlot {
    #[inline]
    pub(crate) fn voice_id(&self) -> VoiceId {
        self.voice_id
    }

    #[inline]
    pub(crate) fn id(&self) -> NoteId {
        self.id
    }

    #[cfg(test)]
    pub(crate) fn note(&self) -> NoteNumber {
        self.note
    }

    #[cfg(test)]
    pub(crate) fn velocity(&self) -> Velocity {
        self.velocity
    }

    #[cfg(test)]
    pub(crate) fn channel(&self) -> MidiChannel {
        self.channel
    }

    #[inline]
    pub(crate) fn envelope_level(&self) -> Amplitude {
        self.envelope_level
    }

    #[inline]
    pub(crate) fn is_idle(&self) -> bool {
        self.state == VoiceState::Idle
    }

    #[cfg(test)]
    #[inline]
    pub(crate) fn is_releasing(&self) -> bool {
        self.state == VoiceState::Releasing
    }

    #[cfg(test)]
    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.state == VoiceState::Active
    }

    /// Starts a new voice in this slot, key held and nothing else.
    pub(crate) fn activate(
        &mut self,
        voice_id: VoiceId,
        id: NoteId,
        note: NoteNumber,
        channel: MidiChannel,
        velocity: Velocity,
    ) {
        self.voice_id = voice_id;
        self.id = id;
        self.note = note;
        self.channel = channel;
        self.velocity = velocity;
        self.envelope_level = Amplitude::SILENT;
        self.state = VoiceState::Active;
    }

    pub(crate) fn release(&mut self) {
        self.state = VoiceState::Releasing;
    }

    pub(crate) fn silence(&mut self) {
        self.state = VoiceState::Idle;
    }

    pub(crate) fn set_envelope_level(&mut self, level: Amplitude) {
        self.envelope_level = level;
    }

    /// Update identity, note and velocity for legato retrigger (the slot keeps
    /// sounding but now represents a new note, hence a new [`NoteId`]).
    pub(crate) fn update_note(&mut self, id: NoteId, note: NoteNumber, velocity: Velocity) {
        self.id = id;
        self.note = note;
        self.velocity = velocity;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SynthVoicesConfig {
    max_voices: NonZeroUsize,
    steal: Steal,
    mode: VoiceMode,
    bend_range: Semitones,
    per_note_bend_range: Semitones,
}

impl SynthVoicesConfig {
    /// 16 poly voices, the oldest stolen first, a whole-tone bend and a 48-semitone
    /// Per-Note Pitch Bend.
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            max_voices: NonZeroUsize::new(16).unwrap(),
            steal: Steal::By(Order::Oldest),
            mode: VoiceMode::Poly,
            bend_range: Semitones::new(2.0),
            per_note_bend_range: Semitones::new(48.0),
        }
    }

    /// How many notes may sound at once.
    #[must_use]
    pub(crate) const fn max_voices(mut self, voices: NonZeroUsize) -> Self {
        self.max_voices = voices;
        self
    }

    /// Which voice is taken when all are busy.
    #[must_use]
    pub(crate) const fn steal(mut self, steal: Steal) -> Self {
        self.steal = steal;
        self
    }

    /// Poly, mono or legato.
    #[must_use]
    pub(crate) const fn mode(mut self, mode: VoiceMode) -> Self {
        self.mode = mode;
        self
    }

    /// What a full pitch bend spans on a channel until RPN 0 says otherwise.
    #[must_use]
    pub(crate) const fn bend_range(mut self, range: Semitones) -> Self {
        self.bend_range = range;
        self
    }

    /// What a full Per-Note Pitch Bend spans on a channel until RPN 0/7 says
    /// otherwise.
    #[must_use]
    pub(crate) const fn per_note_bend_range(mut self, range: Semitones) -> Self {
        self.per_note_bend_range = range;
        self
    }
}

impl SynthVoicesConfig {
    /// This synth's [`VoiceConfig`] for the shared note logic.
    fn voice_config(&self) -> VoiceConfig {
        VoiceConfig::poly(self.max_voices)
            .with_mode(match self.mode {
                VoiceMode::Poly => Mode::Poly,
                VoiceMode::Mono => Mode::Mono,
                VoiceMode::Legato => Mode::Legato,
            })
            .with_steal(self.steal)
            .with_bend_range(self.bend_range)
            .with_per_note_bend_range(self.per_note_bend_range)
    }
}

/// Where a note landed. The synth acts on the gates; this is what the
/// allocator's tests read.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), expect(dead_code, reason = "read by the allocator's tests"))]
pub(crate) enum AllocationResult {
    Allocated { slot_index: usize },
    Stolen { slot_index: usize },
    LegatoRetrigger { slot_index: usize },
    Unavailable,
}

#[cfg_attr(not(test), expect(dead_code, reason = "read by the allocator's tests"))]
impl AllocationResult {
    /// The slot the note landed in, if it got one.
    pub(crate) fn slot_index(&self) -> Option<usize> {
        match *self {
            Self::Allocated { slot_index }
            | Self::Stolen { slot_index }
            | Self::LegatoRetrigger { slot_index } => Some(slot_index),
            Self::Unavailable => None,
        }
    }
}

/// A note is one voice here (its unison copies are lanes of that voice).
struct OneVoice;

impl Patch for OneVoice {
    fn voices(
        &self,
        _: MidiChannel,
        _: NoteNumber,
        _: NoteVelocity,
        spec: &mut dyn FnMut(VoiceSpec),
    ) {
        spec(VoiceSpec {
            group: None,
            scope: 0,
            tag: 0,
        });
    }
}

/// What a command asks the synth to do to a voice, after the slot table has
/// followed it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Gate {
    /// Let the voice at this slot go into its release.
    Off(usize),
    /// Silence the voice at this slot at once.
    Cut(usize),
    /// The channel's pitch moved: its notes sound this far from their keys.
    Retune(MidiChannel, Semitones),
    /// The voice at this slot sounds its key this far from equal temperament
    /// (a tuning message or a Tuning Program or Bank Select reached it).
    Tune(usize, Semitones),
    /// The voice at this slot takes a per-note controller's value.
    Control(usize, NoteControl),
    /// The voice at this slot returns its per-note controllers to defaults.
    ResetControls(usize),
    /// Start (or, `legato`, move) the voice at this slot to this note.
    Start {
        slot: usize,
        key: NoteNumber,
        channel: MidiChannel,
        /// How far the key sounds from its equal-tempered pitch.
        tune: Semitones,
        velocity: Velocity,
        legato: bool,
        /// The glide the stream asked for, if any.
        glide: Option<Glide>,
    },
}

/// The synth's half of the note logic: the slot table. The decisions are
/// `tutti_voicing::VoiceAllocator`'s; this carries out its commands on slots and keeps
/// what only the synth knows (the per-note identity, the envelope level).
#[derive(Debug)]
pub(crate) struct SynthVoices {
    config: SynthVoicesConfig,
    core: VoiceAllocator,
    slots: Vec<VoiceSlot>,
    /// [`NoteId`] → active slot index. Keyed by full per-note identity, so two
    /// notes sharing a note number map to distinct slots.
    id_to_slot: PerNoteMap<usize, 128>,
    commands: Vec<VoiceCommand>,
    gates: Vec<Gate>,
}

impl SynthVoices {
    /// The channel state the note logic holds, as messages in `group`
    /// (`tutti_voicing::VoiceAllocator::chase`).
    pub(crate) fn chase(&self, group: tutti_ump::MidiGroup, emit: impl FnMut(tutti_ump::Ump)) {
        self.core.chase(group, emit);
    }

    pub(crate) fn new(config: SynthVoicesConfig) -> Self {
        let voice_config = config.voice_config();
        let slots = (0..config.max_voices.get())
            .map(|_| VoiceSlot::default())
            .collect();
        Self {
            core: VoiceAllocator::new(voice_config),
            commands: Vec::with_capacity(2 * config.max_voices.get() + 64),
            gates: Vec::with_capacity(2 * config.max_voices.get() + 64),
            config,
            slots,
            id_to_slot: PerNoteMap::new(),
        }
    }

    /// Runs `message` through the core and carries out what it returns.
    /// `strike` is the note being started, if the message starts one.
    fn run(
        &mut self,
        channel: MidiChannel,
        message: &Voice2,
        strike: Option<(NoteId, Velocity)>,
    ) -> AllocationResult {
        // Only a note message asks about the voices (to forget the ones that
        // ended, to choose a victim, to return to a held key).
        let slots = &self.slots;
        let cmds = self.core.step(channel, *message, &OneVoice, |id| {
            slots
                .iter()
                .find(|s| !s.is_idle() && s.voice_id() == id)
                .map(|s| s.envelope_level())
        });
        self.commands.clear();
        self.commands.extend_from_slice(cmds);
        self.carry_out(strike)
    }

    /// Carries out the commands the core returned, which are in
    /// `self.commands`: the slot table follows them and each leaves its gate.
    fn carry_out(&mut self, strike: Option<(NoteId, Velocity)>) -> AllocationResult {
        let mut result = AllocationResult::Unavailable;
        for i in 0..self.commands.len() {
            match self.commands[i] {
                VoiceCommand::Start {
                    id,
                    channel,
                    key,
                    tune,
                    steal,
                    velocity: velocity16,
                    glide,
                    ..
                } => {
                    let (note_id, velocity) = Self::strike_of(strike, channel, key, velocity16);
                    let stolen = steal.and_then(|v| self.slot_of_voice(v));
                    let Some(slot_index) =
                        stolen.or_else(|| self.slots.iter().position(VoiceSlot::is_idle))
                    else {
                        continue;
                    };
                    if stolen.is_some() {
                        self.unmap(slot_index);
                    }
                    self.slots[slot_index].activate(id, note_id, key, channel, velocity);
                    self.id_to_slot.insert(note_id, slot_index);
                    self.gates.push(Gate::Start {
                        slot: slot_index,
                        key,
                        channel,
                        tune,
                        velocity,
                        legato: false,
                        glide,
                    });
                    result = if stolen.is_some() && self.config.mode == VoiceMode::Poly {
                        AllocationResult::Stolen { slot_index }
                    } else {
                        AllocationResult::Allocated { slot_index }
                    };
                }
                VoiceCommand::Retarget {
                    id,
                    key,
                    tune,
                    velocity: velocity16,
                    glide,
                } => {
                    if let Some(slot_index) = self.slot_of_voice(id) {
                        let channel = self.slots[slot_index].channel;
                        let (note_id, velocity) = Self::strike_of(strike, channel, key, velocity16);
                        self.unmap(slot_index);
                        self.slots[slot_index].update_note(note_id, key, velocity);
                        self.id_to_slot.insert(note_id, slot_index);
                        self.gates.push(Gate::Start {
                            slot: slot_index,
                            key,
                            channel,
                            tune,
                            velocity,
                            legato: true,
                            glide,
                        });
                        result = AllocationResult::LegatoRetrigger { slot_index };
                    }
                }
                VoiceCommand::Retune { channel, bend } => {
                    self.gates.push(Gate::Retune(channel, bend));
                }
                VoiceCommand::Tune { id, tune } => {
                    if let Some(slot_index) = self.slot_of_voice(id) {
                        self.gates.push(Gate::Tune(slot_index, tune));
                    }
                }
                VoiceCommand::Control { id, control } => {
                    if let Some(slot_index) = self.slot_of_voice(id) {
                        self.gates.push(Gate::Control(slot_index, control));
                    }
                }
                VoiceCommand::ResetControls(id) => {
                    if let Some(slot_index) = self.slot_of_voice(id) {
                        self.gates.push(Gate::ResetControls(slot_index));
                    }
                }
                // `Damp` is an exclusive class's, which `PolySynth`'s rules
                // never turn on; if it came, its own release is the answer.
                VoiceCommand::Release { id, .. } | VoiceCommand::Damp(id) => {
                    if let Some(slot_index) = self.slot_of_voice(id) {
                        self.unmap(slot_index);
                        self.slots[slot_index].release();
                        self.gates.push(Gate::Off(slot_index));
                    }
                }
                VoiceCommand::Cut(id) => {
                    if let Some(slot_index) = self.slot_of_voice(id) {
                        self.unmap(slot_index);
                        self.slots[slot_index].silence();
                        self.gates.push(Gate::Cut(slot_index));
                    }
                }
            }
        }
        result
    }

    /// The id and velocity of the note a start or move is for: the strike's own
    /// when it is that key, else the key the core returned to (a mono note-off),
    /// named the way a MIDI 1.0 note is and at the velocity it was struck.
    fn strike_of(
        strike: Option<(NoteId, Velocity)>,
        channel: MidiChannel,
        key: NoteNumber,
        velocity16: NoteVelocity,
    ) -> (NoteId, Velocity) {
        match strike {
            Some((id, v)) if id.channel_note().is_some_and(|(_, note)| note == key) => (id, v),
            _ => (
                NoteId::from_channel_note(channel, key),
                velocity16.velocity(),
            ),
        }
    }

    /// Forgets the per-note id the slot is reached by, if it is still the one
    /// that names this slot: a later voice of the same key may have the id by
    /// now, and a stolen releasing voice must not take it with it.
    fn unmap(&mut self, slot_index: usize) {
        let id = self.slots[slot_index].id();
        if self.id_to_slot.get(id).copied() == Some(slot_index) {
            self.id_to_slot.remove(id);
        }
    }

    fn slot_of_voice(&self, id: VoiceId) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| !s.is_idle() && s.voice_id() == id)
    }

    /// What to do to voices now (the commands' effect on the synth's voices),
    /// taken in order.
    pub(crate) fn take_gates(&mut self) -> Vec<Gate> {
        std::mem::take(&mut self.gates)
    }

    /// Gives back the buffer `take_gates` lent, emptied, so it is reused.
    pub(crate) fn return_gates(&mut self, mut gates: Vec<Gate>) {
        gates.clear();
        // Nothing pushes a gate between `take_gates` and here (the synth
        // carries them out), so `self.gates` is the empty buffer `take` left.
        debug_assert!(self.gates.is_empty() && self.gates.capacity() == 0);
        self.gates = gates;
    }

    pub(crate) fn allocate(
        &mut self,
        id: NoteId,
        note: NoteNumber,
        channel: MidiChannel,
        velocity: Velocity,
        attribute: tutti_ump::Attribute,
    ) -> AllocationResult {
        // 16 bits, never 0 (a zero velocity is a note-off).
        let v16 = NoteVelocity::from_velocity(velocity).max(NoteVelocity::new(1));
        let on = Voice2::NoteOn {
            note,
            velocity: v16,
            attribute,
        };
        self.run(channel, &on, Some((id, velocity)))
    }

    /// Processes a note-off for `id`. Returns the slot whose voice should now be
    /// gated off (the note actually stopped sounding), or `None` when the note
    /// is held by sustain/sostenuto or no live voice matched `id`.
    pub(crate) fn release(&mut self, id: NoteId, channel: MidiChannel) -> Option<usize> {
        // Every id a caller holds comes from `from_channel_note`, which names a pair.
        let (_, note) = id
            .channel_note()
            .expect("a voice id is made from a channel and a note");
        let off = Voice2::NoteOff {
            note,
            velocity: NoteVelocity::MIN,
            attribute: tutti_ump::Attribute::None,
        };
        let before = self.gates.len();
        self.run(channel, &off, None);
        self.gates[before..].iter().find_map(|g| match g {
            Gate::Off(i) => Some(*i),
            Gate::Cut(_)
            | Gate::Retune(..)
            | Gate::Tune(..)
            | Gate::Control(..)
            | Gate::ResetControls(_)
            | Gate::Start { .. } => None,
        })
    }

    /// Call when envelope reaches zero to free the slot for reuse.
    pub(crate) fn voice_finished(&mut self, voice_id: VoiceId) {
        if let Some(i) = self.slot_of_voice(voice_id) {
            let id = self.slots[i].id();
            if self.id_to_slot.get(id).copied() == Some(i) {
                self.id_to_slot.remove(id);
            }
            self.slots[i].silence();
        }
    }

    fn control(&mut self, channel: MidiChannel, controller: tutti_ump::CCNumber, down: bool) {
        let cc = Voice2::ControlChange {
            controller,
            value: if down {
                ControlValue::MAX
            } else {
                ControlValue::MIN
            },
        };
        self.run(channel, &cc, None);
    }

    pub(crate) fn sustain_pedal(&mut self, channel: MidiChannel, on: bool) {
        self.control(channel, tutti_ump::CCNumber::SUSTAIN, on);
    }

    pub(crate) fn sostenuto_pedal(&mut self, channel: MidiChannel, on: bool) {
        self.control(channel, tutti_ump::CCNumber::SOSTENUTO, on);
    }

    /// Reset All Controllers: the pedals come up.
    pub(crate) fn reset_controllers(&mut self, channel: MidiChannel) {
        self.control(channel, tutti_ump::CCNumber::RESET_ALL, false);
    }

    pub(crate) fn all_notes_off(&mut self, channel: MidiChannel) {
        self.control(channel, tutti_ump::CCNumber::ALL_NOTES_OFF, false);
    }

    pub(crate) fn all_sound_off(&mut self, channel: MidiChannel) {
        self.control(channel, tutti_ump::CCNumber::ALL_SOUND_OFF, false);
    }

    pub(crate) fn update_envelope_level(&mut self, slot_index: usize, level: Amplitude) {
        if slot_index < self.slots.len() {
            self.slots[slot_index].set_envelope_level(level);
        }
    }

    /// Stores `table` as tuning program 0 of bank 0, the one every channel
    /// plays by until a Tuning Program or Bank Select says otherwise (the synth's
    /// construction-time scale).
    pub(crate) fn store_initial_tuning(&mut self, table: tutti_midi_protocol::KeyTable) {
        let zero = tutti_ump::U7::of(0);
        // Program 0 of bank 0 exists in any capacity.
        let stored = self.core.store_tuning(zero, zero, table);
        debug_assert!(stored.is_ok());
    }

    #[cfg(test)]
    pub(crate) fn core_phase(&self, slot_index: usize) -> Option<tutti_voicing::Phase> {
        let s = self.slots.get(slot_index).filter(|s| !s.is_idle())?;
        self.core.phase(s.voice_id())
    }

    #[cfg(test)]
    pub(crate) fn core_knows(&self, id: VoiceId) -> bool {
        self.core.phase(id).is_some()
    }

    /// The gates holding the voice in `slot_index`, if it is held at all.
    #[cfg(test)]
    pub(crate) fn holds_of(&self, slot_index: usize) -> Option<tutti_voicing::Holds> {
        let s = self.slots.get(slot_index).filter(|s| !s.is_idle())?;
        self.core.holds(s.voice_id())
    }

    /// What `channel` has been told apart from its notes (the note logic's
    /// [`ChannelState`]): the controllers a voice's render reads.
    pub(crate) fn channel_state(&self, channel: MidiChannel) -> &ChannelState {
        self.core.channel(channel)
    }

    /// Runs a Universal System Exclusive message the synth heard through the
    /// core and carries out what it returns (a retune of every channel, the
    /// notes a General MIDI System On cuts).
    pub(crate) fn universal(&mut self, heard: Heard) {
        let commands = self.core.universal(heard);
        self.commands.clear();
        self.commands.extend_from_slice(commands);
        self.carry_out(None);
    }

    /// The gain the Master Volume and Balance set (`VoiceAllocator::master_gain`).
    pub(crate) fn master_gain(&self) -> MasterGain {
        self.core.master_gain()
    }

    /// Runs a message that starts and releases no note (a bend, a sensitivity,
    /// a per-note controller) through the core.
    pub(crate) fn message(&mut self, channel: MidiChannel, message: &Voice2) {
        self.run(channel, message, None);
    }

    #[cfg(test)]
    pub(crate) fn active_count(&self) -> usize {
        self.slots.iter().filter(|s| !s.is_idle()).count()
    }

    pub(crate) fn slots(&self) -> &[VoiceSlot] {
        &self.slots
    }

    pub(crate) fn reset(&mut self) {
        for slot in &mut self.slots {
            *slot = VoiceSlot::default();
        }
        self.id_to_slot = PerNoteMap::new();
        self.core.reset();
        self.gates.clear();
    }
}

impl Clone for SynthVoices {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            core: self.core.clone(),
            slots: self.slots.clone(),
            id_to_slot: self.id_to_slot.clone(),
            commands: Vec::with_capacity(2 * self.config.max_voices.get() + 64),
            gates: Vec::with_capacity(2 * self.config.max_voices.get() + 64),
        }
    }
}

#[cfg(test)]
mod tests;
