//! Monophonic audio to notes.
//!
//! A YIN pitch track ([`yin_track`](crate::yin_track)) and the onsets
//! ([`detect_onsets`](crate::detect_onsets)) are cut into notes: each has a
//! [`Note`], a bend in [`Cents`], a span in [`Samples`] and a peak level. Mapping that to a MIDI event is the caller's: the crate
//! holds no MIDI types, and [`VelocityRange`] is the one documented way to a
//! 1..=127 velocity.
//!
//! # Time
//!
//! YIN compares the first half of its [`YinConfig::buffer_size`] with a lagged
//! copy, so a frame starting at `i * hop` vouches for
//! `i * hop .. i * hop + buffer_size / 2`. A note spans the frames it holds:
//! from its first frame's start to its last frame's start plus that half.
//! A frame that is wholly silent is unvoiced, but one whose window is mostly
//! past a release still reads voiced: the silent part adds nothing to the
//! difference YIN measures, and the same holds at an attack: a frame whose
//! window holds only the first samples of a note can read voiced. A frame whose
//! window holds none of the note is unvoiced, so both edges are within the half
//! frame of the true one, and a start is never later than a hop and a period
//! after it. A note's start or end is thus within one hop plus half the YIN
//! frame of the true one. Where one note hands over to the next
//! the boundary is put where the change is: at the onset, or, for a pitch
//! change, midway between the centres of the last old and first new frame.
//!
//! Only whole frames are analysed, so the last `buffer_size / 2` samples (and
//! the part of the buffer past the last whole hop) belong to no note: a note
//! that runs to the end of the buffer ends at `last * hop + buffer_size / 2`
//! of its last frame, and one that starts inside that tail is lost.
//!
//! # Segmentation
//!
//! 1. A frame is *voiced* when YIN found a pitch and its RMS is at or above
//!    the [floor](NoteConfig::floor). The floor is absolute: YIN reads
//!    periodicity, not level, so without one a buffer of rounding noise has a
//!    pitch. YIN's own confidence is not used: any frame it calls voiced counts.
//! 2. Voiced frames join into a run across at most
//!    [`hold`](NoteConfig::hold) of dropout. The hold is capped so a rest
//!    of half a YIN frame or more is never joined.
//! 3. An onset inside a run splits it, so re-articulating one pitch gives two
//!    notes. An onset estimate is only as good as the onset window, so one
//!    within a window of the start of the piece it would cut (the run's start,
//!    or the last cut) is that piece's own attack, and one within a window of
//!    the run's end is its release; neither cuts anything. A piece cut by an
//!    onset is therefore at least an onset window long.
//! 4. A pitch change splits it where the median pitch of the
//!    [`smoothing`](NoteConfig::smoothing) span after a frame differs from
//!    the median of the span before by more than the
//!    [split](NoteConfig::pitch_split). The test is on the *rate* of
//!    change: pitch must move more than the split within about one smoothing
//!    span (the two medians' centres are a span apart). A slide slower than
//!    `split / smoothing`, and vibrato whose swing over a span is under the
//!    split, stay one note however far they travel; a faster slide splits once,
//!    midway. The medians reject a single-frame octave error. A change within
//!    one smoothing span of an onset is that onset's.
//! 5. A piece shorter than the [minimum length](NoteConfig::min_length) is
//!    dropped.
//!
//! The note is the median pitch of its frames, as the nearest [`Note`] and the
//! signed [`Cents`] from it, measured against a fixed A4 = 440 Hz (there is no
//! tuning offset). Its level is the loudest frame RMS wholly inside the note,
//! in dBFS: a sine of peak amplitude `a` reads `20 log10(a / sqrt 2)`. A note
//! with no whole frame inside it (shorter than a YIN frame) reads the loudest
//! of the frames it holds instead, which may include a neighbour's attack.
//!
//! These are offline or UI-thread calls: they allocate the pitch track, the
//! levels and the notes.

use crate::level::SILENCE_FLOOR;
use tutti_buffers::{Mono, Sample};
use tutti_dsp::{FrameCount, rms};
use tutti_music::Note;
use tutti_units::{Amplitude, Cents, Db, FrequencyRatio, SampleRate, Samples, Seconds};

