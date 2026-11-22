//! Build a `vst::TimeInfo` snapshot from [`TransportInfo`].
//!
//! The snapshot is stored into the host-state `time_info` seqlock
//! ([`TransportWriter`](crate::transport_cell::TransportWriter)) before each
//! process call and served back to the plugin via the
//! `audioMasterGetTime` callback in [`crate::host::HostState`].
//!
//! Flag bits come straight from `vst::TimeInfoFlags` (the vst-rs
//! transcription of the VST2.4 SDK); the bit positions are never hand-rolled.
//! VST2.4 clusters four `*_VALID` bits at non-adjacent positions
//! (`PPQ=9, TEMPO=10, BARS=11, CYCLE=12, TIME_SIG=13`) and
//! `TRANSPORT_RECORDING` sits at bit 3 — mistranscribing any of them
//! silently mis-signals the plugin's transport state.
//!
//! `kVstSmpteValid` is never set: the transport carries no SMPTE offset or frame rate, so
//! `smpteOffset` and `smpteFrameRate` stay zero and are not claimed.
//!
//! # Two flag families, two different rules
//!
//! **`*_VALID` bits are claims about *this* snapshot.** Setting one tells the
//! plugin "the matching field holds a real number, use it". A `f64` has no
//! absent value, so a non-`Option` field is *not* evidence that the field was
//! populated: a position is a `Beat`, which may hold NaN, and a plugin told
//! `kVstPpqPosValid` does arithmetic with it. Each position bit is therefore
//! gated on the field being finite (JUCE's `setFromOptional` is the same
//! shape). The tempo and the signature are the exceptions, because their
//! *types* do the work (a `Bpm` is finite and above zero, a `TimeSignature`
//! is in range) — see [`build_vst2_time_info`].
//!
//! **`TRANSPORT_CHANGED` is a claim about the *transition*.** Per the SDK it
//! means the transport state just changed, and plugins use it as a reset
//! trigger: a tempo-synced delay or arpeggiator retriggers on it, a reverb
//! flushes its tail. Asserting it every block means those plugins reset every
//! 64 samples and never advance. It is a strict edge, computed against the
//! previously served snapshot exactly as JUCE and Ardour do.

use tutti_plugin_types::TransportInfo;
// Shared with the VST3 path so the two cannot drift apart.
use tutti_plugin_types::{AutomationMode, SampleRate};
use tutti_units::{Beat, Bpm};

/// The transport bits whose transition defines `TRANSPORT_CHANGED`.
///
/// Ardour compares playing/recording/cycle; JUCE compares playing/recording.
/// The wider set wins here — a cycle being switched on or off is a transport
/// state change by any reading of the SDK, and a plugin that resets on it is
/// resetting for a real reason.
fn transport_bits(flags: vst::TimeInfoFlags) -> i32 {
    use vst::TimeInfoFlags as F;
    (flags & (F::TRANSPORT_PLAYING | F::TRANSPORT_RECORDING | F::TRANSPORT_CYCLE_ACTIVE)).bits()
}

