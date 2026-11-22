//! Boxed bulk payload for the `ProcessAudio` wire message.

use serde::{Deserialize, Serialize};

#[cfg(test)]
use tutti_plugin_types::fake::{at_frame, unit_value};
use tutti_plugin_types::{MidiEventVec, SampleRate};

use tutti_plugin_types::{
    ChordChanges, NoteExpressionChanges, NoteExpressionIntChanges, NoteExpressionTextChanges,
    ParameterChanges, ScaleChanges, TransportChanges, TransportInfo,
};

/// One process block's inputs, except the audio itself.
///
/// Carries which block this is, the sample count, and all the per-block
/// side-band (MIDI, automation, note expression, VST3 sequencer context,
/// transport). Audio travels in the shared `AudioSlab`; everything here rides
/// the control socket alongside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessAudioData {
    /// Which block this is. Monotonic per plugin instance from 1, and the index
    /// into the slab's ring: the server reads `seq`'s input slot and publishes
    /// into `seq`'s output slot, so both sides address the shared region by this
    /// number alone. `u64` is wide enough that it never wraps, so a reply can
    /// never be matched to the wrong block.
    pub seq: u64,
    /// Block length in **frames**, not samples — the slab's stride carries the
    /// channel count.
    pub num_samples: usize,
    /// MIDI to deliver at the top of the block: at most
    /// `MIDI_STACK_CAPACITY` from the host; the server
    /// refuses a longer list at decode.
    #[serde(with = "super::wire::midi")]
    pub midi_events: MidiEventVec,
    /// Sample-accurate automation points to apply across the block.
    pub param_changes: ParameterChanges,
    /// Per-note expression (pitch, pressure, brightness) for the block.
    pub note_expression: NoteExpressionChanges,
    /// VST3 sequencer-context chords. VST3-only; other formats ignore it.
    pub chords: ChordChanges,
    /// VST3 sequencer-context scales. VST3-only; other formats ignore it.
    pub scales: ScaleChanges,
    /// VST3 per-note text expression. VST3-only; other formats ignore it.
    pub expr_texts: NoteExpressionTextChanges,
    /// VST3 per-note integer expression. VST3-only; other formats ignore it.
    pub expr_ints: NoteExpressionIntChanges,
    /// Tempo, time signature and play state at the block's first frame.
    pub transport: TransportInfo,
    /// The transport changes inside the block, in time order, each at its
    /// frame (never frame 0: that is `transport`). At most
    /// `MAX_TRANSPORT_CHANGES`, inline; a longer list is a decode error.
    pub transport_changes: TransportChanges,
}

