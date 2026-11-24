//! Audio/note port enumeration, configuration, and the render / voice /
//! surround / ambisonic extensions.

use super::ClapLoaded;
use super::load::layout_of_port;
use crate::types::{
    AudioPortInfo, NoteDialect, NoteDialects, NoteName, NotePortInfo, NoteSend, SurroundChannel,
    VoiceInfo,
};
use clap_sys::ext::ambisonic::{
    CLAP_AMBISONIC_NORMALIZATION_MAXN, CLAP_AMBISONIC_NORMALIZATION_N2D,
    CLAP_AMBISONIC_NORMALIZATION_N3D, CLAP_AMBISONIC_NORMALIZATION_SN2D,
    CLAP_AMBISONIC_NORMALIZATION_SN3D, CLAP_AMBISONIC_ORDERING_ACN, CLAP_AMBISONIC_ORDERING_FUMA,
    clap_ambisonic_config,
};
use clap_sys::ext::note_ports::{
    CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_MIDI_MPE,
    CLAP_NOTE_DIALECT_MIDI2,
};
use clap_sys::ext::voice_info::CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES;
use tutti_buffers::{AmbisonicNormalization, AmbisonicOrdering, ChannelTopology};
use tutti_plugin_types::MainThreadMarker;
use tutti_units::Samples;

use crate::TooManyNotePorts;

/// CLAP's constant for `ordering`.
const fn clap_ordering(ordering: AmbisonicOrdering) -> u32 {
    match ordering {
        AmbisonicOrdering::FuMa => CLAP_AMBISONIC_ORDERING_FUMA,
        AmbisonicOrdering::Acn => CLAP_AMBISONIC_ORDERING_ACN,
    }
}

/// CLAP's constant for `normalization`, or `None` for Furse-Malham scaling,
/// which `ambisonic.h` has no constant for.
const fn clap_normalization(normalization: AmbisonicNormalization) -> Option<u32> {
    match normalization {
        AmbisonicNormalization::MaxN => Some(CLAP_AMBISONIC_NORMALIZATION_MAXN),
        AmbisonicNormalization::Sn3d => Some(CLAP_AMBISONIC_NORMALIZATION_SN3D),
        AmbisonicNormalization::N3d => Some(CLAP_AMBISONIC_NORMALIZATION_N3D),
        AmbisonicNormalization::Sn2d => Some(CLAP_AMBISONIC_NORMALIZATION_SN2D),
        AmbisonicNormalization::N2d => Some(CLAP_AMBISONIC_NORMALIZATION_N2D),
        AmbisonicNormalization::FuMa => None,
    }
}

/// The ordering and normalization CLAP's constants `ordering` and
/// `normalization` name, or `None` when either is not one `ambisonic.h` defines.
fn ambisonic_of(
    ordering: u32,
    normalization: u32,
) -> Option<(AmbisonicOrdering, AmbisonicNormalization)> {
    let ordering = match ordering {
        CLAP_AMBISONIC_ORDERING_FUMA => AmbisonicOrdering::FuMa,
        CLAP_AMBISONIC_ORDERING_ACN => AmbisonicOrdering::Acn,
        _ => return None,
    };
    let normalization = match normalization {
        CLAP_AMBISONIC_NORMALIZATION_MAXN => AmbisonicNormalization::MaxN,
        CLAP_AMBISONIC_NORMALIZATION_SN3D => AmbisonicNormalization::Sn3d,
        CLAP_AMBISONIC_NORMALIZATION_N3D => AmbisonicNormalization::N3d,
        CLAP_AMBISONIC_NORMALIZATION_SN2D => AmbisonicNormalization::Sn2d,
        CLAP_AMBISONIC_NORMALIZATION_N2D => AmbisonicNormalization::N2d,
        _ => return None,
    };
    Some((ordering, normalization))
}

impl ClapLoaded {
    /// Number of input or output audio ports exposed by the plugin.
    ///
    /// `u32` because that is what CLAP's own `count`/`get` pair speaks, and it
    /// is the type every port index in this file is denominated in. Widening
    /// the count to `usize` and narrowing the index back at the FFI call is a
    /// matched pair of conversions that buys nothing and leaves one concept
    /// with two spellings in one file.
    pub fn audio_port_count(&self, main: MainThreadMarker, is_input: bool) -> u32 {
        self.plugin.audio_port_count(main, is_input)
    }

