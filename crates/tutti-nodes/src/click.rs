//! Metronome click node — click sounds synced to the transport.
//!
//! The click node is a graph node: it reads the playhead
//! **per frame** from its block's [`Env`] — walked piece by piece across the
//! block's transport changes, with the host's own clock
//! ([`Env::for_each_beat`](tutti_graph::Env::for_each_beat)) — the play and record state from
//! the same pieces, the count-in flag off the same transport
//! ([`TransportPosition::count_in`](tutti_graph::TransportPosition::count_in)), the
//! project's meter from [`Cx::meter`](tutti_graph::Cx#structfield.meter), and its own settings (volume, mode) from
//! [`ClickSettings`].
//!
//! # Why the beat comes from `Env`, not the transport's atomic
//!
//! The transport's `beat` atomic is the clock's *writeback*: stored once per
//! block, as the beat the **next** block starts on. A node reading it gets
//! one value per block, so a click could only ever start on a block boundary
//! — up to a whole block of jitter on every beat. The
//! block's `Env` carries the transport at its first frame and every change
//! inside it (a start, a stop, a seek, a tempo or loop edit, each on its
//! frame), so the node computes the beat of every frame — a loop wrap or a
//! seek landing mid-block included — and an onset starts on the exact frame
//! whose beat first reaches it. A start or a stop inside a block gates the
//! click on its frame too, not at the block's start.
//!
//! # The meter is lent by the block
//!
//! The host lends the project's [`MeterMap`] once per block through
//! [`Cx::meter`](tutti_graph::Cx#structfield.meter). The node reads it at every beat it walks, so a loop wrap and
//! any number of meter changes inside a block count bars exactly, and the
//! accent is the bar's downbeat under the meter the host played. A block with
//! no meter counts 4/4 from beat 0.

use atomic_float::AtomicF64;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tutti_buffers::BusTopology;
use tutti_buffers::Float;
use tutti_dsp::Metronome;
use tutti_graph::{
    Cx, Env, ForkCause, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare, RenderMode,
    Shape, Status, declare_params,
};
use tutti_music::MeterMap;
use tutti_rt::{AtomicCell, atomic_word};
use tutti_units::Amplitude;
use tutti_units::Tail;

/// Metronome operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u8)]
pub enum MetronomeMode {
    /// Metronome is disabled.
    #[default]
    Off,
    /// Only play during preroll count-in.
    PrerollOnly,
    /// Only play while recording.
    RecordingOnly,
    /// Always play when transport is running.
    Always,
}

atomic_word!(MetronomeMode {
    Off,
    PrerollOnly,
    RecordingOnly,
    Always
});

impl core::fmt::Display for MetronomeMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::PrerollOnly => "preroll only",
            Self::RecordingOnly => "recording only",
            Self::Always => "always",
        })
    }
}

tutti_units::ranged! {
    /// The click's level: a linear [`Amplitude`] (not dB) within `0.0..=1.0`.
    ///
    /// [`new`](Self::new) refuses NaN, infinity and anything outside the range,
    /// and never clamps.
    ClickLevel(Amplitude), ClickLevelError, "a click level, finite and within 0.0..=1.0",
    |raw| raw.get() >= 0.0 && raw.get() <= 1.0
}

impl ClickLevel {
    /// Half level, a click's default.
    pub const HALF: ClickLevel = ClickLevel(Amplitude::new(0.5));
}

/// Click-specific settings (volume, mode).
///
/// Transport state (beat, playing, recording, count-in, meter) comes from the
/// node's block [`Env`].
///
/// **Shared as `Arc<ClickSettings>`, never cloned by value**: it is the
/// node's controls ([`ClickNode`]'s `IntoNode`), the one cell a host's writes
/// and the running node both hold. A fork of the node gets a fresh one holding
/// the values at the fork ([`snapshot`](Self::snapshot)).
#[repr(align(64))]
#[derive(Debug)]
pub struct ClickSettings {
    volume: AtomicF64,
    /// When the metronome sounds: read once per block, set from the control
    /// side through [`mode`](Self::mode).
    mode: AtomicCell<MetronomeMode>,
}