/// Builds the snapshot the plugin will read via `audioMasterGetTime`.
///
/// `previous` is the snapshot last served to this plugin (`None` before the
/// first block). It is used only to compute the `TRANSPORT_CHANGED` edge; every
/// other bit is a function of `transport` and `automation` alone.
pub(crate) fn build_vst2_time_info(
    transport: &TransportInfo,
    sample_rate: SampleRate,
    previous: Option<&vst::TimeInfo>,
    automation: AutomationMode,
) -> vst::TimeInfo {
    use vst::TimeInfoFlags as F;

    // TIME_SIG_VALID and TEMPO_VALID are unconditional. Not because the fields
    // are non-`Option` — that argument shipped `tempo = 0, kVstTempoValid` to
    // plugins — but because `TimeSignature` and `Bpm` are validated newtypes
    // whose constructors refuse a value outside the legal range: a signature
    // is in range, and a tempo is finite and above zero (plugins divide by
    // it). There is no representable invalid one, so the claim is always true.
    // A `Beat` is not checked, so each position bit is gated on finiteness.
    let mut flags = F::TIME_SIG_VALID | F::TEMPO_VALID;

    if transport.position.beat.get().is_finite() {
        flags |= F::PPQ_POS_VALID;
    }
    if transport.bar.start.get().is_finite() {
        flags |= F::BARS_VALID;
    }

    // `nanoSeconds` and `samplesToNextClock` are filled only when the
    // producer has them, and flagged on the same condition.
    if transport.system_time_ns.is_some() {
        flags |= F::NANOSECONDS_VALID;
    }
    // At the snapshot's own rate, the one its position was taken at, as the
    // VST3 path reads it.
    let to_clock = transport.samples_to_nearest_clock();
    if to_clock.is_some() {
        flags |= F::VST_CLOCK_VALID;
    }

    if transport.state.playing {
        flags |= F::TRANSPORT_PLAYING;
    }
    if transport.state.recording {
        flags |= F::TRANSPORT_RECORDING;
    }
    // A `LoopRange` is finite and ordered, so the cycle's presence signals both
    // that it is active AND that the cycle_start_pos / cycle_end_pos fields are
    // populated: without CYCLE_POS_VALID the plugin ignores the boundaries.
    let cycle = transport.loop_region.map(|l| l.range);
    if cycle.is_some() {
        flags |= F::TRANSPORT_CYCLE_ACTIVE | F::CYCLE_POS_VALID;
    }

    // The host's automation mode: `kVstAutomationReading` and `kVstAutomationWriting`.
    if matches!(
        automation,
        AutomationMode::Reading | AutomationMode::ReadWriting
    ) {
        flags |= F::AUTOMATION_READING;
    }
    if matches!(
        automation,
        AutomationMode::Writing | AutomationMode::ReadWriting
    ) {
        flags |= F::AUTOMATION_WRITING;
    }

    // The edge. Set only when the transport bits differ from the snapshot the
    // plugin last saw; on the very first block there is no previous state, so
    // there is no transition to report either.
    if let Some(prev) = previous {
        let prev_bits = transport_bits(F::from_bits_truncate(prev.flags));
        if transport_bits(flags) != prev_bits {
            flags |= F::TRANSPORT_CHANGED;
        }
    }

    vst::TimeInfo {
        sample_rate: sample_rate.get(),
        // VST2.4 has no `*_VALID` bit for `sample_pos` — the plugin always reads
        // it as fact — so a snapshot without a project-time position can only
        // send 0. The engine's plugin node always reports one when the tempo
        // is usable (`TransportPosition::samples`). A count-in before the
        // project start clamps to 0: the field is documented non-negative.
        sample_pos: transport.position.samples.unwrap_or(0).max(0) as f64,
        // The ABI's `f64` nanoseconds, from the host's `i64` count: exact
        // below 2^53 ns (104 days of uptime), and a clock reading, not a
        // position, past that.
        nanoseconds: transport.system_time_ns.unwrap_or(0) as f64,
        samples_to_next_clock: to_clock.unwrap_or(0),
        // The SDK struct's `f64`s: the units stop at the C write.
        ppq_pos: transport.position.beat.get(),
        tempo: transport.timing.tempo.get(),
        bar_start_pos: transport.bar.start.get(),
        cycle_start_pos: cycle.map_or(0.0, |c| c.start().get()),
        cycle_end_pos: cycle.map_or(0.0, |c| c.end().get()),
        time_sig_numerator: transport.timing.signature.beats_per_bar().into(),
        time_sig_denominator: transport.timing.signature.note_value().into(),
        flags: flags.bits(),
        ..Default::default()
    }
}