    /// Metadata for the audio port at `index`, or `None` if the index is
    /// invalid, the plugin does not implement the extension, or the port
    /// reports more channels than a layout holds.
    pub fn audio_port_info(
        &self,
        main: MainThreadMarker,
        index: u32,
        is_input: bool,
    ) -> Option<AudioPortInfo> {
        self.plugin
            .audio_port_info(main, index, is_input)
            .and_then(|info| audio_port_info_from_ffi(&info))
    }

    /// Total channel count summed across one side's audio ports.
    ///
    /// Stops at the first index `get` rejects. A sum is order-free, so a hole
    /// misattributes nothing here — but this total must describe the same
    /// truncated list [`port_channels`](super::load) presents, or it stops
    /// matching `PortLayout::{input,output}_channel_total`, which is what sizes
    /// the process scratch.
    fn channel_total(&self, main: MainThreadMarker, is_input: bool) -> usize {
        let count = self.audio_port_count(main, is_input);
        let mut total = 0usize;
        for i in 0..count {
            let Some(port) = self.audio_port_info(main, i, is_input) else {
                break;
            };
            total += port.layout.count() as usize;
        }
        total
    }

    /// Total input channel count, summed across every input port.
    pub fn num_input_channels(&self, main: MainThreadMarker) -> usize {
        self.channel_total(main, true)
    }

    /// Total output channel count, summed across every output port.
    pub fn num_output_channels(&self, main: MainThreadMarker) -> usize {
        self.channel_total(main, false)
    }

    /// Number of input or output note (MIDI) ports.
    pub fn note_port_count(&self, main: MainThreadMarker, is_input: bool) -> u32 {
        self.plugin.note_port_count(main, is_input)
    }

    /// What the first input note port is sent, as [`note_sends`](Self::note_sends)
    /// answers for it; CLAP note events alone for a plugin with no input note
    /// port.
    pub fn note_send(&self, main: MainThreadMarker) -> NoteSend {
        if self.note_port_count(main, true) == 0 {
            return NoteSend::CLAP_ONLY;
        }
        self.note_send_at(main, 0)
    }

    /// What input note port `index` is sent; nothing for one the plugin will
    /// not describe.
    fn note_send_at(&self, main: MainThreadMarker, index: u32) -> NoteSend {
        self.note_port_info(main, index, true)
            .map_or(NoteSend::Nothing, |port| port.dialect_to_send())
    }