declare_params!([] ClickSettings, settings ClickConfig {
    mode: MetronomeMode = MetronomeMode::Off,
});

impl From<ClickConfig> for ClickSettings {
    fn from(config: ClickConfig) -> Self {
        Self {
            volume: AtomicF64::new(ClickLevel::HALF.get().get()),
            mode: AtomicCell::new(config.mode),
        }
    }
}

impl ClickSettings {
    /// Default settings: mode [`MetronomeMode::Off`], half volume.
    #[must_use]
    pub fn new() -> Self {
        ClickConfig::new().into()
    }

    /// Stores the click level; the audio thread reads it at the next block.
    pub fn set_volume(&self, level: ClickLevel) {
        self.volume.store(level.get().get(), Ordering::Release);
    }

    /// The click level.
    pub fn volume(&self) -> ClickLevel {
        // Only `set_volume` and `new` write the cell, with a `ClickLevel`.
        ClickLevel(Amplitude::new(self.volume.load(Ordering::Acquire)))
    }

    /// A fresh cell holding this one's volume and mode now, sharing no cell
    /// with it: what a fork of the metronome renders with. Control thread: it
    /// allocates.
    #[must_use]
    pub fn snapshot(&self) -> ClickSettings {
        let fresh = ClickSettings::new();
        fresh
            .volume
            .store(self.volume.load(Ordering::Acquire), Ordering::Release);
        fresh.mode().store(self.mode().load());
        fresh
    }
}

impl Default for ClickSettings {
    fn default() -> Self {
        Self::new()
    }
}

/// Alias for [`ClickSettings`], for callers that name the metronome's shared
/// cell as state rather than settings.
pub type ClickState = ClickSettings;

/// The metronome, as a graph node.
///
/// No inputs; the click, stereo, on its two outputs. The beat of each frame
/// comes from the block's [`Env`] (see the module docs), so each onset
/// starts on its exact frame and nothing has to be wired into the node.
///
/// Its controls are its [`ClickSettings`] (`IntoNode::Controls`), the cell it
/// was built with. Playing, recording, the count-in (`PrerollOnly`,
/// `RecordingOnly`) and the meter are the block's, piece by piece, so an
/// export's metronome follows the export's transport.
///
/// A fork (an export, a live duplicate) renders the metronome as it was set
/// at the fork: a [`snapshot`](ClickSettings::snapshot) of the settings,
/// sharing nothing with the live node.
///
/// A clone reads the same settings.
#[derive(Clone)]
pub struct ClickNode<F: Float> {
    settings: Arc<ClickSettings>,
    /// The two clicks and the beat latch.
    metronome: Metronome<F>,
}

impl<F: Float> std::fmt::Debug for ClickNode<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickNode")
            .field("settings", &self.settings)
            .field("metronome", &self.metronome)
            .finish()
    }
}

/// What a [`ClickNode`] is made from: the [`ClickSettings`] its controls are.
#[derive(Clone, Debug)]
pub struct ClickNodeConfig {
    settings: Arc<ClickSettings>,
}

impl ClickNodeConfig {
    /// A click node over `settings`.
    ///
    /// `settings` is taken as an `Arc` so a later write to it reaches the running node: they
    /// are its controls.
    #[must_use]
    pub fn new(settings: Arc<ClickSettings>) -> Self {
        Self { settings }
    }
}

/// The click node of `config`. Its click waveforms are rendered when it is prepared, at the
/// graph's rate.
impl<F: Float> From<ClickNodeConfig> for ClickNode<F> {
    fn from(config: ClickNodeConfig) -> Self {
        Self::over(config.settings)
    }
}

impl<F: Float> ClickNode<F> {
    /// A node over `settings`.
    fn over(settings: Arc<ClickSettings>) -> Self {
        Self {
            settings,
            metronome: Metronome::new(),
        }
    }