use crate::analyses::yin::{PitchEstimate, YinConfig, YinPlan, yin_track_at};
use crate::convert::widen;
use crate::framing::{Framing, check_frame_count, check_hop_within, check_overlap};
use crate::meters::onset::{DetectionFunction, OnsetConfig, OnsetPlan, onset_lag, run};
use crate::meters::silence::FiniteLevel;
use crate::{Error, Region, Result, StreamMeter};
use tutti_dsp::{median, median_of_sorted};
use tutti_units::ranged;

/// The longest YIN frame accepted. YIN allocates several buffers of about this
/// size, so an absurd `min_freq` would otherwise ask for gigabytes.
const MAX_FRAME: usize = 1 << 18;

/// The most frames one call analyses, bounding the track, level, voicing and
/// run tables (about 60 bytes a frame between them, so about 250 MB here).
const MAX_FRAMES: FrameCount = FrameCount(1 << 22);

/// The most frames in one smoothing span, bounding the median's cost.
const MAX_SMOOTHING_FRAMES: FrameCount = FrameCount(1024);

ranged! {
    /// A time setting of a note analysis: within `0..=60` seconds.
    NoteSpan(Seconds), NoteSpanError, "a note time within 0..=60 s",
    |raw| raw.get() >= 0.0 && raw.get() <= 60.0
}

ranged! {
    /// How far the pitch track advances per frame: at least one sample. Whether
    /// the hop suits the YIN frame (not wider than it, not under a sixty-fourth
    /// of it) is checked by the call that takes the config.
    PitchHop(Samples), PitchHopError, "a hop of at least one sample",
    |raw| raw.get() >= 1
}

ranged! {
    /// A pitch change that splits a note: within `(0, 1200]` cents.
    PitchSplit(Cents), PitchSplitError, "a pitch split within (0, 1200] cents",
    |raw| raw.get() > 0.0 && raw.get() <= 1200.0
}

/// What [`transcribe`] and [`notes_from_track`] segment with. Immutable once
/// built, and free of a sample rate: the call that takes the audio resolves
/// the seconds to samples, checks the pitch band against Nyquist and the
/// settings that depend on each other, and answers with a named [`Error`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteConfig {
    yin: YinConfig,
    onset: OnsetConfig,
    hop: Option<PitchHop>,
    min_length: NoteSpan,
    pitch_split: PitchSplit,
    smoothing: NoteSpan,
    hold: NoteSpan,
    floor: Db,
}

