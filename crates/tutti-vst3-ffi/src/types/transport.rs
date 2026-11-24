//! VST3-specific encoding of the shared [`TransportInfo`] into VST3's
//! `ProcessContext` struct.

use tutti_plugin_types::TransportInfo;
use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_;

use vst3::Steinberg::Vst::IProcessContextRequirements_::Flags_;

bitflags::bitflags! {
    /// `IProcessContextRequirements` flags, mirroring `Flags_` from the Steinberg
    /// SDK: a plugin returns an OR of these from `getProcessContextRequirements`
    /// to declare which [`ProcessContext`](vst3::Steinberg::Vst::ProcessContext)
    /// fields it consumes. [`to_process_context`] only fills (and marks valid)
    /// the fields whose bit is set; a plugin without the interface is given
    /// [`all`](Self::all).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ProcessContextRequirements: u32 {
        /// Plugin needs `systemTime`.
        const NEED_SYSTEM_TIME = crate::sdk_enum_u32(Flags_::kNeedSystemTime);
        /// Plugin needs `continousTimeSamples`.
        const NEED_CONTINOUS_TIME_SAMPLES = crate::sdk_enum_u32(Flags_::kNeedContinousTimeSamples);
        /// Plugin needs `projectTimeMusic`.
        const NEED_PROJECT_TIME_MUSIC = crate::sdk_enum_u32(Flags_::kNeedProjectTimeMusic);
        /// Plugin needs `barPositionMusic`.
        const NEED_BAR_POSITION_MUSIC = crate::sdk_enum_u32(Flags_::kNeedBarPositionMusic);
        /// Plugin needs `cycleStartMusic` / `cycleEndMusic`.
        const NEED_CYCLE_MUSIC = crate::sdk_enum_u32(Flags_::kNeedCycleMusic);
        /// Plugin needs `samplesToNextClock`.
        const NEED_SAMPLES_TO_NEXT_CLOCK = crate::sdk_enum_u32(Flags_::kNeedSamplesToNextClock);
        /// Plugin needs `tempo`.
        const NEED_TEMPO = crate::sdk_enum_u32(Flags_::kNeedTempo);
        /// Plugin needs `timeSigNumerator` / `timeSigDenominator`.
        const NEED_TIME_SIGNATURE = crate::sdk_enum_u32(Flags_::kNeedTimeSignature);
        /// Plugin needs the (host-track) chord field.
        const NEED_CHORD = crate::sdk_enum_u32(Flags_::kNeedChord);
        /// Plugin needs `frameRate`.
        const NEED_FRAME_RATE = crate::sdk_enum_u32(Flags_::kNeedFrameRate);
        /// Plugin needs the transport-state bits in `state`.
        const NEED_TRANSPORT_STATE = crate::sdk_enum_u32(Flags_::kNeedTransportState);
        // The plugin's own bits beyond the ones named, and `all()` as the
        // "plugin has no such interface" request for everything.
        const _ = !0;
    }
}

