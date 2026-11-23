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
use tutti_buffers::ChannelTopology;
use tutti_buffers::Float;
use tutti_graph::{
    Cx, Env, ForkCause, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare, RenderMode,
    Shape, Status, declare_params,
};
use tutti_music::{Meter, MeterMap, TimeSignature};
use tutti_rt::{AtomicCell, atomic_word};
use tutti_units::Tail;
use tutti_units::{
    Amplitude, Beat, Hz, Phase, PhaseIncrement, PhaseStep, RunningPhase, Samples, Seconds,
};

/// How far two beat onsets must differ to count as different beats.
///
/// The playhead is an accumulated `f64`, so an exact compare would retrigger on
/// drift alone. A thousandth of a quarter note is far below the shortest notated
/// beat any legal meter can express (a 64th note is 0.0625 quarters) and far
/// above the drift of a realistic session.
const ONSET_EPSILON: f64 = 1e-3;

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
    /// The rate `click_normal` and `click_accent` were rendered at; `None`
    /// until the node is first prepared.
    sample_rate: Option<tutti_units::SampleRate>,
    /// The two clicks, designed in `f64` and crossed once per sample into `F` when the node is
    /// prepared.
    click_normal: Vec<F>,
    click_accent: Vec<F>,
    click_pos: usize,
    is_accent: bool,
    /// Timeline position of the notated beat currently sounding, or `None` when
    /// nothing has been clicked yet.
    ///
    /// A position rather than a running index: an index has to be scaled by some
    /// `beat_length`, which changes across a meter change, so it is only unique
    /// within one segment. `Option` rather than a sentinel value, since every
    /// finite beat — including negative pre-roll — is a legitimate onset.
    last_click_onset: Option<Beat>,
    /// Where the notated beat latched in `last_click_onset` ends: the next
    /// onset, or an earlier meter change. While the playhead stays in
    /// `[last_click_onset, latched_until)` nothing can retrigger, so the
    /// per-frame onset check is two compares instead of a `bar_at`.
    ///
    /// **A cache of the meter, so it is dropped at every block** (see
    /// [`forget_latch`](Self::forget_latch)): a meter republished mid-beat must
    /// be seen within one block, not only once the playhead leaves a span the
    /// *old* meter drew.
    ///
    /// Meaningless while `last_click_onset` is `None`, and never read then.
    latched_until: Beat,
}