    /// What each input note port is sent. Empty for a plugin with no input
    /// note port. A port the plugin declares but will not describe is sent
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`TooManyNotePorts`] when the plugin declares more than 16 (one for each
    /// UMP Group), before any port is read.
    pub fn note_sends(&self, main: MainThreadMarker) -> Result<Vec<NoteSend>, TooManyNotePorts> {
        let count = input_note_ports(self.note_port_count(main, true))?;
        Ok((0..count)
            .map(|index| self.note_send_at(main, index))
            .collect())
    }

    /// Metadata for the note port at `index`, including supported dialects.
    pub fn note_port_info(
        &self,
        main: MainThreadMarker,
        index: u32,
        is_input: bool,
    ) -> Option<NotePortInfo> {
        let info = self.plugin.note_port_info(main, index, is_input)?;

        let preferred_dialect = match info.preferred_dialect {
            CLAP_NOTE_DIALECT_CLAP => Some(NoteDialect::Clap),
            CLAP_NOTE_DIALECT_MIDI => Some(NoteDialect::Midi),
            CLAP_NOTE_DIALECT_MIDI_MPE => Some(NoteDialect::MidiMpe),
            CLAP_NOTE_DIALECT_MIDI2 => Some(NoteDialect::Midi2),
            _ => None,
        };

        Some(NotePortInfo {
            id: info.id,
            name: info.name,
            supported_dialects: NoteDialects::from_bits_truncate(info.supported_dialects),
            preferred_dialect,
        })
    }

    /// Plugin-reported processing latency in samples. 0 when unsupported.
    pub fn latency_samples(&self, main: MainThreadMarker) -> Samples {
        self.plugin.latency(main)
    }

    /// Plugin-reported tail length in samples (audio continues after input
    /// stops — reverbs, delays), or `None` when the plugin does not implement
    /// `clap.tail`: no extension means the tail is unknown, which is not the
    /// same answer as `Some(0)`, which means no tail. Raw: any value at or
    /// above `INT32_MAX` means an infinite tail (`clap/ext/tail.h`), which
    /// `Tail::from_clap_samples` decodes.
    pub fn tail_samples(&self, main: MainThreadMarker) -> Option<u32> {
        self.plugin.tail(main)
    }

    /// Whether the plugin requires a hard real-time environment (e.g. it
    /// talks to hardware). Hosts should avoid running such plugins offline.
    #[must_use]
    pub fn has_hard_realtime_requirement(&self, main: MainThreadMarker) -> bool {
        self.plugin.has_hard_realtime_requirement(main)
    }

    /// Voice-allocation information from `CLAP_EXT_VOICE_INFO`.
    pub fn voice_info(&self, main: MainThreadMarker) -> Option<VoiceInfo> {
        let info = self.plugin.voice_info(main)?;
        Some(VoiceInfo {
            voice_count: info.voice_count,
            voice_capacity: info.voice_capacity,
            supports_overlapping_notes: (info.flags & CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES)
                != 0,
        })
    }

    /// Number of custom note names (e.g. drum-kit labels) the plugin
    /// exposes via `CLAP_EXT_NOTE_NAME`.
    pub fn note_name_count(&self, main: MainThreadMarker) -> u32 {
        self.plugin.note_name_count(main)
    }

    /// Retrieves a single custom note name.
    pub fn note_name(&self, main: MainThreadMarker, index: u32) -> Option<NoteName> {
        let info = self.plugin.note_name(main, index)?;
        Some(NoteName {
            name: info.name,
            port: info.port,
            channel: info.channel,
            key: info.key,
        })
    }

    /// The plugin's key names for the first note port, as the shared type: a name
    /// for every port or for port 0, and for one key (`-1`, every key, names no
    /// key).
    pub(crate) fn declared_note_names(
        &self,
        main: MainThreadMarker,
    ) -> Vec<tutti_plugin_types::NoteName> {
        (0..self.note_name_count(main))
            .filter_map(|index| self.note_name(main, index))
            .filter(|n| matches!(n.port, -1 | 0))
            .filter_map(|n| {
                Some(tutti_plugin_types::NoteName {
                    key: u8::try_from(n.key)
                        .ok()
                        .and_then(tutti_ump::NoteNumber::new)?,
                    channel: u8::try_from(n.channel)
                        .ok()
                        .and_then(tutti_ump::MidiChannel::new),
                    text: n.name,
                })
            })
            .collect()
    }

    /// Asks whether the plugin can process the given ambisonic ordering +
    /// normalization. Returns false when `CLAP_EXT_AMBISONIC` is unsupported,
    /// and for Furse-Malham normalization, which CLAP has no constant for.
    #[must_use]
    pub fn is_ambisonic_config_supported(
        &self,
        main: MainThreadMarker,
        ordering: AmbisonicOrdering,
        normalization: AmbisonicNormalization,
    ) -> bool {
        let Some(normalization) = clap_normalization(normalization) else {
            return false;
        };
        let config = clap_ambisonic_config {
            ordering: clap_ordering(ordering),
            normalization,
        };
        self.plugin.is_ambisonic_config_supported(main, &config)
    }

    /// Retrieves the ambisonic ordering and normalization currently used on a
    /// given port. `None` when the plugin has no `CLAP_EXT_AMBISONIC`, or
    /// names an ordering or normalization this crate has no name for.
    pub fn ambisonic_config(
        &self,
        main: MainThreadMarker,
        is_input: bool,
        port_index: u32,
    ) -> Option<(AmbisonicOrdering, AmbisonicNormalization)> {
        let config = self.plugin.ambisonic_config(main, is_input, port_index)?;
        ambisonic_of(config.ordering, config.normalization)
    }

    /// Asks whether the plugin can process the given
    /// [`SurroundChannel`]-bit channel mask (`CLAP_EXT_SURROUND`).
    #[must_use]
    pub fn is_surround_channel_mask_supported(
        &self,
        main: MainThreadMarker,
        channel_mask: u64,
    ) -> bool {
        self.plugin
            .is_surround_channel_mask_supported(main, channel_mask)
    }

    /// Retrieves the channel-to-speaker mapping the plugin uses on a port.
    ///
    /// The result is indexed **by channel**: element `i` is the speaker fed by
    /// channel `i`, so it always has `count` elements. A position this crate
    /// cannot name arrives as [`SurroundChannel::Unknown`] rather than being
    /// dropped — omitting it would renumber every channel after it.
    ///
    /// `None` means the plugin cannot answer — no `clap.surround` extension or
    /// no `get_channel_map`. An empty `Vec` means it answered with no channels:
    /// `get_channel_map` returns "the number of elements stored", so `0` is
    /// data, not failure. Only the capacity overrun is genuinely broken — the
    /// plugin claims to have written past the 64 elements it was given, so
    /// nothing in the buffer can be trusted.
    pub fn surround_channel_map(
        &self,
        main: MainThreadMarker,
        is_input: bool,
        port_index: u32,
    ) -> Option<Vec<SurroundChannel>> {
        let (map, count) = self
            .plugin
            .surround_channel_map(main, is_input, port_index)?;
        decode_surround_channel_map(&map, count)
    }

    /// The channel-to-speaker mapping for a port as a [`ChannelTopology`].
    ///
    /// The same query as
    /// [`surround_channel_map`](Self::surround_channel_map), in the
    /// shared vocabulary the other format hosts also speak — so a caller can
    /// ask "which speaker is channel 3" without knowing it is holding a CLAP
    /// plugin.
    ///
    /// `None` for the same reason: the plugin has no `clap.surround` extension
    /// or no `get_channel_map`, which is distinct from answering with no
    /// channels. Nothing is lost in the conversion — a position this crate
    /// cannot name arrives as [`Speaker::Unknown`](tutti_buffers::Speaker::Unknown)
    /// and keeps its channel's slot.
    pub(crate) fn surround_topology(
        &self,
        main: MainThreadMarker,
        is_input: bool,
        port_index: u32,
    ) -> Option<ChannelTopology> {
        self.surround_channel_map(main, is_input, port_index)
            .and_then(|map| crate::topology::topology_of(&map))
    }
}