/// Builds a VST3 [`ProcessContext`](vst3::Steinberg::Vst::ProcessContext)
/// from the shared [`TransportInfo`], encoding the validity flags the
/// VST3 spec requires.
///
/// `requirements` is what the plugin returned from
/// `IProcessContextRequirements::getProcessContextRequirements` (or
/// [`all`](ProcessContextRequirements::all) when the plugin doesn't implement
/// that interface). Only the fields whose requirement bit is set are
/// populated and marked valid; the rest are left zeroed. Passing `all()`
/// fills everything.
///
/// Field mapping:
/// - `position.samples` → `projectTimeSamples` (always). VST3 has no validity
///   bit for this field, so a snapshot without one (`None`) sends `0`; the
///   engine's plugin node reports one whenever the tempo is usable. See
///   `TransportPosition::samples`.
/// - `position.continuous_samples` → `continousTimeSamples` (the monotonic
///   clock that does not reset on loop), sent as the count it is: `0` is the
///   clock's first frame
/// - `position.beat` → `projectTimeMusic`
/// - `bar.start` → `barPositionMusic`
/// - `loop_region` (when present) → `cycleStartMusic`/`cycleEndMusic`
/// - `system_time_ns` → `systemTime`, under `kSystemTimeValid`, when present
/// - [`TransportInfo::samples_to_nearest_clock`] → `samplesToNextClock`,
///   under `kClockValid`, when derivable
/// - `sample_rate` → `sampleRate` (always)
#[must_use]
pub fn to_process_context(
    t: &TransportInfo,
    requirements: ProcessContextRequirements,
) -> vst3::Steinberg::Vst::ProcessContext {
    use ProcessContextRequirements as need;

    let wants = |flag: ProcessContextRequirements| requirements.intersects(flag);
    // The MIDI clock grid under the playhead, at the snapshot's own rate.
    let to_clock = t.samples_to_nearest_clock();

    // `sdk_enum_u32` normalises the platform-varying SDK constants to `u32`.
    let state = {
        let mut state = 0u32;
        // The play/record/cycle bits are part of the transport state the plugin
        // reads only when it asked for kNeedTransportState.
        if wants(need::NEED_TRANSPORT_STATE) {
            if t.state.playing {
                state |= crate::sdk_enum_u32(StatesAndFlags_::kPlaying);
            }
            if t.state.recording {
                state |= crate::sdk_enum_u32(StatesAndFlags_::kRecording);
            }
            if t.loop_region.is_some() {
                state |= crate::sdk_enum_u32(StatesAndFlags_::kCycleActive);
            }
        }
        // `kCycleValid` is deliberately NOT set above beside `kCycleActive`.
        // The two answer different questions and the SDK routes them through
        // different requirement bits: `kNeedTransportState` covers
        // "kPlaying, kCycleActive, kRecording", while `kCycleValid` is listed
        // against `kNeedCycleMusic` (`ivstaudioprocessor.h:446,452`). And the
        // flag's own doc is about the *fields* — "cycleStartMusic and
        // barPositionMusic contain valid information" — not about whether a
        // cycle is running.
        //
        // So `kCycleValid` is set only when the plugin asked for
        // `kNeedCycleMusic` and a cycle is present: it is advertised exactly
        // when the bounds are filled. A `LoopRange` is finite and ordered, so
        // its presence makes the bounds usable.
        if wants(need::NEED_CYCLE_MUSIC) && t.loop_region.is_some() {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kCycleValid);
        }
        // Each `*Valid` bit advertises that the matching field below is filled,
        // so it must track the same requirement gate — AND the value must be one
        // a plugin can compute with.
        //
        // The requirement mask alone is not enough: it says what the plugin
        // *asked for*, not what the host *has*. Setting a validity bit over a
        // NaN or an infinity is worse than leaving it clear, because a plugin
        // that trusts the bit propagates the NaN through its timing math into
        // the audio buffer. A `Beat` is not range-checked, so the two musical
        // positions are gated on finiteness; the tempo (`Bpm`: finite and above
        // zero) and the cycle (`LoopRange`: finite and ordered) are valid by
        // their types.
        if wants(need::NEED_PROJECT_TIME_MUSIC) && t.position.beat.get().is_finite() {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kProjectTimeMusicValid);
        }
        if wants(need::NEED_BAR_POSITION_MUSIC) && t.bar.start.get().is_finite() {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kBarPositionValid);
        }
        if wants(need::NEED_TEMPO) {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kTempoValid);
        }
        if wants(need::NEED_TIME_SIGNATURE) {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kTimeSigValid);
        }
        // `continousTimeSamples` is filled below whenever the plugin asked for
        // it, so `kContTimeValid` is set on the same condition: without the bit
        // a spec-correct plugin ignores the field.
        if wants(need::NEED_CONTINOUS_TIME_SAMPLES) {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kContTimeValid);
        }
        // The host clock and the MIDI clock: flagged only when the snapshot
        // has them (an offline render has no host clock; a non-finite position
        // leaves no clock grid), on the same condition that fills
        // the field below. `kSmpteValid` / `kChordValid` stay clear: their
        // fields have no producer.
        if wants(need::NEED_SYSTEM_TIME) && t.system_time_ns.is_some() {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kSystemTimeValid);
        }
        if wants(need::NEED_SAMPLES_TO_NEXT_CLOCK) && to_clock.is_some() {
            state |= crate::sdk_enum_u32(StatesAndFlags_::kClockValid);
        }
        state
    };

    let mut ctx: vst3::Steinberg::Vst::ProcessContext = crate::sdk::zeroed();
    ctx.state = state;
    // Always-on: not gated by any requirement flag in the spec.
    ctx.sampleRate = t.sample_rate.get();
    // VST3 carries no `kProjectTimeSamplesValid` bit — the field is always read
    // as fact — so a snapshot without a project-time position can only send
    // 0 (see `TransportPosition::samples`).
    ctx.projectTimeSamples = t.position.samples.unwrap_or(0);

    if wants(need::NEED_CONTINOUS_TIME_SAMPLES) {
        // The continuous clock is monotonic across loop/cycle boundaries and is
        // distinct from `projectTimeSamples`, which jumps back on a loop. Its
        // count is sent as it is: `0` is the first frame of the clock, never a
        // sign to send project time, which would step the clock backward at the
        // next block.
        ctx.continousTimeSamples = t.position.continuous_samples;
    }
    if wants(need::NEED_SYSTEM_TIME)
        && let Some(ns) = t.system_time_ns
    {
        ctx.systemTime = ns;
    }
    if wants(need::NEED_PROJECT_TIME_MUSIC) {
        ctx.projectTimeMusic = t.position.beat.get();
    }
    if wants(need::NEED_BAR_POSITION_MUSIC) {
        ctx.barPositionMusic = t.bar.start.get();
    }
    // Same condition as `kCycleValid` above: the bit and the fields it
    // advertises have to move together.
    if wants(need::NEED_CYCLE_MUSIC)
        && let Some(region) = t.loop_region
    {
        ctx.cycleStartMusic = region.range.start().get();
        ctx.cycleEndMusic = region.range.end().get();
    }
    if wants(need::NEED_TEMPO) {
        ctx.tempo = t.timing.tempo.get();
    }
    if wants(need::NEED_TIME_SIGNATURE) {
        // Conversion at the boundary, gating unchanged: the `kTimeSigValid` bit
        // above is set from the same `wants` check, so folding the gate into the
        // conversion would split a pair that has to move together.
        ctx.timeSigNumerator = t.timing.signature.beats_per_bar().into();
        ctx.timeSigDenominator = t.timing.signature.note_value().into();
    }
    if wants(need::NEED_SAMPLES_TO_NEXT_CLOCK)
        && let Some(frames) = to_clock
    {
        ctx.samplesToNextClock = frames;
    }
    // `smpteOffsetSubframes` / `frameRate` (kNeedFrameRate) and the chord field
    // (kNeedChord) are not sourced from TransportInfo yet; left zeroed with
    // `kSmpteValid` / `kChordValid` clear. When a producer exists they gate on
    // NEED_FRAME_RATE / NEED_CHORD here and set their own valid bit.
    ctx
}