    /// Whether the metronome sounds under `mode`, over a piece of a block
    /// whose transport is `t`.
    #[inline]
    fn should_play(mode: MetronomeMode, t: &tutti_graph::TransportPosition) -> bool {
        match mode {
            MetronomeMode::Off => false,
            MetronomeMode::Always => t.playing,
            MetronomeMode::PrerollOnly => t.count_in(),
            MetronomeMode::RecordingOnly => t.recording() && !t.count_in(),
        }
    }

    /// Render `env`'s block into `left` and `right`.
    ///
    /// The mode and the volume are read once per block — a UI write lands at
    /// the next block, never mid-block. The beat and the meter are read per
    /// frame, and the play, record and count-in state per piece of the block
    /// ([`Env::segments`]): a stop inside the block silences the click from
    /// its frame, and a start sounds the beat it lands on, on its frame.
    fn render(&mut self, env: &Env, meter: Option<&MeterMap>, left: &mut [F], right: &mut [F]) {
        let mode = self.settings.mode().load();
        // The level narrows to the render's format once per block.
        let volume = F::from_unit(self.settings.volume().get());
        self.metronome.begin_block();
        for (start, piece) in env.segments() {
            if !Self::should_play(mode, &piece.transport) {
                self.metronome.silence();
                let range = start.index()..start.index() + piece.block_len.get();
                left[range.clone()].fill(F::ZERO);
                right[range].fill(F::ZERO);
                continue;
            }
            let metronome = &mut self.metronome;
            piece.for_each_piece_beat(start, |i, beat| {
                let sample = metronome.tick(meter, beat, volume);
                left[i] = sample;
                right[i] = sample;
            });
        }
    }
}

impl<F: Float> Node<F> for ClickNode<F> {
    /// No inputs, the click in stereo. Its sound depends on the transport
    /// rather than an input, so a render's end ends it: [`Tail::None`].
    fn shape(&self) -> Shape {
        Shape::source(BusTopology::STEREO)
            .tail(Tail::None)
            // A beat derived and compared per frame: measured 3.8 ns per
            // frame and channel, against a gain's 0.2 and `SvfFilterNode`'s
            // 7.2.
            .cost(tutti_graph::Cost::Moderate)
    }

    /// Render the two click waveforms at the prepared rate (control thread:
    /// this allocates), unless they already are.
    fn prepare(&mut self, p: &Prepare) {
        self.metronome.prepare(p.sample_rate());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let (_, mut outputs) = io.split();
        let mut channels = outputs.iter_mut();
        let (Some(left), Some(right)) = (channels.next(), channels.next()) else {
            return Status::Modified;
        };
        self.render(cx.env, cx.meter, left, right);
        Status::Modified
    }

    fn reset(&mut self) {
        self.metronome.silence();
    }
}

/// [`ClickNode`]'s fork source: the live node's settings, forked into a node
/// that shares none of them.
///
/// It holds the cell, not a node: a fork is a node built over a new cell
/// ([`ClickNode::over`]).
struct ClickFork {
    settings: Arc<ClickSettings>,
}

impl ClickFork {
    /// The fork source of `node`.
    fn of<F: Float>(node: &ClickNode<F>) -> Self {
        Self {
            settings: Arc::clone(&node.settings),
        }
    }

    /// The fork: the settings as they stand now, in a cell of its own, and
    /// nothing sounding (it is unprepared).
    fn node<F: Float>(&self) -> ClickNode<F> {
        ClickNode::over(Arc::new(self.settings.snapshot()))
    }
}

impl<F: Float> ForkSource<F> for ClickFork {
    fn fork(&self, _mode: RenderMode) -> Result<Forked<F>, ForkCause> {
        Ok(Forked::new(Box::new(self.node::<F>())))
    }
}

/// Inserted with its [`ClickSettings`] as its controls and a fork that
/// renders the metronome as it was set at the fork (see [`ClickNode`]).
impl<F: Float> IntoNode<F> for ClickNode<F> {
    type Controls = Arc<ClickSettings>;