/// Decodes the first `count` entries of a plugin-filled surround channel map.
///
/// Split out of [`ClapLoaded::surround_channel_map`] so the count
/// validation is testable without a live `ClapLoaded`, which no stub vtable can
/// produce.
fn decode_surround_channel_map(map: &[u8], count: usize) -> Option<Vec<SurroundChannel>> {
    // A count past the capacity we handed over is a plugin bug and makes every
    // element suspect — reject. `count == 0` is a real empty map.
    if count > map.len() {
        return None;
    }
    // `map`, not `filter_map`: the result is indexed by channel, so an
    // unnameable position must keep its slot. Filtering shortened the vector
    // and renumbered every channel after the dropped one.
    Some(
        map[..count]
            .iter()
            .map(|&pos| SurroundChannel::from_position(pos))
            .collect(),
    )
}

/// `None` for a port that reports more channels than a layout holds, as for a
/// port the plugin rejects.
fn audio_port_info_from_ffi(info: &tutti_clap_ffi::AudioPortInfo) -> Option<AudioPortInfo> {
    Some(AudioPortInfo {
        id: info.id,
        name: info.name.clone(),
        layout: layout_of_port(info).ok()?,
        flags: info.flags,
        in_place_pair_id: info.in_place_pair,
    })
}

#[cfg(test)]
mod surround_map_tests {
    use super::*;

    /// A plugin reporting **zero** channels is answering, not failing — a
    /// `count == 0 || count > map.len()` guard would fold that in with the error.
    #[test]
    fn zero_count_is_an_empty_map_not_a_failure() {
        let map = [0u8; 64];
        assert_eq!(
            decode_surround_channel_map(&map, 0),
            Some(Vec::new()),
            "a zero-length map is data the plugin returned, not a failure"
        );
    }

    /// A count past the capacity the host handed over is a genuine plugin bug:
    /// the plugin claims to have written more than it was given room for, so no
    /// element can be trusted.
    #[test]
    fn count_past_capacity_is_rejected() {
        let map = [0u8; 64];
        assert_eq!(decode_surround_channel_map(&map, 65), None);
    }

    /// The ordinary path still decodes, and stops at `count` rather than
    /// running to the end of the buffer.
    #[test]
    fn decodes_exactly_count_entries() {
        let mut map = [0u8; 64];
        map[0] = 0; // FrontLeft
        map[1] = 1; // FrontRight
        map[2] = 2; // FrontCenter — past `count`, must not appear
        let decoded = decode_surround_channel_map(&map, 2).expect("valid count");
        assert_eq!(
            decoded,
            vec![SurroundChannel::FrontLeft, SurroundChannel::FrontRight]
        );
    }