impl<F: Float> std::fmt::Debug for ClickNode<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickNode")
            .field("settings", &self.settings)
            .field("sample_rate", &self.sample_rate)
            .field("click_pos", &self.click_pos)
            .field("is_accent", &self.is_accent)
            .field("last_click_onset", &self.last_click_onset)
            .field("latched_until", &self.latched_until)
            .finish_non_exhaustive()
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
            sample_rate: None,
            click_normal: Vec::new(),
            click_accent: Vec::new(),
            click_pos: 0,
            is_accent: false,
            last_click_onset: None,
            latched_until: Beat::new(f64::NEG_INFINITY),
        }
    }

    /// One rendered click: a windowed sine, generated once and replayed.
    ///
    /// The three envelope times are `Seconds` like the total, so the
    /// relationship between them (attack, then hold, then a release that ends
    /// exactly at `CLICK_LEN`) is stated in one unit rather than five bare
    /// floats that happen to line up.
    fn generate_click(sample_rate: tutti_units::SampleRate, is_accent: bool) -> Vec<F> {
        /// Total click length.
        const CLICK_LEN: Seconds = Seconds::new(0.03);
        /// Fade-in, then full level until `HOLD_END`, then fade to zero.
        const ATTACK: Seconds = Seconds::new(0.001);
        const HOLD_END: Seconds = Seconds::new(0.02);

        // `to_samples_ceil`, not a truncating cast: this sizes a buffer, which
        // is the allocating case the three named roundings exist to
        // distinguish.
        let num_samples = CLICK_LEN.to_samples_ceil(sample_rate).get();

        let freq = if is_accent {
            Hz::new(1200.0)
        } else {
            Hz::new(1000.0)
        };
        let accent_volume = if is_accent {
            Amplitude::new(1.0)
        } else {
            Amplitude::new(0.7)
        };
        let (attack, hold_end) = (ATTACK.get(), HOLD_END.get());
        let release_len = CLICK_LEN.get() - hold_end;
        let phase_step = PhaseStep::from(PhaseIncrement::per_sample(freq, sample_rate));

        let mut phase = RunningPhase::START;
        (0..num_samples)
            .map(|i| {
                let t = Samples::new(i).to_seconds(sample_rate).get();
                let env = if t < attack {
                    t / attack
                } else if t < hold_end {
                    1.0
                } else {
                    1.0 - (t - hold_end) / release_len
                };
                let sample = Phase::new(f64::from_phase(phase)).sin() * env * accent_volume.get();
                phase = phase.advance(phase_step);
                F::from_design(sample)
            })
            .collect()
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

    /// Silence the node and forget the last beat, so resuming re-triggers.
    ///
    /// Clears `is_accent` too: `advance_to` always rewrites it before the next
    /// sample is emitted, so leaving it stale is latent rather than live — but
    /// then `reset()` would not be a reset, and any future early-return path
    /// would turn that into a real bug.
    #[inline]
    fn go_silent(&mut self) {
        self.click_pos = 0;
        self.is_accent = false;
        self.last_click_onset = None;
    }

    /// Retrigger if `beat` has crossed into a new notated beat under `meter`
    /// (`None`: 4/4 from beat 0).
    ///
    /// The index counts *notated* beats, not quarter notes: in 7/8 that is an
    /// eighth, so the metronome clicks seven times per bar rather than four.
    ///
    /// Called once per **frame**, so the common case — still inside the beat
    /// already latched — returns after two compares; `bar_at` runs only when the
    /// playhead leaves that span, i.e. about once per notated beat.
    ///
    /// Takes its mutable fields individually rather than `&mut self`: the block
    /// loop holds the playback buffers borrowed alongside it.
    #[inline]
    fn advance_to(
        last_click_onset: &mut Option<Beat>,
        latched_until: &mut Beat,
        click_pos: &mut usize,
        is_accent: &mut bool,
        meter: Option<&MeterMap>,
        beat: Beat,
    ) {
        if let Some(onset) = *last_click_onset
            && beat >= onset
            && beat < *latched_until
        {
            return;
        }

        let position = match meter {
            Some(map) => map.bar_at(beat),
            None => TimeSignature::default().bar_at(beat),
        };

        // Identify the beat by the *onset it belongs to*, not by a running
        // count. The onset is unambiguous everywhere
        // and needs no global numbering.
        let onset = position.bar_start
            + position.signature.beat_length() * (position.beat.get() - 1) as f64;

        // Inequality, not `>`: a backward jump from a loop wrap must retrigger
        // too. The epsilon is for float drift in the accumulated playhead, well
        // below the shortest notated beat this meter can express.
        let changed = match *last_click_onset {
            Some(previous) => (onset - previous).get().abs() > ONSET_EPSILON,
            None => true,
        };

        if changed {
            *last_click_onset = Some(onset);
            *click_pos = 0;
            // The accent is the bar's downbeat, straight from the meter — never
            // a standalone every-N count, which drifts against the bar the
            // moment the time signature is not 4/4.
            *is_accent = position.is_downbeat();
        }

        // The latched beat ends at the next notated onset — or sooner, at a
        // meter change, which may fall mid-beat (the bar before a change is
        // simply short). Measured from the onset actually latched, so a
        // within-epsilon non-change keeps the span it already had.
        let latched = last_click_onset.unwrap_or(onset);
        let next_onset = latched + position.signature.beat_length();
        let changes = meter.map_or(&[][..], |m| m.changes());
        let next_change = changes
            .get(changes.partition_point(|c| c.beat <= latched))
            .map(|c| c.beat);
        *latched_until = match next_change {
            Some(change) if change < next_onset => change,
            _ => next_onset,
        };
    }

    /// Drop the cached span, so the next onset check consults the meter.
    ///
    /// Called at the top of every block, which is all it takes for a
    /// republished meter to reach the node: the meter is lent per block, and
    /// this is the only thing derived from it that outlives one.
    #[inline]
    fn forget_latch(&mut self) {
        self.latched_until = Beat::new(f64::NEG_INFINITY);
    }

    /// One sample of the click envelope, or silence once it has run out.
    ///
    /// Over the playback fields rather than `&mut self`, for the reason
    /// [`advance_to`](Self::advance_to) gives.
    #[inline]
    fn next_sample(
        click_normal: &[F],
        click_accent: &[F],
        click_pos: &mut usize,
        is_accent: bool,
        volume: F,
    ) -> F {
        let buffer = if is_accent {
            click_accent
        } else {
            click_normal
        };

        if *click_pos < buffer.len() {
            let sample = buffer[*click_pos] * volume;
            *click_pos += 1;
            sample
        } else {
            F::ZERO
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
        self.forget_latch();
        for (start, piece) in env.segments() {
            if !Self::should_play(mode, &piece.transport) {
                self.go_silent();
                let range = start.index()..start.index() + piece.block_len.get();
                left[range.clone()].fill(F::ZERO);
                right[range].fill(F::ZERO);
                continue;
            }
            let Self {
                click_normal,
                click_accent,
                click_pos,
                is_accent,
                last_click_onset,
                latched_until,
                ..
            } = self;
            piece.for_each_piece_beat(start, |i, beat| {
                Self::advance_to(
                    last_click_onset,
                    latched_until,
                    click_pos,
                    is_accent,
                    meter,
                    beat,
                );
                let sample =
                    Self::next_sample(click_normal, click_accent, click_pos, *is_accent, volume);
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
        Shape::source(ChannelTopology::STEREO)
            .tail(Tail::None)
            // A beat derived and compared per frame: measured 3.8 ns per
            // frame and channel, against a gain's 0.2 and `SvfFilterNode`'s
            // 7.2.
            .cost(tutti_graph::Cost::Moderate)
    }

    /// Render the two click waveforms at the prepared rate (control thread:
    /// this allocates), unless they already are.
    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate();
        if self
            .sample_rate
            .is_some_and(|had| (had.get() - rate.get()).abs() <= 0.1)
        {
            return;
        }
        self.sample_rate = Some(rate);
        self.click_normal = Self::generate_click(rate, false);
        self.click_accent = Self::generate_click(rate, true);
        self.go_silent();
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
        self.go_silent();
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
    use tutti_units::Bpm;
    use tutti_units::Frame;
    use tutti_units::SampleRate;

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

    /// One frame at `beat` under the meter `map`, as a `[left, right]` pair.
    fn frame_under(node: &mut ClickNode<f32>, map: &MeterMap, deck: Deck, beat: f64) -> [f32; 2] {
        let [l, r] = block_under(node, &env(SR, 1, held(deck, beat)), Some(map));
        [l[0], r[0]]
    }

    /// Stores a click level, as the session's volume target does after it
    /// has checked the range.
    fn set_volume(settings: &ClickSettings, level: Amplitude) {
        settings.set_volume(ClickLevel::of(level));
    }

    /// Whether any of `n` one-frame blocks at `beat` sounds.
    fn sounds_within(node: &mut ClickNode<f32>, deck: Deck, beat: f64, n: usize) -> bool {
        (0..n).any(|_| {
            let out = frame(node, deck, beat);
            out[0] != 0.0 || out[1] != 0.0
        })
    }

    #[test]
    fn test_click_node_silent_when_paused() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);

        let output = frame(&mut node, STOPPED, 0.0);
        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn test_click_node_plays_on_beat() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);
        set_volume(&settings, Amplitude::new(1.0));

        assert!(
            sounds_within(&mut node, ROLLING, 0.0, 100),
            "Click should produce non-zero output"
        );
        assert!(
            sounds_within(&mut node, ROLLING, 1.0, 100),
            "Click should produce non-zero output on new beat"
        );
    }

    #[test]
    fn test_click_plays_after_loop_wrap() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);
        set_volume(&settings, Amplitude::new(1.0));

        let _ = frame(&mut node, ROLLING, 7.0);
        assert_eq!(node.last_click_onset, Some(Beat::new(7.0)));

        // Simulate loop wrap: beat jumps backward from 7 to 4
        let found_nonzero = (0..100).any(|_| frame(&mut node, ROLLING, 4.0)[0] != 0.0);
        assert_eq!(
            node.last_click_onset,
            Some(Beat::new(4.0)),
            "Should reset to beat 4 after loop wrap"
        );
        assert!(found_nonzero, "Click should play after loop wrap");
    }

    /// The accent is the bar's downbeat, taken from the meter — not a fixed
    /// every-N count, which would ignore the project's time signature.
    #[test]
    fn accent_follows_the_meter_downbeat() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);

        let accents_at = |node: &mut ClickNode<f32>, map: &MeterMap, beat: f64| {
            Node::<f32>::reset(node);
            let _ = frame_under(node, map, ROLLING, beat);
            node.is_accent
        };

        // 4/4: accent every 4 quarter notes.
        let four = MeterMap::default();
        assert!(accents_at(&mut node, &four, 0.0));
        assert!(!accents_at(&mut node, &four, 1.0));
        assert!(accents_at(&mut node, &four, 4.0));

        // 3/4: the accent moves to every 3 quarters. A fixed count of 4 would
        // drift against the bar here.
        let three = MeterMap::new([MeterChange::new(
            Beat::new(0.0),
            TimeSignature::new(BeatsPerBar::of(3), NoteValue::QUARTER),
        )]);
        assert!(accents_at(&mut node, &three, 3.0));
        assert!(!accents_at(&mut node, &three, 4.0));
        assert!(accents_at(&mut node, &three, 6.0));

        // No meter on the transport: 4/4 from beat 0.
        Node::<f32>::reset(&mut node);
        let _ = frame(&mut node, ROLLING, 4.0);
        assert!(node.is_accent);
        Node::<f32>::reset(&mut node);
        let _ = frame(&mut node, ROLLING, 3.0);
        assert!(!node.is_accent);
    }

    /// In 7/8 the notated beat is an eighth, so a bar holds seven clicks across
    /// 3.5 quarter notes — not four clicks on the quarters.
    #[test]
    fn click_rate_follows_the_notated_beat() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);
        let map = MeterMap::new([MeterChange::new(
            Beat::new(0.0),
            TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH),
        )]);
        let frame = |node: &mut ClickNode<f32>, deck, beat| frame_under(node, &map, deck, beat);

        // Half a quarter note apart is a full notated beat in 7/8, so these are
        // distinct clicks.
        let _ = frame(&mut node, ROLLING, 0.0);
        assert_eq!(node.last_click_onset, Some(Beat::new(0.0)));
        assert!(node.is_accent, "beat 0 is the downbeat of bar 1");

        let _ = frame(&mut node, ROLLING, 0.5);
        assert_eq!(
            node.last_click_onset,
            Some(Beat::new(0.5)),
            "an eighth is one notated beat"
        );
        assert!(!node.is_accent, "beat 2 of the bar is not accented");

        // The next bar starts at 3.5 quarters, not 7.
        let _ = frame(&mut node, ROLLING, 3.5);
        assert!(node.is_accent, "3.5 quarters is the downbeat of bar 2");
    }

    /// Clicks must stay distinct across a meter change.
    ///
    /// A running index scaled by the *current* segment's `beat_length` cannot
    /// do this: `beat_length` changes at a `MeterChange`, so indices from a 7/8
    /// segment collide with indices from a following 4/4 segment and silently
    /// suppress clicks. Identifying the beat by its onset position removes the
    /// scale entirely.
    #[test]
    fn clicks_stay_distinct_across_a_meter_change() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);
        // 7/8 (eighth-note beats) for three bars, then 4/4 (quarter-note beats).
        let map = MeterMap::new([
            MeterChange::new(
                Beat::new(0.0),
                TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH),
            ),
            MeterChange::new(Beat::new(10.5), TimeSignature::default()),
        ]);

        // Walk every notated beat across the change and collect the onsets the
        // node actually latched.
        let mut onsets = Vec::new();
        let mut beat = 0.0;
        while beat < 14.5 {
            let _ = frame_under(&mut node, &map, ROLLING, beat);
            if let Some(onset) = node.last_click_onset
                && onsets.last() != Some(&onset)
            {
                onsets.push(onset);
            }
            // Step by whichever notated beat is in force here.
            beat += if beat < 10.5 { 0.5 } else { 1.0 };
        }

        // Every latched onset must be distinct and strictly increasing — a
        // collision would show up as a missing entry.
        for pair in onsets.windows(2) {
            assert!(
                pair[1] > pair[0],
                "onsets must strictly increase across the change, got {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        // 21 eighths before the change (0.0..10.5) + 4 quarters after.
        assert_eq!(
            onsets.len(),
            25,
            "every notated beat must click exactly once"
        );
        assert!(onsets.contains(&Beat::new(10.5)), "the change begins a bar");
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
        node.last_click_onset = Some(Beat::new(2.0));
        node.click_pos = node.click_normal.len();

        // Frame `i` carries beat `2 + i/100`, so the change at 2.5 is frame 50.
        let [left, _] = block_under(&mut node, &env(SR, 64, hundredths_from(2.0)), Some(&map));

        assert_eq!(
            node.last_click_onset,
            Some(Beat::new(2.5)),
            "the change's downbeat is a new onset"
        );
        assert!(node.is_accent, "and it is a downbeat");
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
        node.last_click_onset = Some(Beat::new(2.0));
        node.click_pos = node.click_normal.len();

        let [left, _] = block_under(&mut node, &env(SR, 100, hundredths_from(2.0)), Some(&map));

        let accent = node.click_accent.clone();
        assert_eq!(&left[50..75], &accent[..25], "the first change's click");
        assert_eq!(
            &left[75..96],
            &accent[..21],
            "the second restarts it on frame 75"
        );
        assert_eq!(node.last_click_onset, Some(Beat::new(2.75)));
        assert!(node.is_accent);
    }

    /// A block under another meter is counted under it from its first
    /// frame, not from whenever the playhead leaves the span the old meter drew.
    #[test]
    fn a_new_blocks_meter_is_counted_from_its_first_frame() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);

        // 4/4: 2.00..2.10 all belong to the quarter at 2.0.
        let _ = block(&mut node, &env(SR, 10, hundredths_from(2.0)));
        assert_eq!(node.last_click_onset, Some(Beat::new(2.0)));

        // 3/16: sixteenth-note beats, three to a 0.75-quarter bar. Beat 2.64
        // falls in the sixteenth starting at 2.5 — inside the old span.
        let map = MeterMap::new([MeterChange::new(
            Beat::new(0.0),
            TimeSignature::new(BeatsPerBar::of(3), NoteValue::SIXTEENTH),
        )]);
        let _ = block_under(&mut node, &env(SR, 10, hundredths_from(2.64)), Some(&map));
        assert_eq!(
            node.last_click_onset,
            Some(Beat::new(2.5)),
            "the new meter's beat must be the one latched"
        );
    }

    /// Pre-roll sits at negative beats, so the accent must be derived from the
    /// meter rather than from a cast — `(beat as u32)` turns -1 into
    /// 4294967295 and accents an arbitrary beat.
    #[test]
    fn negative_beats_do_not_wrap() {
        let (settings, mut node) = make_click();
        settings.mode().store(MetronomeMode::Always);

        // One bar before the start: a downbeat, and no panic or wrap.
        let _ = frame(&mut node, ROLLING, -4.0);
        assert!(node.is_accent, "-4.0 in 4/4 is the downbeat of bar 0");

        let _ = frame(&mut node, ROLLING, -3.0);
        assert!(!node.is_accent, "-3.0 is beat 2 of bar 0");
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
        block_node.last_click_onset = Some(Beat::new(1.0));
        block_node.click_pos = block_node.click_normal.len();
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
            assert_eq!(node.click_normal[0], 0.0);
            assert_ne!(node.click_normal[1], 0.0);
            // As if the transport had rolled through beat 0 already: the click
            // under test is the one at beat 1, and beat 0's has played out.
            node.last_click_onset = Some(Beat::new(0.0));
            node.click_pos = node.click_normal.len();

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
    fn test_preroll_only_mode() {
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
    fn test_recording_only_mode() {
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