    fn into_parts(self) -> NodeParts<Arc<ClickSettings>, F> {
        let controls = Arc::clone(&self.settings);
        let fork = ClickFork::of(&self);
        NodeParts::new(Box::new(self), controls, Some(Box::new(fork)), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::contract::prepared;
    use tutti_graph::{Offset, TransportChanges};
    use tutti_music::FrameClock;
    use tutti_music::{BeatsPerBar, MeterChange, MeterMap, NoteValue, TimeSignature};
    use tutti_units::Frame;
    use tutti_units::SampleRate;
    use tutti_units::{Beat, Bpm, Samples};

    /// A project saves a mode by name and reads it back; a name that is not a
    /// mode is refused, never read as `Off`.
    #[test]
    fn a_mode_round_trips_through_serde_and_an_unknown_one_is_refused() {
        for mode in [
            MetronomeMode::Off,
            MetronomeMode::PrerollOnly,
            MetronomeMode::RecordingOnly,
            MetronomeMode::Always,
        ] {
            let text = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<MetronomeMode>(&text).unwrap(), mode);
        }
        assert!(serde_json::from_str::<MetronomeMode>("\"Sometimes\"").is_err());
    }

    /// The settings cell gives back the mode last set, for each of the four.
    #[test]
    fn the_settings_cell_holds_each_mode() {
        let settings = ClickSettings::new();
        for mode in [
            MetronomeMode::PrerollOnly,
            MetronomeMode::RecordingOnly,
            MetronomeMode::Always,
            MetronomeMode::Off,
        ] {
            settings.mode().store(mode);
            assert_eq!(settings.mode().load(), mode);
        }
    }

    /// `ClickLevel` keeps the range-checked shape: `new` refuses and never
    /// clamps, both ends are taken.
    #[test]
    fn a_click_level_keeps_the_range_checked_shape() {
        use tutti_fixtures::{RangeContract, check_range_contract};
        check_range_contract(&RangeContract {
            new: ClickLevel::new,
            get: ClickLevel::get,
            of: Some(ClickLevel::of),
            valid: &[
                Amplitude::new(0.0),
                Amplitude::new(0.5),
                Amplitude::new(1.0),
            ],
            invalid: &[
                Amplitude::new(-0.001),
                Amplitude::new(1.001),
                Amplitude::new(f64::NAN),
                Amplitude::new(f64::INFINITY),
            ],
        });
    }

    const SR: f64 = 44_100.0;

    /// The session state a block is rendered under: playing, recording and
    /// counting in.
    #[derive(Clone, Copy)]
    struct Deck {
        playing: bool,
        recording: bool,
        count_in: bool,
    }

    const ROLLING: Deck = Deck {
        playing: true,
        recording: false,
        count_in: false,
    };
    const STOPPED: Deck = Deck {
        playing: false,
        recording: false,
        count_in: false,
    };

    /// A transport at `beat` that holds it for the whole block (the smallest
    /// tempo, whose step per frame underflows to zero), under `deck`: the
    /// constant beat a test positions the playhead at.
    fn held(deck: Deck, beat: f64) -> tutti_graph::TransportPosition {
        tutti_graph::TransportPosition::new(
            deck.playing,
            Bpm::of(f64::from_bits(1)),
            Beat::new(beat),
            None,
        )
        .with_recording(deck.recording)
        .counting_in(deck.count_in)
    }

    /// A block of `len` frames at `rate` under `transport`, no changes.
    fn env(rate: f64, len: usize, transport: tutti_graph::TransportPosition) -> Env {
        Env {
            frame: Frame::new(0),
            sample_rate: tutti_units::SampleRate::of(rate),
            block_len: Samples::new(len),
            transport,
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        }
    }

    /// One block under the meter `meter` (`None`: 4/4), as `[left, right]`.
    fn block_under(
        node: &mut ClickNode<f32>,
        env: &Env,
        meter: Option<&MeterMap>,
    ) -> [Vec<f32>; 2] {
        let frames = env.block_len.get();
        let (mut left, mut right) = (vec![0.0; frames], vec![0.0; frames]);
        node.render(env, meter, &mut left, &mut right);
        [left, right]
    }

    /// One block with no meter, as `[left, right]`.
    fn block(node: &mut ClickNode<f32>, env: &Env) -> [Vec<f32>; 2] {
        block_under(node, env, None)
    }

    /// One frame at `beat`, as a `[left, right]` pair.
    fn frame(node: &mut ClickNode<f32>, deck: Deck, beat: f64) -> [f32; 2] {
        let [l, r] = block(node, &env(SR, 1, held(deck, beat)));
        [l[0], r[0]]
    }

    fn make_click() -> (Arc<ClickSettings>, ClickNode<f32>) {
        let settings = Arc::new(ClickSettings::new());
        let node = prepared(
            ClickNode::from(ClickNodeConfig::new(Arc::clone(&settings))),
            tutti_units::SampleRate::of(SR),
            64,
        );
        (settings, node)
    }

    /// Stores a click level, as the session's volume target does after it
    /// has checked the range.
    fn set_volume(settings: &ClickSettings, level: Amplitude) {
        settings.set_volume(ClickLevel::of(level));
    }

    /// Plays the click of the notated beat at `beat` out, as a transport rolling through it
    /// would have: one held block longer than the click, then the beat is latched and silent.
    fn play_out(node: &mut ClickNode<f32>, beat: f64) {
        let _ = block(node, &env(SR, 4_096, held(ROLLING, beat)));
    }

    /// Whether any of `n` one-frame blocks at `beat` sounds.
    fn sounds_within(node: &mut ClickNode<f32>, deck: Deck, beat: f64, n: usize) -> bool {
        (0..n).any(|_| {
            let out = frame(node, deck, beat);
            out[0] != 0.0 || out[1] != 0.0
        })
    }

    #[test]
    fn an_always_click_is_silent_on_a_stopped_transport() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);