impl NoteConfig {
    /// Segments the pitch track of the standard [`YinConfig`] and the onsets
    /// of a default spectral-flux [`OnsetConfig`] with the defaults: a hop of
    /// an eighth of the YIN frame, a 60 ms minimum note, a 50 cent split,
    /// 30 ms of smoothing, 10 ms of hold and a -60 dBFS floor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            yin: YinConfig::new(),
            onset: OnsetConfig::new(DetectionFunction::SpectralFlux),
            hop: None,
            min_length: NoteSpan::of(Seconds::new(0.06)),
            pitch_split: PitchSplit::of(Cents::new(50.0)),
            smoothing: NoteSpan::of(Seconds::new(0.03)),
            hold: NoteSpan::of(Seconds::new(0.01)),
            floor: Db::new(-60.0),
        }
    }

    /// The pitch analysis: its band and threshold.
    ///
    /// The call that takes the audio refuses a YIN frame longer than 262144
    /// samples ([`Error::YinFrameTooLong`]; a lowest frequency that low is not
    /// a note) and a band above the rate's Nyquist ([`Error::AboveNyquist`]).
    #[must_use]
    pub fn pitch(mut self, yin: YinConfig) -> Self {
        self.yin = yin;
        self
    }

    /// The onset analysis that splits notes. The call that takes the audio
    /// refuses one that is not [`DetectionFunction::SpectralFlux`], the only
    /// function whose peak has a known lag behind its attack
    /// ([`Error::OnsetNotSpectralFlux`]).
    #[must_use]
    pub fn onsets(mut self, onset: OnsetConfig) -> Self {
        self.onset = onset;
        self
    }

    /// How far the pitch track advances per frame.
    ///
    /// [`transcribe`], [`frame_levels`] and [`notes_from_track`] refuse a hop
    /// wider than the YIN frame ([`Error::HopWiderThanWindow`]) or under 1/64 of
    /// it ([`Error::TooManyOverlaps`]).
    #[must_use]
    pub fn hop(mut self, hop: PitchHop) -> Self {
        self.hop = Some(hop);
        self
    }

    /// Notes shorter than this are dropped. Zero keeps every piece.
    #[must_use]
    pub fn min_length(mut self, length: NoteSpan) -> Self {
        self.min_length = length;
        self
    }

    /// A pitch change above this splits a note. Under a semitone it reads
    /// vibrato as notes; over it, a step as one.
    #[must_use]
    pub fn pitch_split(mut self, split: PitchSplit) -> Self {
        self.pitch_split = split;
        self
    }

    /// How long a pitch change must last to split a note: the median span each
    /// side of a candidate. Shorter blips, and vibrato whose half period is
    /// shorter, do not split. At least one frame is used.
    #[must_use]
    pub fn smoothing(mut self, span: NoteSpan) -> Self {
        self.smoothing = span;
        self
    }

    /// The longest pitch dropout bridged inside a note, capped for the hop in
    /// use below the shortest true rest, so a rest is never bridged.
    #[must_use]
    pub fn hold(mut self, hold: NoteSpan) -> Self {
        self.hold = hold;
        self
    }

    /// Frames whose RMS is below this are unvoiced. Absolute, in dBFS, so a
    /// note quieter than it is lost; scale the input rather than lower it
    /// below the noise.
    #[must_use]
    pub fn floor(mut self, floor: FiniteLevel) -> Self {
        self.floor = floor.get();
        self
    }

    /// The config at `rate`.
    ///
    /// # Errors
    ///
    /// [`Error::AboveNyquist`] for a pitch band above half `rate`;
    /// [`Error::YinFrameTooLong`] when the YIN frame is longer than 262144
    /// samples; [`Error::OnsetNotSpectralFlux`] when the onset function is not
    /// spectral flux; [`Error::ZeroWindow`] or [`Error::ZeroHop`] for a zero onset window or hop.
    fn plan(&self, rate: SampleRate) -> Result<NotePlan> {
        let yin = self.yin.plan(rate)?;
        let frame = yin.buffer_size().get();
        if frame > MAX_FRAME {
            return Err(Error::YinFrameTooLong {
                frame: Samples::new(frame),
                max: Samples::new(MAX_FRAME),
            });
        }
        if self.onset.function() != DetectionFunction::SpectralFlux {
            return Err(Error::OnsetNotSpectralFlux);
        }
        let onset = self.onset.plan(rate)?;
        let samples = |span: NoteSpan| span.get().to_samples(rate);
        Ok(NotePlan {
            yin,
            onset,
            hop: self
                .hop
                .map_or(Samples::new((frame / 8).max(1)), |hop| hop.get()),
            min_length: samples(self.min_length),
            pitch_split: self.pitch_split,
            smoothing: samples(self.smoothing),
            hold: samples(self.hold),
            floor: self.floor,
        })
    }
}

impl Default for NoteConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`NoteConfig`] at one sample rate.
#[derive(Debug, Clone, Copy, PartialEq)]
struct NotePlan {
    yin: YinPlan,
    onset: OnsetPlan,
    hop: Samples,
    min_length: Samples,
    pitch_split: PitchSplit,
    smoothing: Samples,
    hold: Samples,
    floor: Db,
}

impl NotePlan {
    /// The span one pitch frame covers, in [`Samples`]: YIN's buffer size.
    #[inline]
    #[must_use]
    pub(crate) fn frame(&self) -> Samples {
        self.yin.buffer_size()
    }

    /// The YIN frame and the pitch hop as a [`Framing`].
    fn framing(&self) -> Framing {
        Framing::new(self.frame(), self.hop)
    }

    /// Whether the hop suits the YIN frame: not wider than it, and no more
    /// than `MAX_OVERLAP` hops across it.
    fn check_hop(&self) -> Result<()> {
        check_hop_within(self.frame(), self.hop)?;
        check_overlap(self.frame(), self.hop)
    }

    /// The most frames of dropout bridged: the hold in hops, capped one under
    /// the fewest unvoiced frames a rest of half a YIN frame `W` leaves. Frames
    /// from the last voiced start (at most the note's end) to the next note's
    /// first voiced start (not before its attack) are unvoiced between, so a
    /// rest `R >= W` leaves at least `ceil(W / hop) - 1`.
    fn bridge_frames(&self) -> FrameCount {
        let hold = self.hold.get() / self.hop.get();
        let rest = (self.frame().get() / 2)
            .div_ceil(self.hop.get())
            .saturating_sub(1);
        FrameCount(hold.min(rest.saturating_sub(1)))
    }