    /// Every position CLAP defines decodes, including the top-side pair.
    ///
    /// `CLAP_SURROUND_TSL`/`TSR` (18/19, `surround.h:55-56`) are the last two
    /// constants and the ones a 7.1.4 / Atmos bed actually uses. They were
    /// missing, which the next test shows is worse than a missing name.
    #[test]
    fn every_clap_position_decodes() {
        let map: Vec<u8> = (0..=19).collect();
        let decoded = decode_surround_channel_map(&map, map.len()).expect("valid count");
        assert_eq!(
            decoded.len(),
            20,
            "CLAP defines positions 0..=19; a short result means one was dropped"
        );
        assert_eq!(decoded[18], SurroundChannel::TopSideLeft);
        assert_eq!(decoded[19], SurroundChannel::TopSideRight);
    }

    /// An unknown position keeps its channel's slot instead of collapsing it.
    ///
    /// The map is **positional** — `map[i]` is the speaker fed by channel `i` —
    /// so dropping an entry silently renumbers every channel after it. That is
    /// the failure this guards: a 12-channel bed whose channel 4 is unreadable
    /// must not report channel 5's speaker as channel 4's.
    ///
    /// A future CLAP revision adding position 20 is the realistic source of an
    /// unknown value, which is why this is `Unknown(u8)` rather than an error:
    /// one unnameable speaker should not discard the eleven that decoded.
    #[test]
    fn an_unknown_position_holds_its_slot_rather_than_shifting_the_rest() {
        // Positions 0..=3 then an unallocated one, then 5.
        let map = [0u8, 1, 2, 3, 200, 5];
        let decoded = decode_surround_channel_map(&map, 6).expect("valid count");

        assert_eq!(
            decoded.len(),
            6,
            "one unreadable position must not shorten a positional map"
        );
        assert_eq!(decoded[4], SurroundChannel::Unknown(200));
        assert_eq!(
            decoded[5],
            SurroundChannel::BackRight,
            "channel 5 must still report its own speaker, not channel 4's"
        );
    }
}

#[cfg(test)]
mod ambisonic_tests {
    use super::*;

    /// A raw ordering or normalization `ambisonic.h` does not define is not
    /// reported as some other one.
    #[test]
    fn an_unknown_raw_ambisonic_value_is_none() {
        let known = (
            CLAP_AMBISONIC_ORDERING_ACN,
            CLAP_AMBISONIC_NORMALIZATION_SN3D,
        );
        assert_eq!(
            ambisonic_of(known.0, known.1),
            Some((AmbisonicOrdering::Acn, AmbisonicNormalization::Sn3d))
        );
        assert_eq!(ambisonic_of(u32::MAX, known.1), None, "ordering");
        assert_eq!(ambisonic_of(known.0, u32::MAX), None, "normalization");
    }

    /// CLAP has no Furse-Malham normalization constant, so asking for one is
    /// unsupported, while every normalization it does define maps.
    #[test]
    fn fuma_normalization_has_no_clap_constant() {
        assert_eq!(clap_normalization(AmbisonicNormalization::FuMa), None);
        for n in [
            AmbisonicNormalization::MaxN,
            AmbisonicNormalization::Sn3d,
            AmbisonicNormalization::N3d,
            AmbisonicNormalization::Sn2d,
            AmbisonicNormalization::N2d,
        ] {
            let raw = clap_normalization(n).expect("CLAP defines it");
            assert_eq!(
                ambisonic_of(CLAP_AMBISONIC_ORDERING_ACN, raw).map(|(_, n)| n),
                Some(n)
            );
        }
    }
}

/// The input note ports a plugin declares, when each has a UMP Group to be
/// named by: refused before any port is read, so a count the plugin makes up
/// is never walked.
pub(crate) fn input_note_ports(count: u32) -> Result<u32, TooManyNotePorts> {
    if count > u32::from(tutti_ump::MidiGroup::COUNT) {
        return Err(TooManyNotePorts(count as usize));
    }
    Ok(count)
}

#[cfg(test)]
mod note_port_count_tests {
    use super::*;

    /// **A declared count of input note ports past the 16 Groups is refused
    /// from the number alone**, whatever it is (up to `u32::MAX`), and up to 16
    /// is taken as it is.
    #[test]
    fn a_declared_count_past_sixteen_is_refused_before_any_port_is_read() {
        for count in [0, 1, 16] {
            assert_eq!(input_note_ports(count), Ok(count));
        }
        for count in [17, 1 << 20, u32::MAX] {
            assert_eq!(
                input_note_ports(count),
                Err(TooManyNotePorts(count as usize))
            );
        }
    }
}