/// The snapshot for the next piece of a split block, `frames` after `t`: sample position,
/// host time and (while rolling) the musical position move on, the bar start moves to the
/// last bar line the position has passed, and `samplesToNextClock` is measured again at the
/// new position (or `kVstClockValid` is cleared when it cannot be); `kVstTransportChanged`
/// was an edge of the block's first piece and does not repeat.
pub(crate) fn advance_time_info(
    mut t: vst::TimeInfo,
    frames: usize,
    sample_rate: SampleRate,
) -> vst::TimeInfo {
    use vst::TimeInfoFlags as F;
    let flags = F::from_bits_truncate(t.flags);
    let secs = frames as f64 / sample_rate.get();
    t.sample_pos += frames as f64;
    if flags.contains(F::NANOSECONDS_VALID) {
        t.nanoseconds += secs * 1e9;
    }
    if flags.contains(F::TRANSPORT_PLAYING)
        && flags.contains(F::PPQ_POS_VALID)
        && flags.contains(F::TEMPO_VALID)
    {
        t.ppq_pos += secs * t.tempo / 60.0;
        if flags.contains(F::VST_CLOCK_VALID) {
            // The nearest 24-ppq clock moves with the position.
            let clock = Bpm::new(t.tempo).and_then(|tempo| {
                TransportInfo::new(sample_rate)
                    .tempo(tempo)
                    .beat(Beat::new(t.ppq_pos))
                    .samples_to_nearest_clock()
            });
            match clock {
                Some(samples) => t.samples_to_next_clock = samples,
                // No grid to measure on: the stale figure is not claimed.
                None => t.flags &= !F::VST_CLOCK_VALID.bits(),
            }
        }
        // A bar line inside the piece: the bar is `numerator` beats of
        // `4 / denominator` quarters.
        if flags.contains(F::BARS_VALID) && t.time_sig_denominator > 0 {
            let bar = f64::from(t.time_sig_numerator) * 4.0 / f64::from(t.time_sig_denominator);
            if bar > 0.0 && t.ppq_pos >= t.bar_start_pos + bar {
                t.bar_start_pos += ((t.ppq_pos - t.bar_start_pos) / bar).floor() * bar;
            }
        }
    }
    t.flags &= !F::TRANSPORT_CHANGED.bits();
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_music::LoopRange;
    use tutti_plugin_types::LoopRegion;
    use tutti_units::{Beat, Bpm};
    use vst::TimeInfoFlags as F;

    fn base() -> TransportInfo {
        TransportInfo::new(SampleRate::SR_48K)
    }

    fn cycle(start: f64, end: f64) -> LoopRegion {
        LoopRegion::new(LoopRange::new(Beat::new(start), Beat::new(end)).expect("a loop"))
    }

    fn playing_transport() -> TransportInfo {
        base()
            .playing(true)
            .recording(true)
            .loop_region(cycle(0.0, 8.0))
    }

    /// Spec: vst2.timeinfo.kVstTransportRecording, vst2.timeinfo.kVstTransportCycleActive, vst2.timeinfo.kVstTransportPlaying, vst2.timeinfo.kVstPpqPosValid, vst2.timeinfo.kVstTempoValid
    #[test]
    fn flags_recording_and_cycle_active() {
        // Recording + cycle-active + playing, with real tempo/position values,
        // so every VALID bit is genuinely earned.
        let transport = playing_transport()
            .tempo(Bpm::of(120.0))
            .beat(Beat::new(4.0))
            .bar(Beat::new(4.0), Default::default());

        // First block: no previous snapshot, so no transition to report.
        let info = build_vst2_time_info(&transport, SampleRate::SR_48K, None, AutomationMode::Off);

        let expected = (F::PPQ_POS_VALID
            | F::TEMPO_VALID
            | F::TIME_SIG_VALID
            | F::BARS_VALID
            | F::TRANSPORT_PLAYING
            | F::TRANSPORT_RECORDING
            | F::TRANSPORT_CYCLE_ACTIVE
            | F::CYCLE_POS_VALID
            // A finite position at the snapshot's tempo and rate put the MIDI clock grid under the
            // playhead.
            | F::VST_CLOCK_VALID)
            .bits();

        assert_eq!(info.flags, expected);

        // Spot-check the easily-mistranscribed bits are actually set.
        assert_ne!(info.flags & F::TRANSPORT_RECORDING.bits(), 0);
        assert_ne!(info.flags & F::CYCLE_POS_VALID.bits(), 0);
        // Automation off: AUTOMATION_WRITING (bit 6) is not TRANSPORT_RECORDING
        // (bit 3), which a hand-rolled flag table can confuse, and READING is clear.
        assert_eq!(info.flags & F::AUTOMATION_WRITING.bits(), 0);
        assert_eq!(info.flags & F::AUTOMATION_READING.bits(), 0);
    }

    /// The host's automation mode reaches the snapshot as `kVstAutomationReading` (bit 7) and
    /// `kVstAutomationWriting` (bit 6), each set only by a mode that reads or writes, and
    /// no transport bit moves with it.
    /// Spec: vst2.timeinfo.kVstAutomationReading, vst2.timeinfo.kVstAutomationWriting
    #[test]
    fn automation_mode_sets_exactly_its_flags() {
        use vst::TimeInfoFlags as F;
        let transport = playing_transport();
        let off = build_vst2_time_info(&transport, SampleRate::SR_48K, None, AutomationMode::Off);
        for (mode, reading, writing) in [
            (AutomationMode::Off, false, false),
            (AutomationMode::Reading, true, false),
            (AutomationMode::Writing, false, true),
            (AutomationMode::ReadWriting, true, true),
        ] {
            let info = build_vst2_time_info(&transport, SampleRate::SR_48K, None, mode);
            let flags = F::from_bits_truncate(info.flags);
            assert_eq!(flags.contains(F::AUTOMATION_READING), reading, "{mode:?}");
            assert_eq!(flags.contains(F::AUTOMATION_WRITING), writing, "{mode:?}");
            let automation = F::AUTOMATION_READING | F::AUTOMATION_WRITING;
            assert_eq!(
                flags - automation,
                F::from_bits_truncate(off.flags),
                "{mode:?} moves no other bit"
            );
        }
    }

    /// `kVstSmpteValid` (bit 14) is claimed for no transport: the snapshot has no SMPTE
    /// offset or frame rate, so the bit stays clear and the fields stay zero.
    #[test]
    fn smpte_is_never_claimed() {
        let transport = playing_transport()
            .tempo(Bpm::of(120.0))
            .beat(Beat::new(4.0))
            .bar(Beat::new(4.0), Default::default());
        for transport in [&transport, &base()] {
            let info = build_vst2_time_info(
                transport,
                SampleRate::SR_48K,
                None,
                AutomationMode::ReadWriting,
            );
            assert_eq!(info.flags & (1 << 14), 0);
            assert_eq!(info.flags & F::SMPTE_VALID.bits(), 0);
            assert_eq!(info.smpte_offset, 0);
        }
    }

    #[test]
    fn flags_stopped_no_cycle() {
        // Stopped, not recording, no cycle. A new snapshot carries a real 120
        // BPM tempo and 4/4, and 0.0 is a finite position, so the
        // position/tempo VALID bits stand, and so does the MIDI clock grid
        // under the playhead; `nanoSeconds` is not claimed, with no host clock.
        let transport = base();
        let info = build_vst2_time_info(&transport, SampleRate::SR_48K, None, AutomationMode::Off);

        let expected = (F::PPQ_POS_VALID
            | F::TEMPO_VALID
            | F::TIME_SIG_VALID
            | F::BARS_VALID
            | F::VST_CLOCK_VALID)
            .bits();

        assert_eq!(info.flags, expected);
        assert_eq!(info.flags & F::TRANSPORT_PLAYING.bits(), 0);
        assert_eq!(info.flags & F::TRANSPORT_RECORDING.bits(), 0);
        assert_eq!(info.flags & F::TRANSPORT_CYCLE_ACTIVE.bits(), 0);
        assert_eq!(info.flags & F::CYCLE_POS_VALID.bits(), 0);
    }

    /// `kVstTransportChanged` means the transport state *just
    /// changed*, not "a transport snapshot exists". The previous version of
    /// this function asserted it on every block — and the previous version of
    /// this test asserted that as correct — so a tempo-synced delay or
    /// arpeggiator retriggered every 64 samples and never advanced, and a
    /// reverb flushed its tail every block.
    /// Spec: vst2.timeinfo.kVstTransportChanged
    #[test]
    fn transport_changed_is_an_edge_not_a_level() {
        let stopped = base();
        let playing = base().playing(true);

        // Stopped → playing: an edge.
        let first = build_vst2_time_info(&stopped, SampleRate::SR_48K, None, AutomationMode::Off);
        let start = build_vst2_time_info(
            &playing,
            SampleRate::SR_48K,
            Some(&first),
            AutomationMode::Off,
        );
        assert_ne!(start.flags & F::TRANSPORT_CHANGED.bits(), 0);

        // Playing → still playing, over and over: no edge. A level-triggered
        // implementation would keep asserting the bit here.
        let mut prev = start;
        for _ in 0..8 {
            let next = build_vst2_time_info(
                &playing,
                SampleRate::SR_48K,
                Some(&prev),
                AutomationMode::Off,
            );
            assert_eq!(
                next.flags & F::TRANSPORT_CHANGED.bits(),
                0,
                "TRANSPORT_CHANGED must not repeat while the transport is steady"
            );
            prev = next;
        }

        // Playing → stopped: an edge again.
        let stop = build_vst2_time_info(
            &stopped,
            SampleRate::SR_48K,
            Some(&prev),
            AutomationMode::Off,
        );
        assert_ne!(stop.flags & F::TRANSPORT_CHANGED.bits(), 0);
    }

    /// The first snapshot has nothing to compare against, so there is no
    /// transition to announce.
    #[test]
    fn transport_changed_is_clear_on_the_first_snapshot() {
        let info = build_vst2_time_info(
            &playing_transport(),
            SampleRate::SR_48K,
            None,
            AutomationMode::Off,
        );
        assert_eq!(info.flags & F::TRANSPORT_CHANGED.bits(), 0);
    }

    /// Recording and cycle transitions are transport changes too, even while
    /// `playing` never moves.
    #[test]
    fn transport_changed_fires_for_recording_and_cycle_transitions() {
        let playing = base().playing(true);
        let base = build_vst2_time_info(&playing, SampleRate::SR_48K, None, AutomationMode::Off);

        let armed = playing.recording(true);
        let rec =
            build_vst2_time_info(&armed, SampleRate::SR_48K, Some(&base), AutomationMode::Off);
        assert_ne!(rec.flags & F::TRANSPORT_CHANGED.bits(), 0);

        let looped = armed.loop_region(cycle(0.0, 8.0));
        let cyc =
            build_vst2_time_info(&looped, SampleRate::SR_48K, Some(&rec), AutomationMode::Off);
        assert_ne!(cyc.flags & F::TRANSPORT_CHANGED.bits(), 0);
    }

    /// A pure position advance is not a transport state change.
    #[test]
    fn moving_the_playhead_is_not_a_transport_change() {
        let a = base().playing(true).beat(Beat::new(1.0));
        let b = base().playing(true).beat(Beat::new(2.0));

        let first = build_vst2_time_info(&a, SampleRate::SR_48K, None, AutomationMode::Off);
        let second =
            build_vst2_time_info(&b, SampleRate::SR_48K, Some(&first), AutomationMode::Off);
        assert_eq!(second.flags & F::TRANSPORT_CHANGED.bits(), 0);
    }

    /// The plugin reads the snapshot's tempo unchanged (the project map's,
    /// which the engine's plugin node puts there); VST2 has no slope field,
    /// so the snapshot's `tempo_inc` reaches nothing.
    #[test]
    fn the_snapshots_tempo_is_what_the_plugin_reads() {
        let transport = base().tempo(Bpm::of(97.5)).tempo_inc(0.001);
        let info = build_vst2_time_info(&transport, SampleRate::SR_48K, None, AutomationMode::Off);
        assert_eq!(info.tempo, 97.5);
        assert_ne!(info.flags & F::TEMPO_VALID.bits(), 0);
    }

    #[test]
    fn non_finite_fields_are_withheld() {
        let transport = base()
            .beat(Beat::new(f64::INFINITY))
            .bar(Beat::new(f64::NAN), Default::default());
        let info = build_vst2_time_info(&transport, SampleRate::SR_48K, None, AutomationMode::Off);

        assert_eq!(info.flags & F::PPQ_POS_VALID.bits(), 0);
        assert_eq!(info.flags & F::BARS_VALID.bits(), 0);
        // The signature is a validated newtype, so its claim always holds.
        assert_ne!(info.flags & F::TIME_SIG_VALID.bits(), 0);
    }

    /// The cycle's presence is `kVstTransportCycleActive` and `kVstCyclePosValid`
    /// together, and its bounds are the loop's; with none, both are clear and
    /// the bounds are zero.
    /// Spec: vst2.timeinfo.kVstCyclePosValid
    #[test]
    fn the_cycle_sets_both_cycle_bits_and_its_bounds() {
        let on = build_vst2_time_info(
            &base().loop_region(cycle(4.0, 16.0)),
            SampleRate::SR_48K,
            None,
            AutomationMode::Off,
        );
        assert_ne!(on.flags & F::TRANSPORT_CYCLE_ACTIVE.bits(), 0);
        assert_ne!(on.flags & F::CYCLE_POS_VALID.bits(), 0);
        assert_eq!((on.cycle_start_pos, on.cycle_end_pos), (4.0, 16.0));

        let off = build_vst2_time_info(&base(), SampleRate::SR_48K, None, AutomationMode::Off);
        assert_eq!(off.flags & F::TRANSPORT_CYCLE_ACTIVE.bits(), 0);
        assert_eq!(off.flags & F::CYCLE_POS_VALID.bits(), 0);
        assert_eq!((off.cycle_start_pos, off.cycle_end_pos), (0.0, 0.0));
    }

    /// **The host clock and the MIDI clock reach `nanoSeconds` and
    /// `samplesToNextClock`, each under its own validity bit, and only when
    /// the snapshot has them.** 120 BPM at 48 kHz: a 24-ppq clock is 1 000
    /// frames, and a playhead a quarter clock past one is 250 frames after it.
    ///
    /// The distance is at the snapshot's rate, as VST3's is: an instance
    /// prepared at another rate does not rescale it.
    #[test]
    fn host_time_and_midi_clock_are_filled_and_flagged_when_known() {
        let transport = base()
            .tempo(Bpm::of(120.0))
            .beat(Beat::new(2.0 + 0.25 / 24.0))
            .position_samples(48_250)
            .system_time_ns(5_000_000_000);
        let info = build_vst2_time_info(&transport, SampleRate::SR_48K, None, AutomationMode::Off);
        assert_eq!(info.sample_pos, 48_250.0);
        assert_eq!(info.nanoseconds, 5_000_000_000.0);
        assert_ne!(info.flags & F::NANOSECONDS_VALID.bits(), 0);
        assert_eq!(info.samples_to_next_clock, -250);
        assert_ne!(info.flags & F::VST_CLOCK_VALID.bits(), 0);
        let other_rate =
            build_vst2_time_info(&transport, SampleRate::SR_96K, None, AutomationMode::Off);
        assert_eq!(other_rate.samples_to_next_clock, -250);

        // No host clock and no finite position: neither field is claimed.
        let bare = base().beat(Beat::new(f64::NAN));
        let info = build_vst2_time_info(&bare, SampleRate::SR_48K, None, AutomationMode::Off);
        assert_eq!(info.flags & F::NANOSECONDS_VALID.bits(), 0);
        assert_eq!(info.flags & F::VST_CLOCK_VALID.bits(), 0);
        assert_eq!(info.nanoseconds, 0.0);
        assert_eq!(info.samples_to_next_clock, 0);
    }

    /// A piece after the first measures `samplesToNextClock` at its own position: at 120 BPM
    /// and 48 kHz a clock is 1 000 frames, so 292 frames past one the nearest is 292 behind
    /// it. A snapshot with no usable tempo has no grid, and `kVstClockValid` is cleared
    /// rather than the stale figure kept.
    /// Spec: vst2.timeinfo.kVstClockValid
    #[test]
    fn advancing_measures_the_clock_again_or_stops_claiming_it() {
        let flags =
            (F::TRANSPORT_PLAYING | F::PPQ_POS_VALID | F::TEMPO_VALID | F::VST_CLOCK_VALID).bits();
        let at = |tempo: f64| vst::TimeInfo {
            sample_rate: 48_000.0,
            ppq_pos: 4.0 + 100.0 / 24_000.0,
            tempo,
            samples_to_next_clock: -100,
            flags,
            ..Default::default()
        };

        let moved = advance_time_info(at(120.0), 192, SampleRate::SR_48K);
        assert_eq!(moved.samples_to_next_clock, -292);
        assert_ne!(moved.flags & F::VST_CLOCK_VALID.bits(), 0);

        let unusable = advance_time_info(at(0.0), 192, SampleRate::SR_48K);
        assert_eq!(unusable.flags & F::VST_CLOCK_VALID.bits(), 0);
    }
}