    /// Frames in the smoothing span, at least one.
    fn smoothing_frames(&self) -> FrameCount {
        FrameCount(self.smoothing.get().div_ceil(self.hop.get()).max(1))
    }

    /// Where an onset's attack is, from the position it reports.
    fn attack(&self, position: Samples) -> Samples {
        position + onset_lag(self.onset.geometry())
    }
}

/// One note.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteEvent {
    /// The nearest equal-tempered note to the median pitch.
    pub note: Note,
    /// The median pitch's distance from `note`, `-50..=50` [`Cents`]: the
    /// pitch-bend amount.
    pub cents: Cents,
    /// The samples the note spans. Never empty: `build_note` yields no note
    /// whose end is not past its start.
    pub span: Region,
    /// The loudest frame RMS wholly inside the note, in dBFS (the loudest frame
    /// it holds, for a note shorter than a YIN frame). See [`VelocityRange`]
    /// for a MIDI velocity.
    pub velocity: Db,
}

/// A linear-in-dB map from a note's level to a MIDI velocity: `floor` and
/// anything quieter is 1, `ceiling` and anything louder 127.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VelocityRange {
    floor: Db,
    ceiling: Db,
}

impl Default for VelocityRange {
    /// -60 dBFS to -3 dBFS, the RMS of a full-scale sine.
    fn default() -> Self {
        Self {
            floor: Db::new(-60.0),
            ceiling: Db::new(-3.0),
        }
    }
}

impl TryFrom<core::ops::RangeInclusive<FiniteLevel>> for VelocityRange {
    type Error = Error;

    /// # Errors
    ///
    /// [`Error::EmptyVelocityRange`] unless `floor < ceiling`.
    fn try_from(range: core::ops::RangeInclusive<FiniteLevel>) -> Result<Self> {
        let (floor, ceiling) = (range.start().get(), range.end().get());
        if floor >= ceiling {
            return Err(Error::EmptyVelocityRange { floor, ceiling });
        }
        Ok(Self { floor, ceiling })
    }
}

impl VelocityRange {
    /// Levels from `floor` to `ceiling`, or `None` unless `floor < ceiling`.
    /// `TryFrom` over `floor..=ceiling` is the same check with the reason.
    #[must_use]
    pub fn new(floor: FiniteLevel, ceiling: FiniteLevel) -> Option<Self> {
        Self::try_from(floor..=ceiling).ok()
    }

    /// The MIDI velocity of `level`, `1..=127`. A NaN level is 1.
    #[must_use]
    pub fn midi(&self, level: Db) -> u8 {
        let t = (level.get() - self.floor.get()) / (self.ceiling.get() - self.floor.get());
        if t.is_nan() {
            return 1;
        }
        1 + (t.clamp(0.0, 1.0) * 126.0).round() as u8
    }
}