impl ProcessAudioData {
    /// A block of no frames and no events, with a stopped transport at
    /// `sample_rate`: what a block's fields are overwritten from.
    #[must_use]
    pub fn empty(sample_rate: SampleRate) -> Self {
        Self {
            seq: 0,
            num_samples: 0,
            midi_events: MidiEventVec::new(),
            param_changes: ParameterChanges::new(),
            note_expression: NoteExpressionChanges::new(),
            chords: ChordChanges::new(),
            scales: ScaleChanges::new(),
            expr_texts: NoteExpressionTextChanges::new(),
            expr_ints: NoteExpressionIntChanges::new(),
            transport: TransportInfo::new(sample_rate),
            transport_changes: TransportChanges::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::{TransportChange, WireMidi};
    use tutti_ump::Ump;
    use tutti_units::Bpm;

    fn info() -> TransportInfo {
        TransportInfo::new(SampleRate::SR_48K)
    }

    /// A block with every list populated: MIDI,
    /// automation on two parameters, note expression (a named and a custom
    /// dimension), a chord, a scale, a note-expression text and an integer
    /// expression.
    fn full_block() -> ProcessAudioData {
        use tutti_plugin_types::{
            ChordValue, NoteExpressionIntValue, NoteExpressionTextValue, NoteExpressionType,
            NoteExpressionValue, ScaleValue,
        };
        use tutti_units::ParamId;
        let mut param_changes = ParameterChanges::reserved(2, 2);
        param_changes.add_change(ParamId::new(3), at_frame(0), unit_value(0.25));
        param_changes.add_change(ParamId::new(3), at_frame(32), unit_value(0.75));
        param_changes.add_change(ParamId::new(0xDEAD_BEEFu32), at_frame(16), unit_value(1.0));
        let mut note_expression = NoteExpressionChanges::new();
        note_expression.add_change(NoteExpressionValue {
            sample_offset: 4,
            note_id: 7,
            expression_type: NoteExpressionType::Tuning,
            value: 0.5,
        });
        note_expression.add_change(NoteExpressionValue {
            sample_offset: 9,
            note_id: 8,
            expression_type: NoteExpressionType::Custom(0x1234),
            value: 0.125,
        });
        let mut chords = ChordChanges::new();
        let mut scales = ScaleChanges::new();
        let mut expr_texts = NoteExpressionTextChanges::new();
        let mut expr_ints = NoteExpressionIntChanges::new();
        let kept = [
            chords.add_change(ChordValue {
                sample_offset: 0,
                root: 60,
                bass_note: 48,
                mask: 0b1001_0001,
                text: "Cmaj".into(),
            }),
            scales.add_change(ScaleValue {
                sample_offset: 8,
                root: 2,
                mask: 0x0AB5,
                text: "D major".into(),
            }),
            expr_texts.add_change(NoteExpressionTextValue {
                sample_offset: 12,
                note_id: 7,
                type_id: 5,
                text: "ah".into(),
            }),
            expr_ints.add_change(NoteExpressionIntValue {
                sample_offset: 20,
                note_id: 7,
                type_id: 6,
                value: -1234,
            }),
        ];
        assert!(kept.iter().all(|k| *k), "each container has room");
        ProcessAudioData {
            seq: 42,
            num_samples: 64,
            midi_events: [
                WireMidi {
                    offset: 0,
                    ump: Ump([0x2090_3C40, 0, 0, 0]),
                },
                WireMidi {
                    offset: 63,
                    ump: Ump([0x2080_3C00, 0, 0, 0]),
                },
            ]
            .into_iter()
            .collect(),
            param_changes,
            note_expression,
            chords,
            scales,
            expr_texts,
            expr_ints,
            transport: info().system_time_ns(0x0102_0304_0506_0708),
            transport_changes: [TransportChange {
                offset: 17,
                transport: info()
                    .system_time_ns(0x0102_0304_0506_0708)
                    .tempo(Bpm::of(90.0)),
            }]
            .into_iter()
            .collect(),
        }
    }

    /// **A fully populated block crosses the wire whole**: a host and a server
    /// exchange the same `ProcessAudio` body, note expressions and texts
    /// included. `ProcessAudioData` has no `PartialEq`, so the decode is
    /// checked by re-encoding to the same bytes and by reading back its fields.
    #[test]
    fn a_full_block_round_trips_the_wire() {
        let bytes = postcard::to_allocvec(&full_block()).unwrap();
        let back: ProcessAudioData = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(postcard::to_allocvec(&back).unwrap(), bytes);
        assert_eq!(back.midi_events.len(), 2);
        assert_eq!(back.midi_events[1].offset, 63);
        assert_eq!(back.param_changes.queues.len(), 2);
        assert_eq!(back.param_changes.queues[0].points.len(), 2);
        assert_eq!(back.note_expression.changes.len(), 2);
        assert_eq!(
            back.note_expression.changes[1].expression_type,
            tutti_plugin_types::NoteExpressionType::Custom(0x1234)
        );
        assert_eq!(back.chords.changes[0].text, "Cmaj");
        assert_eq!(back.scales.changes[0].text, "D major");
        assert_eq!(back.expr_texts.changes[0].text, "ah");
        assert_eq!(back.expr_ints.changes[0].value, -1234);
        assert_eq!(back.transport.system_time_ns, Some(0x0102_0304_0506_0708));
        assert_eq!(back.transport_changes.len(), 1);
        assert_eq!(back.transport_changes[0].offset, 17);
        assert_eq!(
            back.transport_changes[0].transport.timing.tempo,
            Bpm::of(90.0)
        );
    }

    /// **The transport's project-time position and host clock cross the
    /// wire**: the plugin server fills VST3 `projectTimeSamples` /
    /// `systemTime` and VST2 `samplePos` / `nanoSeconds` from what arrives.
    #[test]
    fn the_transport_clocks_cross_the_wire() {
        let block = ProcessAudioData {
            transport: info()
                .position_samples(96_007)
                .system_time_ns(5_001_000_000),
            ..ProcessAudioData::empty(SampleRate::SR_48K)
        };
        let back: ProcessAudioData =
            postcard::from_bytes(&postcard::to_allocvec(&block).unwrap()).unwrap();
        assert_eq!(back.transport.position.samples, Some(96_007));
        assert_eq!(back.transport.system_time_ns, Some(5_001_000_000));
    }

    /// **A cycle crosses the wire whole**: its beat range and its seconds.
    #[test]
    fn a_cycle_with_its_seconds_crosses_the_wire() {
        let region = tutti_plugin_types::LoopRegion {
            seconds: Some((
                tutti_plugin_types::Seconds::new(1.0),
                tutti_plugin_types::Seconds::new(4.5),
            )),
            ..tutti_plugin_types::LoopRegion::new(
                tutti_music::LoopRange::new(
                    tutti_units::Beat::new(2.0),
                    tutti_units::Beat::new(8.0),
                )
                .expect("a loop"),
            )
        };
        let block = ProcessAudioData {
            transport: info().loop_region(region),
            ..ProcessAudioData::empty(SampleRate::SR_48K)
        };
        let back: ProcessAudioData =
            postcard::from_bytes(&postcard::to_allocvec(&block).unwrap()).unwrap();
        assert_eq!(back.transport.loop_region, Some(region));
        assert_eq!(
            ProcessAudioData::empty(SampleRate::SR_48K)
                .transport
                .loop_region,
            None
        );
    }

    /// **The tempo and its slope cross the wire**: the plugin server hands
    /// CLAP's `tempo_inc` the host's figure.
    #[test]
    fn the_tempo_slope_crosses_the_wire() {
        let block = ProcessAudioData {
            transport: info().tempo(Bpm::of(93.5)).tempo_inc(-0.000_125),
            ..ProcessAudioData::empty(SampleRate::SR_48K)
        };
        let back: ProcessAudioData =
            postcard::from_bytes(&postcard::to_allocvec(&block).unwrap()).unwrap();
        assert_eq!(back.transport.timing.tempo, Bpm::of(93.5));
        assert_eq!(back.transport.timing.tempo_inc, -0.000_125);
    }
}

#[cfg(test)]
mod change_tests {
    use super::*;
    use tutti_plugin_types::TransportChange;
    use tutti_units::Bpm;

    fn info() -> TransportInfo {
        TransportInfo::new(SampleRate::SR_48K)
    }

    /// **The transport changes cross the wire, in order, each at its frame
    /// with its own tempo and slope**, up to the graph's bound; a list one
    /// longer is refused at decode (the server never grows one).
    #[test]
    fn the_transport_changes_cross_the_wire_in_order() {
        let mut block = ProcessAudioData::empty(SampleRate::SR_48K);
        for (offset, tempo, inc) in [(10, Bpm::of(100.0), 0.001), (50, Bpm::of(90.0), -0.002)] {
            block.transport_changes.push(TransportChange {
                offset,
                transport: info().tempo(tempo).tempo_inc(inc).playing(true),
            });
        }
        let back: ProcessAudioData =
            postcard::from_bytes(&postcard::to_allocvec(&block).unwrap()).unwrap();
        let got: Vec<_> = back
            .transport_changes
            .iter()
            .map(|c| {
                (
                    c.offset,
                    c.transport.timing.tempo,
                    c.transport.timing.tempo_inc,
                )
            })
            .collect();
        assert_eq!(
            got,
            [(10, Bpm::of(100.0), 0.001), (50, Bpm::of(90.0), -0.002)]
        );
        assert!(back.transport_changes[1].transport.state.playing);

        // One more than the bound: the encoding of nine is a decode error.
        let mut nine = postcard::to_allocvec(&ProcessAudioData::empty(SampleRate::SR_48K)).unwrap();
        let tail = nine.len() - 1;
        // The empty list is the last byte (a varint length of 0); say 9 and
        // append nine changes' worth of bytes taken from a real one.
        let one = postcard::to_allocvec(&TransportChange {
            offset: 1,
            transport: info(),
        })
        .unwrap();
        nine.truncate(tail);
        nine.push(9);
        for _ in 0..9 {
            nine.extend_from_slice(&one);
        }
        assert!(postcard::from_bytes::<ProcessAudioData>(&nine).is_err());
    }
}