        let output = frame(&mut node, STOPPED, 0.0);
        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    /// A transport rolling from `start` so that frame `i` of the block is at
    /// beat `start + i/100` (at 44.1 kHz): 26 460 BPM.
    fn hundredths_from(start: f64) -> tutti_graph::TransportPosition {
        tutti_graph::TransportPosition::new(true, Bpm::of(26_460.0), Beat::new(start), None)
    }

    /// A meter change that lands *inside* a notated beat still clicks its
    /// downbeat, on its frame.
    ///
    /// Within a block the onset check skips `bar_at` while the playhead stays in
    /// the latched beat's span, so that span must end at the change rather than
    /// at the next onset of the old meter — a change always begins a bar, and
    /// here it begins one half-way through a quarter note. Driven as one block
    /// with the change mid-block, because the span is dropped at every block
    /// boundary: a frame-by-frame version of this test passes with or without
    /// the cap.
    #[test]
    fn a_mid_beat_meter_change_clicks_its_downbeat() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);
        set_volume(&settings, Amplitude::new(1.0));
        let map = MeterMap::new([
            MeterChange::new(Beat::new(0.0), TimeSignature::default()),
            MeterChange::new(
                Beat::new(2.5),
                TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH),
            ),
        ]);

        // Beat 2's click has already played out, so only the change can sound.
        play_out(&mut node, 2.0);

        // Frame `i` carries beat `2 + i/100`, so the change at 2.5 is frame 50.
        let [left, _] = block_under(&mut node, &env(SR, 64, hundredths_from(2.0)), Some(&map));

        assert_eq!(
            node.metronome.onset(),
            Some(Beat::new(2.5)),
            "the change's downbeat is a new onset"
        );
        assert!(node.metronome.is_accent(), "and it is a downbeat");
        let first_sound = left.iter().position(|&s| s != 0.0);
        // The click's first frame is exactly zero, so it sounds one frame after
        // its onset — see `click_onset_is_sample_accurate_within_the_block`.
        assert_eq!(first_sound, Some(51), "clicked on the change's frame");
    }

    /// Two meter changes inside one block are each accented on their own
    /// frame: the first at 2.5 (frame 50 of a block carrying beat `2 + i/100`)
    /// and the second at 2.75 (frame 75), the click of each beginning where
    /// its bar does. A click is silent on its onset frame (`sin(0)`), so the
    /// accent's samples follow from the next one.
    #[test]
    fn two_meter_changes_in_one_block_are_each_accented_on_their_frame() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);
        set_volume(&settings, Amplitude::new(1.0));
        let map = MeterMap::new([
            MeterChange::new(Beat::new(0.0), TimeSignature::default()),
            MeterChange::new(Beat::new(2.5), TimeSignature::of(3, 4)),
            MeterChange::new(Beat::new(2.75), TimeSignature::of(7, 8)),
        ]);
        play_out(&mut node, 2.0);

        let [left, _] = block_under(&mut node, &env(SR, 100, hundredths_from(2.0)), Some(&map));

        let accent = node.metronome.click(true).to_vec();
        assert_eq!(&left[50..75], &accent[..25], "the first change's click");
        assert_eq!(
            &left[75..96],
            &accent[..21],
            "the second restarts it on frame 75"
        );
        assert_eq!(node.metronome.onset(), Some(Beat::new(2.75)));
        assert!(node.metronome.is_accent());
    }

    /// A block under another meter is counted under it from its first
    /// frame, not from whenever the playhead leaves the span the old meter drew.
    #[test]
    fn a_new_blocks_meter_is_counted_from_its_first_frame() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);

        // 4/4: 2.00..2.10 all belong to the quarter at 2.0.
        let _ = block(&mut node, &env(SR, 10, hundredths_from(2.0)));
        assert_eq!(node.metronome.onset(), Some(Beat::new(2.0)));

        // 3/16: sixteenth-note beats, three to a 0.75-quarter bar. Beat 2.64
        // falls in the sixteenth starting at 2.5 — inside the old span.
        let map = MeterMap::new([MeterChange::new(
            Beat::new(0.0),
            TimeSignature::new(BeatsPerBar::of(3), NoteValue::SIXTEENTH),
        )]);
        let _ = block_under(&mut node, &env(SR, 10, hundredths_from(2.64)), Some(&map));
        assert_eq!(
            node.metronome.onset(),
            Some(Beat::new(2.5)),
            "the new meter's beat must be the one latched"
        );
    }

    /// A block renders exactly what the same frames render cut into
    /// one-frame blocks: the per-block reads (mode, volume, meter) change
    /// nothing while they are stable, and the beat is per frame.
    ///
    /// The beat *moves* through the block and crosses an onset mid-block, so
    /// this pins the per-frame onset path as well as the steady one: a node
    /// that read the beat once per block would start the click on frame 0 and
    /// diverge from the one-frame blocks there.
    #[test]
    fn a_block_matches_its_frames_one_by_one() {
        let (settings, mut block_node) = make_click();
        settings.mode().store(MetronomeMode::Always);
        set_volume(&settings, Amplitude::new(1.0));

        // 44.1 kHz at 120 BPM is 1/22050 beat per frame, so starting 20 frames
        // before beat 2 puts that onset at frame 20 of the block.
        const N: usize = 64;
        let mut host = FrameClock::new(
            Beat::new(2.0 - 20.0 / 22_050.0),
            Bpm::of(120.0),
            tutti_units::SampleRate::of(SR),
        );
        let counted = |host: &FrameClock| {
            tutti_graph::TransportPosition::counted(true, host.tempo(), host.origin(), None)
        };

        // Latch the beat before it, as a transport rolling into this block
        // would have, with its click already played out, so the onset at beat 2
        // is the only sound in the block.
        play_out(&mut block_node, 1.0);
        let mut frame_node = block_node.clone();

        let [left, right] = block(&mut block_node, &env(SR, N, counted(&host)));
        assert!(
            left.iter().any(|&s| s != 0.0),
            "the onset must fall inside the block, or this compares silence"
        );

        for i in 0..N {
            let [l, r] = block(&mut frame_node, &env(SR, 1, counted(&host)));
            host.advance(Samples::new(1), None);
            assert_eq!(left[i], l[0], "left channel diverged at frame {i}");
            assert_eq!(right[i], r[0], "right channel diverged at frame {i}");
        }
    }

    /// A click starts on the frame where the playhead reaches its onset — not
    /// on the next block boundary — at more than one block size.
    ///
    /// A node that read one beat per block from the clock's writeback would
    /// start every click on a block boundary, up to a block late.
    ///
    /// The expected frame is derived from tempo arithmetic alone, not from the
    /// node: the host's clock is at `start + i·bps` on frame `i` (emit, then
    /// advance), so the first frame at or past the onset is
    /// `ceil((onset − start) / bps)`. `±1` absorbs rounding across that
    /// boundary. Each block's transport is the host clock's, counted, as the
    /// engine hands it.
    #[test]
    fn click_onset_is_sample_accurate_within_the_block() {
        const RATE: f64 = 48_000.0;
        const BPM: f64 = 137.0;
        const START: f64 = 0.9;
        const ONSET: f64 = 1.0;

        for len in [64usize, 40] {
            let settings = Arc::new(ClickSettings::new());
            settings.mode().store(MetronomeMode::Always);
            set_volume(&settings, Amplitude::new(1.0));
            let mut node = prepared(
                ClickNode::from(ClickNodeConfig::new(settings)),
                tutti_units::SampleRate::of(RATE),
                len,
            );
            // The click's first frame is exactly zero (`sin(0)` under a zero
            // attack) and its second is not, so the onset is one before the
            // first non-zero frame. Asserted, since the test leans on it.
            assert_eq!(node.metronome.click(false)[0], 0.0);
            assert_ne!(node.metronome.click(false)[1], 0.0);
            // As if the transport had rolled through beat 0 already: the click
            // under test is the one at beat 1, and beat 0's has played out.
            play_out(&mut node, 0.0);

            let bps = tutti_music::beats_per_sample(Bpm::of(BPM), SampleRate::of(RATE)).get();
            let expected = ((ONSET - START) / bps).ceil() as usize;
            assert_ne!(
                expected % len,
                0,
                "the onset must land mid-block, or a block-quantised click passes"
            );

            let mut host = FrameClock::new(
                Beat::new(START),
                Bpm::of(BPM),
                tutti_units::SampleRate::of(RATE),
            );
            let mut left = Vec::new();
            while left.len() < expected + 2 * len {
                let t = tutti_graph::TransportPosition::counted(
                    true,
                    host.tempo(),
                    host.origin(),
                    None,
                );
                let [l, _] = block(&mut node, &env(RATE, len, t));
                host.advance(Samples::new(len), None);
                left.extend(l);
            }

            let first_sound = left
                .iter()
                .position(|s| *s != 0.0)
                .expect("the click at beat 1 must sound");
            let onset = first_sound - 1;
            assert!(
                onset.abs_diff(expected) <= 1,
                "block {len}: click started at frame {onset}, the beat reaches \
                 {ONSET} at frame {expected}"
            );
        }
    }

    /// A stop inside a block silences the click from its frame, and a start
    /// inside one sounds the beat it lands on from its frame: the play state
    /// is the block's pieces', not the block start's.
    ///
    /// Before its port to `tutti_graph::Node` the click read the live play flag once per
    /// 64-frame chunk, so a mid-block start or stop gated it from the
    /// block's start (the gap `tests/render/env_beat.rs` recorded).
    #[test]
    fn a_start_or_stop_inside_a_block_gates_on_its_frame() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);
        set_volume(&settings, Amplitude::new(1.0));

        // Rolling on beat 3 (its click sounding), stopped at frame 30.
        let mut changes = TransportChanges::NONE;
        changes
            .push(
                Offset::of(Samples::new(30), Samples::new(64)),
                held(STOPPED, 3.0),
            )
            .expect("fits");
        let mut rolling = env(SR, 64, held(ROLLING, 3.0));
        rolling.changes = changes;
        let [left, _] = block(&mut node, &rolling);
        assert!(left[1..30].iter().all(|&s| s != 0.0), "sounding until 30");
        assert!(left[30..].iter().all(|&s| s == 0.0), "silent from 30");

        // Stopped, then started at frame 10 on beat 5.
        let mut changes = TransportChanges::NONE;
        changes
            .push(
                Offset::of(Samples::new(10), Samples::new(64)),
                held(ROLLING, 5.0),
            )
            .expect("fits");
        let mut starting = env(SR, 64, held(STOPPED, 4.0));
        starting.changes = changes;
        let [left, _] = block(&mut node, &starting);
        assert!(left[..11].iter().all(|&s| s == 0.0), "silent until 10");
        assert_ne!(left[11], 0.0, "beat 5's click from frame 10");
    }

    #[test]
    fn a_preroll_only_click_sounds_only_in_the_count_in() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::PrerollOnly);
        set_volume(&settings, Amplitude::new(1.0));

        // Should be silent when not in preroll
        let output = frame(&mut node, ROLLING, 0.0);
        assert_eq!(output[0], 0.0);

        // The block's transport is in its count-in - should play
        Node::<f32>::reset(&mut node);
        let counting = Deck {
            count_in: true,
            ..ROLLING
        };
        assert!(
            sounds_within(&mut node, counting, 0.0, 100),
            "Click should play during preroll"
        );
    }

    #[test]
    fn a_recording_only_click_sounds_only_while_recording() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::RecordingOnly);
        set_volume(&settings, Amplitude::new(1.0));
        let recording = Deck {
            playing: true,
            recording: true,
            count_in: false,
        };

        // Should be silent when not recording
        let output = frame(&mut node, ROLLING, 0.0);
        assert_eq!(output[0], 0.0);

        // Enable recording - should play
        Node::<f32>::reset(&mut node);
        assert!(
            sounds_within(&mut node, recording, 0.0, 100),
            "Click should play during recording"
        );

        // In preroll while recording - should NOT play
        Node::<f32>::reset(&mut node);
        let output = frame(
            &mut node,
            Deck {
                count_in: true,
                ..recording
            },
            0.0,
        );
        assert_eq!(
            output[0], 0.0,
            "Click should not play during preroll in RecordingOnly mode"
        );
    }

    /// A fork of the click renders the metronome as it was set when it was
    /// taken: volume and mode come from a cell of its own, so neither live
    /// move below reaches it — and each is heard by a fork taken after it.
    /// The play state, the count-in and the meter are the fork's own block
    /// transport, by construction.
    #[test]
    fn a_fork_snapshots_the_settings() {
        type Move = fn(&ClickSettings);
        let moves: [(&str, Move); 2] = [
            ("volume", |s| set_volume(s, Amplitude::new(1.0))),
            ("mode", |s| s.mode().store(MetronomeMode::Off)),
        ];
        let fresh = || {
            let settings = Arc::new(ClickSettings::new());
            set_volume(&settings, Amplitude::new(0.5));
            settings.mode().store(MetronomeMode::RecordingOnly);
            let node = ClickNode::<f32>::from(ClickNodeConfig::new(Arc::clone(&settings)));
            (settings, ClickFork::of(&node))
        };
        // 10 240 frames rolling (and recording) from beat 0 at 20 beats a
        // second: four clicks.
        let render = |node: ClickNode<f32>| {
            let mut node = prepared(node, tutti_units::SampleRate::SR_48K, 64);
            let mut host = FrameClock::new(
                Beat::new(0.0),
                Bpm::of(1_200.0),
                tutti_units::SampleRate::SR_48K,
            );
            let mut left = Vec::new();
            for _ in 0..160 {
                let t = tutti_graph::TransportPosition::counted(
                    true,
                    host.tempo(),
                    host.origin(),
                    None,
                )
                .with_recording(true);
                left.extend(block(&mut node, &env(48_000.0, 64, t))[0].clone());
                host.advance(Samples::new(64), None);
            }
            left
        };
        let (_, source) = fresh();
        let baseline = render(source.node());
        assert!(baseline.iter().any(|&s| s != 0.0), "the fork sounds");
        for (what, apply) in moves {
            let (settings, source) = fresh();
            let taken = source.node();
            apply(&settings);
            assert_eq!(
                render(taken),
                baseline,
                "{what}: a live move reached a fork taken before it"
            );
            assert_ne!(
                render(source.node()),
                baseline,
                "{what}: a fork taken after the move does not hear it"
            );
        }
    }
}