/// Transcribes the notes of a mono buffer.
///
/// Runs [`yin_track`](crate::yin_track) at the config's hop, [`frame_levels`] and
/// [`detect_onsets`](crate::detect_onsets), and segments them with [`notes_from_track`]. Onsets are
/// placed at `position + 3 * window / 4 - hop / 2` of the onset geometry, where
/// spectral flux peaks behind an attack.
///
/// # Errors
///
/// [`Error::HopWiderThanWindow`] or [`Error::TooManyOverlaps`] when the hop does
/// not suit the YIN frame; [`Error::NonFiniteInput`] for a NaN or infinite
/// sample; [`Error::InsufficientInput`] when shorter than one YIN frame;
/// [`Error::NoSignal`] when the peak is under -90 dBFS;
/// [`Error::TooManyFrames`] when the hop would make more than 4194304
/// frames. Noise gives no notes rather than an error, and a buffer shorter
/// than the onset window (but not than a YIN frame) has no onsets, so only
/// pitch changes split it.
pub fn transcribe<S: Sample>(
    cfg: &NoteConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Vec<NoteEvent>> {
    let cfg = &cfg.plan(rate.into())?;
    let samples = &widen(samples.samples());
    cfg.check_hop()?;
    if samples.iter().any(|s| !s.is_finite()) {
        return Err(Error::NonFiniteInput);
    }
    let frame = cfg.frame();
    if samples.len() < frame.get() {
        return Err(Error::InsufficientInput {
            needed: frame,
            got: Samples::new(samples.len()),
        });
    }
    let peak = samples.iter().fold(0.0f64, |m, s| m.max(s.abs()));
    if peak < SILENCE_FLOOR.to_amplitude().get() {
        return Err(Error::NoSignal);
    }
    check_frame_count(
        cfg.framing().frames_for(Samples::new(samples.len())),
        MAX_FRAMES,
    )?;

    let track = yin_track_at(&cfg.yin, samples, cfg.hop)?;
    let levels = levels_at(cfg, samples)?;
    let onsets: Vec<Samples> = run(cfg.onset, samples)
        .finish()
        .iter()
        .map(|o| cfg.attack(o.position))
        .collect();
    segment(cfg, &track, &levels, &onsets)
}

/// The RMS of every pitch frame of `samples`: frame `i` covers
/// `i * hop .. i * hop + frame`, as [`yin_track`](crate::yin_track)'s does, so the result lines
/// up with a track made at the config's hop. Empty for input shorter than a
/// frame.
///
/// # Errors
///
/// [`Error::HopWiderThanWindow`] or [`Error::TooManyOverlaps`] when the hop
/// does not suit the YIN frame.
pub fn frame_levels<S: Sample>(
    cfg: &NoteConfig,
    samples: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Vec<Amplitude>> {
    levels_at(&cfg.plan(rate.into())?, &widen(samples.samples()))
}

/// [`frame_levels`] over a resolved config and `f64` samples.
fn levels_at(cfg: &NotePlan, samples: &[f64]) -> Result<Vec<Amplitude>> {
    cfg.check_hop()?;
    let frame = cfg.frame().get();
    let frames = cfg.framing().frames_for(Samples::new(samples.len()));
    Ok((0..frames.get())
        .map(|i| {
            let start = i * cfg.hop.get();
            rms(Mono::new(&samples[start..start + frame]))
        })
        .collect())
}

/// Segments a pitch track into notes.
///
/// `track` is [`yin_track`](crate::yin_track) at the config's hop and YIN config, `levels` the
/// [`frame_levels`] of the same audio, and `onsets` the sample positions of the
/// attacks, in any order. For onsets from [`detect_onsets`](crate::detect_onsets) add the lag
/// described at [`transcribe`].
///
/// # Errors
///
/// [`Error::HopWiderThanWindow`] or [`Error::TooManyOverlaps`] when the hop does
/// not suit the YIN frame; [`Error::LevelsLengthMismatch`] when `levels` and
/// `track` differ in length;
/// [`Error::NegativeInput`] when a level is negative;
/// [`Error::SmoothingTooLong`] when the smoothing span is over 1024 frames;
/// [`Error::TooManyFrames`] when the track is over 4194304 frames;
/// [`Error::NonFiniteInput`] for a non-finite level or voiced frequency;
/// [`Error::NonPositiveFrequency`] for a voiced frequency of zero or below.
pub fn notes_from_track(
    cfg: &NoteConfig,
    track: &[PitchEstimate],
    levels: &[Amplitude],
    onsets: &[Samples],
    rate: impl Into<SampleRate>,
) -> Result<Vec<NoteEvent>> {
    segment(&cfg.plan(rate.into())?, track, levels, onsets)
}

/// [`notes_from_track`] over a resolved config.
fn segment(
    cfg: &NotePlan,
    track: &[PitchEstimate],
    levels: &[Amplitude],
    onsets: &[Samples],
) -> Result<Vec<NoteEvent>> {
    cfg.check_hop()?;
    if track.len() != levels.len() {
        return Err(Error::LevelsLengthMismatch {
            levels: levels.len(),
            track: track.len(),
        });
    }
    check_frame_count(FrameCount(track.len()), MAX_FRAMES)?;
    if cfg.smoothing_frames() > MAX_SMOOTHING_FRAMES {
        return Err(Error::SmoothingTooLong {
            frames: cfg.smoothing_frames(),
            max: MAX_SMOOTHING_FRAMES,
        });
    }
    let frames = voiced_frames(track, levels, cfg.floor.to_amplitude())?;
    let mut onset_frames: Vec<(usize, usize)> = onsets
        .iter()
        .map(|t| (t.get().div_ceil(cfg.hop.get()), t.get()))
        .collect();
    onset_frames.sort_unstable();

    let mut notes = Vec::new();
    for run in runs(&frames, cfg.bridge_frames()) {
        let pieces = split_run(cfg, &run, &frames, &onset_frames);
        for (i, piece) in pieces.iter().enumerate() {
            let next_split = pieces.get(i + 1).and_then(|p| p.split);
            if let Some(note) = build_note(cfg, &frames, &run, piece, next_split)
                && note.span.len() >= cfg.min_length
            {
                notes.push(note);
            }
        }
    }
    Ok(notes)
}

/// The samples a voiced frame vouches for, from its start: the integration
/// window YIN compares, half the buffer.
fn span_of(cfg: &NotePlan) -> usize {
    cfg.frame().get() / 2
}

/// What a voiced frame contributes.
#[derive(Debug, Clone, Copy)]
struct Voiced {
    /// Pitch against A4, in [`Cents`].
    cents: Cents,
    level: Amplitude,
}

/// The voiced frames of a track, `None` where unvoiced or under `floor`.
fn voiced_frames(
    track: &[PitchEstimate],
    levels: &[Amplitude],
    floor: Amplitude,
) -> Result<Vec<Option<Voiced>>> {
    let a4 = Note::A4.frequency().get();
    track
        .iter()
        .zip(levels)
        .map(|(pitch, level)| {
            if !level.get().is_finite() {
                return Err(Error::NonFiniteInput);
            }
            if level.get() < 0.0 {
                return Err(Error::NegativeInput);
            }
            let Some(pitch) = pitch.pitch() else {
                return Ok(None);
            };
            let frequency = pitch.frequency.get();
            if !frequency.is_finite() {
                return Err(Error::NonFiniteInput);
            }
            if frequency <= 0.0 {
                return Err(Error::NonPositiveFrequency);
            }
            // Under the finite positive frequency, the ratio is positive unless
            // it underflows to zero.
            let ratio = FrequencyRatio::new(frequency / a4).ok_or(Error::NonPositiveFrequency)?;
            Ok((*level >= floor).then(|| Voiced {
                cents: Cents::from_pitch_ratio(ratio),
                level: *level,
            }))
        })
        .collect()
}

/// The voiced frame indices of each run: frames joined across at most
/// `bridge` unvoiced ones.
fn runs(frames: &[Option<Voiced>], bridge: FrameCount) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    for (i, _) in frames.iter().enumerate().filter(|(_, f)| f.is_some()) {
        match out.last_mut() {
            Some(run) if i - run[run.len() - 1] - 1 <= bridge.get() => run.push(i),
            _ => out.push(vec![i]),
        }
    }
    out
}

/// A slice of a run's frames and the sample where the note holding it begins,
/// when it begins at a split rather than at its first frame.
#[derive(Debug, Clone, Copy)]
struct Piece {
    /// Positions in the run, `from..to`.
    from: usize,
    to: usize,
    /// The split before this piece, in samples, or `None` for the first.
    split: Option<usize>,
}

/// Cuts a run at its onsets and pitch changes.
fn split_run(
    cfg: &NotePlan,
    run: &[usize],
    frames: &[Option<Voiced>],
    onsets: &[(usize, usize)],
) -> Vec<Piece> {
    let hop = cfg.hop.get();
    let span = cfg.smoothing_frames().get();

    // An onset at `t` (first whole frame `k`) splits before the first voiced
    // frame at or after `k`, provided something voiced precedes it.
    // An onset within one onset window of its piece's start is that piece's own
    // attack, and one within a window of the run's end is its release's flux:
    // an onset time is no better than the window.
    let guard = cfg.onset.geometry().window().get();
    let run_end = run[run.len() - 1] * hop + span_of(cfg);
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut piece_start = run[0] * hop;
    for &(k, t) in onsets {
        let pos = run.partition_point(|&f| f < k);
        if pos > 0
            && pos < run.len()
            && cuts.last().is_none_or(|&(p, _)| p < pos)
            && t >= piece_start + guard
            && t + guard <= run_end
        {
            cuts.push((pos, t));
            piece_start = run[pos] * hop;
        }
    }

    let cents: Vec<f64> = run
        .iter()
        .map(|&f| frames[f].map_or(0.0, |v| v.cents.get()))
        .collect();
    for pos in pitch_changes(&cents, span, cfg.pitch_split.get().get()) {
        if cuts.iter().all(|&(p, _)| p.abs_diff(pos) >= span) {
            // Midway between the centres of the last old and first new frame.
            let centre = |f: usize| f * hop + span_of(cfg) / 2;
            let split = (centre(run[pos - 1]) + centre(run[pos])) / 2;
            cuts.push((pos, split));
        }
    }
    cuts.sort_unstable();

    let mut pieces = Vec::with_capacity(cuts.len() + 1);
    let mut from = 0;
    let mut split = None;
    for (pos, t) in cuts {
        pieces.push(Piece {
            from,
            to: pos,
            split,
        });
        from = pos;
        split = Some(t);
    }
    pieces.push(Piece {
        from,
        to: run.len(),
        split,
    });
    pieces
}

/// The positions where the median of the next `span` values differs from the
/// median of the previous `span` by more than `split` cents, one per
/// contiguous stretch of such positions, at its middle.
///
/// Both windows slide, held sorted, so each step is a binary search and a
/// shift of at most `span` values rather than a sort.
fn pitch_changes(cents: &[f64], span: usize, split: f64) -> Vec<usize> {
    let middle = |sorted: &[f64]| median_of_sorted(sorted).unwrap_or_default();
    let n = cents.len();
    let mut changes = Vec::new();
    if n < 2 * span {
        return changes;
    }
    let sorted = |w: &[f64]| {
        let mut v = w.to_vec();
        v.sort_unstable_by(f64::total_cmp);
        v
    };
    let mut before = sorted(&cents[..span]);
    let mut after = sorted(&cents[span..2 * span]);
    let mut stretch: Option<(usize, usize)> = None;
    for pos in span..=n - span {
        if (middle(&after) - middle(&before)).abs() > split {
            stretch = Some(stretch.map_or((pos, pos), |(first, _)| (first, pos)));
        } else if let Some((first, last)) = stretch.take() {
            changes.push((first + last) / 2);
        }
        if pos < n - span {
            // `before` takes `cents[pos]` in and `cents[pos - span]` out;
            // `after` takes `cents[pos + span]` in and `cents[pos]` out.
            replace_sorted(&mut before, cents[pos - span], cents[pos]);
            replace_sorted(&mut after, cents[pos], cents[pos + span]);
        }
    }
    if let Some((first, last)) = stretch {
        changes.push((first + last) / 2);
    }
    changes
}

/// Removes one `old` from the sorted `v` and inserts `new`, keeping it sorted.
fn replace_sorted(v: &mut Vec<f64>, old: f64, new: f64) {
    let at = v.partition_point(|x| x.total_cmp(&old).is_lt());
    v.remove(at);
    let at = v.partition_point(|x| x.total_cmp(&new).is_lt());
    v.insert(at, new);
}

/// The note of one piece, or `None` when its span is empty.
fn build_note(
    cfg: &NotePlan,
    frames: &[Option<Voiced>],
    run: &[usize],
    piece: &Piece,
    next_split: Option<usize>,
) -> Option<NoteEvent> {
    let hop = cfg.hop.get();
    let frame = cfg.frame().get();
    let mine = &run[piece.from..piece.to];
    let (first, last) = (*mine.first()?, *mine.last()?);

    let covered_start = first * hop;
    let covered_end = last * hop + span_of(cfg);
    let start = piece.split.map_or(covered_start, |s| covered_start.max(s));
    let end = next_split.map_or(covered_end, |s| covered_end.min(s));
    if end <= start {
        return None;
    }

    let voiced = |f: usize| frames[f].expect("a run holds voiced frames");
    let cents: Vec<f64> = mine.iter().map(|&f| voiced(f).cents.get()).collect();
    let pitch = Cents::new(median(&cents)?);
    let (note, bend) = Note::nearest_to(Note::A4.frequency() * pitch.to_pitch_ratio())?;

    // The loudest frame wholly inside the note; one that straddles a neighbour
    // reads that neighbour's level too. A note shorter than a frame has none.
    let inside = |&&f: &&usize| f * hop >= start && f * hop + frame <= end;
    let whole: Vec<usize> = mine.iter().filter(inside).copied().collect();
    let level = if whole.is_empty() { mine } else { &whole }
        .iter()
        .map(|&f| voiced(f).level)
        .fold(Amplitude::SILENT, |a, b| a.max(b));

    Some(NoteEvent {
        note,
        cents: bend,
        span: Region::new(Samples::new(start), Samples::new(end))?,
        velocity: level.to_db(),
    })
}

#[cfg(test)]
mod tests;