#[cfg(test)]
mod tests {
    use super::ProcessContextRequirements as need;
    use super::*;
    use tutti_music::LoopRange;
    use tutti_plugin_types::{BeatsPerBar, LoopRegion, NoteValue, TimeSignature};
    use tutti_units::{Beat, Bpm, SampleRate};
    use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_;

    /// A TransportInfo with every field set to a recognisable non-zero value,
    /// so a "field was populated" check is unambiguous.
    fn populated_transport() -> TransportInfo {
        TransportInfo::new(SampleRate::of(48_000.0))
            .position_samples(1_234)
            .continuous_samples(5_678)
            .beat(Beat::new(4.0))
            .bar(Beat::new(8.0), Default::default())
            .loop_region(LoopRegion::new(
                LoopRange::new(Beat::new(2.0), Beat::new(6.0)).expect("a loop"),
            ))
            .tempo(Bpm::of(128.0))
            .time_signature(TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH))
            .playing(true)
            .recording(true)
            .system_time_ns(9_000_000_000)
    }

    /// An unusable value is not advertised as valid, however loudly the
    /// plugin asked for it.
    ///
    /// The requirement mask says what the plugin *wants*, not what the host
    /// *has*: a `*Valid` bit is set only over a value a plugin can compute
    /// with, so a non-finite position never reaches the plugin flagged valid.
    /// The VST2 path gates on the value the same way.
    #[test]
    fn a_non_finite_position_is_not_advertised_as_valid() {
        // Everything requested, so only the value can clear a bit.
        let all = need::all();

        let mut t = populated_transport();
        t.position.beat = Beat::new(f64::INFINITY);
        let ctx = to_process_context(&t, all);
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kProjectTimeMusicValid),
            0,
            "an infinite musical position must not be flagged valid"
        );

        let mut t = populated_transport();
        t.bar.start = Beat::new(f64::NAN);
        let ctx = to_process_context(&t, all);
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kBarPositionValid),
            0,
            "a NaN bar position must not be flagged valid"
        );

        // The gate must not fire on good values — otherwise it would pass by
        // clearing every bit unconditionally.
        let ctx = to_process_context(&populated_transport(), all);
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kTempoValid),
            0
        );
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kProjectTimeMusicValid),
            0
        );
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kBarPositionValid),
            0
        );
    }

    #[test]
    fn all_bits_fills_everything() {
        let t = populated_transport();
        let ctx = to_process_context(&t, need::all());

        assert_eq!(ctx.sampleRate, 48_000.0);
        assert_eq!(ctx.projectTimeSamples, 1_234);
        assert_eq!(ctx.continousTimeSamples, 5_678);
        assert_eq!(ctx.projectTimeMusic, 4.0);
        assert_eq!(ctx.barPositionMusic, 8.0);
        assert_eq!(ctx.cycleStartMusic, 2.0);
        assert_eq!(ctx.cycleEndMusic, 6.0);
        assert_eq!(ctx.tempo, 128.0);
        assert_eq!(ctx.timeSigNumerator, 7);
        assert_eq!(ctx.timeSigDenominator, 8);

        // The transport-state bits (the validity bits are pinned per field below).
        let s = ctx.state;
        assert_ne!(s & crate::sdk_enum_u32(StatesAndFlags_::kPlaying), 0);
        assert_ne!(s & crate::sdk_enum_u32(StatesAndFlags_::kRecording), 0);
        assert_ne!(s & crate::sdk_enum_u32(StatesAndFlags_::kCycleActive), 0);

        // Fields with no producer are neither written nor flagged.
        assert_eq!(s & crate::sdk_enum_u32(StatesAndFlags_::kSmpteValid), 0);
        assert_eq!(s & crate::sdk_enum_u32(StatesAndFlags_::kChordValid), 0);
    }

    /// Per `ivstprocesscontext.h`, each `*Valid` bit must be set iff its field
    /// carries meaning. This pins the pairing for every field this host fills:
    /// requesting exactly one field sets exactly that field's valid bit, and
    /// not requesting it leaves both the field and the bit clear.
    #[test]
    fn each_valid_bit_tracks_its_own_field() {
        let t = populated_transport();

        // (requirement bit, valid bit, "is the field non-zero?" probe)
        let cases: &[(
            &str,
            need,
            u32,
            fn(&vst3::Steinberg::Vst::ProcessContext) -> bool,
        )] = &[
            (
                "projectTimeMusic",
                need::NEED_PROJECT_TIME_MUSIC,
                crate::sdk_enum_u32(StatesAndFlags_::kProjectTimeMusicValid),
                |c| c.projectTimeMusic != 0.0,
            ),
            (
                "barPositionMusic",
                need::NEED_BAR_POSITION_MUSIC,
                crate::sdk_enum_u32(StatesAndFlags_::kBarPositionValid),
                |c| c.barPositionMusic != 0.0,
            ),
            (
                "tempo",
                need::NEED_TEMPO,
                crate::sdk_enum_u32(StatesAndFlags_::kTempoValid),
                |c| c.tempo != 0.0,
            ),
            (
                "timeSig",
                need::NEED_TIME_SIGNATURE,
                crate::sdk_enum_u32(StatesAndFlags_::kTimeSigValid),
                |c| c.timeSigNumerator != 0,
            ),
            (
                "continousTimeSamples",
                need::NEED_CONTINOUS_TIME_SAMPLES,
                crate::sdk_enum_u32(StatesAndFlags_::kContTimeValid),
                |c| c.continousTimeSamples != 0,
            ),
            (
                "systemTime",
                need::NEED_SYSTEM_TIME,
                crate::sdk_enum_u32(StatesAndFlags_::kSystemTimeValid),
                |c| c.systemTime != 0,
            ),
            (
                "cycleStartMusic",
                need::NEED_CYCLE_MUSIC,
                crate::sdk_enum_u32(StatesAndFlags_::kCycleValid),
                |c| c.cycleStartMusic != 0.0,
            ),
        ];

        for (name, requirement, valid_bit, field_filled) in cases {
            let on = to_process_context(&t, *requirement);
            assert!(field_filled(&on), "{name}: requested but not filled");
            assert_ne!(
                on.state & valid_bit,
                0,
                "{name}: filled but its *Valid bit is clear — a spec-correct \
                 plugin ignores the field, so the feature is dead"
            );

            // Exactly that bit, and no other field filled.
            assert_eq!(on.state, *valid_bit, "{name}: another bit rode with it");
            for (other, _, _, other_filled) in cases {
                assert!(
                    other == name || !other_filled(&on),
                    "{name}: requesting it also filled {other}"
                );
            }

            // The inverse: nothing requested → field untouched, bit clear.
            let off = to_process_context(&t, need::empty());
            assert!(
                !field_filled(&off),
                "{name}: filled without being requested"
            );
            assert_eq!(
                off.state & valid_bit,
                0,
                "{name}: valid bit set with no data"
            );
        }
    }

    /// `kCycleActive` and `kCycleValid` answer different questions and ride
    /// different requirement bits.
    ///
    /// The SDK maps them separately: `kNeedTransportState` covers
    /// "kPlaying, kCycleActive, kRecording" while `kCycleValid` is listed
    /// against `kNeedCycleMusic` (`ivstaudioprocessor.h:446,452`), and the
    /// flag itself is documented as "cycleStartMusic and barPositionMusic
    /// contain valid information" — a claim about fields, not about whether a
    /// cycle is running.
    ///
    /// Setting `kCycleValid` from the cycle's presence breaks both directions at
    /// once, and each half needs its own assertion because either alone still
    /// passes when only one direction is right.
    #[test]
    fn cycle_active_and_cycle_valid_are_gated_separately() {
        let t = populated_transport();

        // A plugin that asked only for transport state gets the running flag
        // but no claim about the bounds — which it never asked for, and which
        // are still `mem::zeroed` here.
        let transport_only = to_process_context(&t, need::NEED_TRANSPORT_STATE);
        assert_ne!(
            transport_only.state & crate::sdk_enum_u32(StatesAndFlags_::kCycleActive),
            0,
            "a cycling transport must still report kCycleActive"
        );
        assert_eq!(
            transport_only.state & crate::sdk_enum_u32(StatesAndFlags_::kCycleValid),
            0,
            "kCycleValid claims cycleStartMusic/cycleEndMusic are meaningful, \
             but nothing asked for them and they are still zero"
        );
        assert_eq!(transport_only.cycleStartMusic, 0.0);
        assert_eq!(transport_only.cycleEndMusic, 0.0);

        // The converse: a plugin that asked only for cycle music gets the
        // bounds AND the bit that makes them readable. Without the bit a
        // spec-correct plugin ignores the fields, so the loop points are
        // computed and thrown away.
        let cycle_only = to_process_context(&t, need::NEED_CYCLE_MUSIC);
        assert_ne!(
            cycle_only.state & crate::sdk_enum_u32(StatesAndFlags_::kCycleValid),
            0,
            "cycle bounds were filled but not flagged — the plugin ignores them"
        );
        assert_eq!(cycle_only.cycleStartMusic, 2.0);
        assert_eq!(cycle_only.cycleEndMusic, 6.0);
    }

    /// **The continuous clock's first frame is sent as 0, not as project
    /// time.** A node's clock starts at 0 wherever the playhead is, and its next
    /// block continues from the block's length: project time in its place would
    /// make `continousTimeSamples` step backward, which the field never does.
    #[test]
    fn a_continuous_clock_at_zero_is_sent_as_zero() {
        let first = populated_transport().continuous_samples(0);
        let ctx = to_process_context(&first, need::NEED_CONTINOUS_TIME_SAMPLES);
        assert_eq!(ctx.projectTimeSamples, 1_234);
        assert_eq!(ctx.continousTimeSamples, 0);
    }

    /// Always-on fields (sampleRate, projectTimeSamples) are populated even
    /// when the plugin requested literally nothing.
    #[test]
    fn always_on_fields_survive_empty_requirements() {
        let t = populated_transport();
        let ctx = to_process_context(&t, need::empty());

        assert_eq!(ctx.sampleRate, 48_000.0);
        assert_eq!(ctx.projectTimeSamples, 1_234);
        assert_eq!(ctx.state, 0);
    }

    /// **`samplesToNextClock` is the signed distance to the nearest 24-ppq
    /// MIDI clock, under `kClockValid`**, and a snapshot with no host clock
    /// or no finite position claims neither field. 128 BPM at 48 kHz: a quarter
    /// is 22 500 frames, a clock 937.5; a playhead 0.2 clock past one is
    /// 187.5 frames after it, sent as -188 (rounded half away from zero).
    #[test]
    fn the_midi_clock_and_host_time_are_flagged_only_when_known() {
        let wants = need::NEED_SAMPLES_TO_NEXT_CLOCK | need::NEED_SYSTEM_TIME;
        let t = populated_transport().beat(Beat::new(4.0 + 0.2 / 24.0));
        let ctx = to_process_context(&t, wants);
        assert_eq!(ctx.samplesToNextClock, -188);
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kClockValid),
            0
        );
        assert_eq!(ctx.systemTime, 9_000_000_000);
        assert_ne!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kSystemTimeValid),
            0
        );

        let mut bare = populated_transport().beat(Beat::new(f64::NAN));
        bare.system_time_ns = None;
        let ctx = to_process_context(&bare, wants);
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kClockValid),
            0
        );
        assert_eq!(
            ctx.state & crate::sdk_enum_u32(StatesAndFlags_::kSystemTimeValid),
            0
        );
        assert_eq!(ctx.samplesToNextClock, 0);
        assert_eq!(ctx.systemTime, 0);
    }
}
