use super::*;
use crate::analyses::yin::Pitch;
use crate::framing::Framing;
use crate::meters::chroma::FrequencyBand;
use crate::test_rng::uniform;
use tutti_rt::SplitMix64;
use tutti_ump::NoteNumber;
use tutti_units::{Confidence, Hz};

const RATE: f64 = 44100.0;
const TAU: f64 = std::f64::consts::TAU;

/// A note config and what it resolves to at its rate: the tests read the
/// resolved figures through `Deref` and call the public entry points with
/// the config.
#[derive(Clone, Copy)]
struct Case {
    cfg: super::NoteConfig,
    rate: SampleRate,
    plan: NotePlan,
}

impl Case {
    fn new(cfg: super::NoteConfig, rate: SampleRate) -> Self {
        Self {
            cfg,
            rate,
            plan: cfg.plan(rate).unwrap(),
        }
    }

    fn with(self, change: impl FnOnce(super::NoteConfig) -> super::NoteConfig) -> Self {
        Self::new(change(self.cfg), self.rate)
    }

    fn hop(self, hop: PitchHop) -> Self {
        self.with(|c| c.hop(hop))
    }

    fn min_length(self, span: NoteSpan) -> Self {
        self.with(|c| c.min_length(span))
    }

    fn pitch_split(self, split: PitchSplit) -> Self {
        self.with(|c| c.pitch_split(split))
    }

    fn smoothing(self, span: NoteSpan) -> Self {
        self.with(|c| c.smoothing(span))
    }

    fn hold(self, span: NoteSpan) -> Self {
        self.with(|c| c.hold(span))
    }

    fn floor(self, floor: FiniteLevel) -> Self {
        self.with(|c| c.floor(floor))
    }
}

impl std::ops::Deref for Case {
    type Target = NotePlan;

    fn deref(&self) -> &NotePlan {
        &self.plan
    }
}

type NoteConfig = Case;

fn transcribe(c: &Case, x: &[f64]) -> Result<Vec<NoteEvent>> {
    super::transcribe(&c.cfg, Mono::new(x), c.rate)
}

fn frame_levels(c: &Case, x: &[f64]) -> Result<Vec<Amplitude>> {
    super::frame_levels(&c.cfg, Mono::new(x), c.rate)
}

fn notes_from_track(
    c: &Case,
    track: &[PitchEstimate],
    levels: &[Amplitude],
    onsets: &[Samples],
) -> Result<Vec<NoteEvent>> {
    super::notes_from_track(&c.cfg, track, levels, onsets, c.rate)
}

fn yin_track(yin: &YinPlan, x: &[f64], hop: Samples) -> Result<Vec<PitchEstimate>> {
    yin_track_at(yin, x, hop)
}

fn config(rate: f64, min_freq: f64, hop_div: usize, onset_window: usize) -> NoteConfig {
    let rate = SampleRate::of(rate);
    let yin =
        YinConfig::new().band(FrequencyBand::new(Hz::new(min_freq), Hz::new(1000.0)).unwrap());
    let onset = OnsetConfig::new(DetectionFunction::SpectralFlux).framing(Framing::new(
        Samples::new(onset_window),
        Samples::new(onset_window / 4),
    ));
    let frame = yin.buffer_size(rate).unwrap().get();
    Case::new(
        super::NoteConfig::new()
            .pitch(yin)
            .onsets(onset)
            .hop(PitchHop::of(Samples::new(frame / hop_div))),
        rate,
    )
}

/// How far a note's start or end may sit from the true attack or release
/// of a steady note: one hop for the frame grid plus the integration
/// window, half the YIN frame.
///
/// The window is the bound because a frame whose window holds only the
/// first or last samples of a note still reads voiced (one holding none is
/// unvoiced): a start can be early, and an end late, by up to a window. A
/// start is never later than a hop and a period after the attack, and an
/// end is never earlier than a period and a hop before the release, and a
/// period is within a window. An onset-split boundary is as exact as the
/// onset, about one onset window; an onset within one onset window of a
/// note's start is taken as that start's own and cuts nothing.
///
/// The bound holds away from the buffer's end: a note running to the end
/// stops at its last whole frame's start plus the window, which is at least
/// the window short of the end, and one starting in the final window is
/// lost.
fn timing_tolerance(c: &NoteConfig) -> Samples {
    Samples::new(c.hop.get() + c.frame().get() / 2)
}

fn band(min: f64, max: f64) -> FrequencyBand {
    FrequencyBand::new(Hz::new(min), Hz::new(max)).unwrap()
}

fn cfg() -> NoteConfig {
    config(RATE, 80.0, 8, 1024)
}

/// A tone from `a` to `b` seconds at `rate`: `pitch(t)` in Hz and
/// `amp(t)`, `t` seconds into the tone, with continuous phase.
struct Tone {
    a: f64,
    b: f64,
    pitch: Box<dyn Fn(f64) -> f64>,
    amp: Box<dyn Fn(f64) -> f64>,
}

fn tone(a: f64, b: f64, hz: f64, amp: f64) -> Tone {
    Tone {
        a,
        b,
        pitch: Box::new(move |_| hz),
        amp: Box::new(move |_| amp),
    }
}

fn render(rate: f64, tones: &[Tone], secs: f64) -> Vec<f64> {
    let mut out = vec![0.0f64; (secs * rate) as usize];
    for t in tones {
        let (a, b) = ((t.a * rate) as usize, (t.b * rate) as usize);
        let mut phase = 0.0f64;
        for (i, s) in out[a..b].iter_mut().enumerate() {
            let at = i as f64 / rate;
            *s = (t.amp)(at) * phase.sin();
            phase += TAU * (t.pitch)(at) / rate;
        }
    }
    out
}

fn cents_up(hz: f64, cents: f64) -> f64 {
    hz * (cents / 1200.0).exp2()
}

fn run(cfg: &NoteConfig, x: &[f64]) -> Vec<NoteEvent> {
    transcribe(cfg, x).unwrap()
}

fn secs(s: Samples) -> f64 {
    s.get() as f64 / RATE
}

fn near(got: Samples, want: f64, rate: f64, tol: Samples) -> bool {
    (got.get() as f64 - want * rate).abs() <= tol.get() as f64
}

/// A hand-made track: `Some(hz)` voiced, `None` not.
fn track(freqs: &[Option<f64>]) -> Vec<PitchEstimate> {
    freqs
        .iter()
        .map(|f| match f {
            None => PitchEstimate::Unvoiced,
            Some(hz) => {
                let (note, cents_offset) =
                    Note::nearest_to(Hz::new(*hz)).unwrap_or((Note::A4, Cents::new(0.0)));
                PitchEstimate::Voiced(Pitch {
                    frequency: Hz::new(*hz),
                    confidence: Confidence::CERTAIN,
                    note,
                    cents_offset,
                })
            }
        })
        .collect()
}

fn loud(n: usize) -> Vec<Amplitude> {
    vec![Amplitude::new(0.3); n]
}

/// A4, C5 and E5, each 250 ms, with gaps, come out as exactly those notes
/// at several YIN frame lengths, hops, onset windows and sample rates.
/// Starts are within a hop and a period of the true attack, and starts and
/// ends within one hop plus half the YIN frame (`timing_tolerance`).
///
/// The two 8192-sample onset windows are far wider than the pitch hop; an
/// onset cut there would clip a note's edges, which the guards in
/// `split_run` prevent.
#[test]
fn a_melody_gives_its_notes_at_every_geometry() {
    let melody = [
        (0.2, 0.45, 440.0, 69),
        (0.6, 0.85, 523.2511, 72),
        (1.0, 1.25, 659.2551, 76),
    ];
    for (rate, min_freq, hop_div, onset_window) in [
        (44100.0, 80.0, 8, 1024),
        (44100.0, 50.0, 4, 2048),
        (44100.0, 50.0, 16, 512),
        (48000.0, 100.0, 2, 1024),
        (22050.0, 60.0, 32, 512),
        // An onset window far wider than the pitch hop (8192 against 110).
        (44100.0, 80.0, 10, 8192),
        (44100.0, 50.0, 16, 8192),
    ] {
        let c = config(rate, min_freq, hop_div, onset_window);
        let tones: Vec<_> = melody
            .iter()
            .map(|&(a, b, hz, _)| tone(a, b, hz, 0.5))
            .collect();
        let notes = transcribe(&c, &render(rate, &tones, 1.5)).unwrap();
        assert_eq!(notes.len(), 3, "{notes:?} at {rate} {min_freq} {hop_div}");
        for (n, &(a, b, hz, midi)) in notes.iter().zip(&melody) {
            assert_eq!(NoteNumber::try_from(n.note).map(u8::from), Ok(midi));
            assert!(n.cents.get().abs() < 1.0, "{}", n.cents);
            let tol = timing_tolerance(&c);
            let period = Samples::new((rate / hz) as usize);
            // Never later than a hop and a period after the attack.
            let late = (a * rate) as usize + c.hop.get() + period.get();
            assert!(
                n.span.start().get() <= late,
                "start {:?} vs {a}",
                n.span.start()
            );
            assert!(
                near(n.span.start(), a, rate, tol),
                "start {:?} vs {a} ({tol:?})",
                n.span.start()
            );
            assert!(
                near(n.span.end(), b, rate, tol),
                "end {:?} vs {b} ({tol:?})",
                n.span.end()
            );
        }
    }
}

/// A tone 30 cents sharp is A4 plus 30 cents; 49 cents either way still
/// rounds to A4, and 51 to the neighbour.
#[test]
fn a_detuned_tone_reports_its_bend() {
    for (cents, midi, bend) in [
        (30.0, 69, 30.0),
        (-30.0, 69, -30.0),
        (49.0, 69, 49.0),
        (-49.0, 69, -49.0),
        (51.0, 70, -49.0),
    ] {
        let x = render(RATE, &[tone(0.1, 0.5, cents_up(440.0, cents), 0.5)], 0.7);
        let notes = run(&cfg(), &x);
        assert_eq!(notes.len(), 1, "{cents}: {notes:?}");
        assert_eq!(
            NoteNumber::try_from(notes[0].note).map(u8::from),
            Ok(midi),
            "{cents}"
        );
        assert!(
            (notes[0].cents.get() - bend).abs() < 1.5,
            "{cents}: {}",
            notes[0].cents
        );
    }
}

/// One pitch struck twice, with a 30 ms dip between, is two notes. Without
/// the onsets the same track is one: the split is the onset's.
#[test]
fn a_re_articulated_note_is_two_notes() {
    let c = cfg();
    let dip = |t: f64| if (0.2..0.23).contains(&t) { 0.02 } else { 0.5 };
    let x = render(
        RATE,
        &[Tone {
            a: 0.2,
            b: 0.7,
            pitch: Box::new(|_| 440.0),
            amp: Box::new(dip),
        }],
        0.9,
    );
    let notes = run(&c, &x);
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes
            .iter()
            .all(|n| NoteNumber::try_from(n.note).map(u8::from) == Ok(69))
    );
    assert!(notes[0].span.end() <= notes[1].span.start());
    let dip_start = 0.2 + 0.2; // the dip starts 0.2 s into the tone
    assert!(near(
        notes[1].span.start(),
        dip_start + 0.03,
        RATE,
        timing_tolerance(&c)
    ));

    let track = yin_track(&c.yin, &x, c.hop).unwrap();
    let levels = frame_levels(&c, &x).unwrap();
    assert_eq!(notes_from_track(&c, &track, &levels, &[]).unwrap().len(), 1);
}

/// Vibrato of +/-30 cents at 5 Hz, and a slide of 1000 cents a second
/// (30 cents over a 30 ms smoothing span, under the 50 cent split) that
/// travels 600 cents, are one note each; a 100 cent step with no gap is
/// two.
#[test]
fn pitch_changes_split_only_beyond_the_threshold() {
    let c = cfg();
    let vib = Tone {
        a: 0.1,
        b: 1.1,
        pitch: Box::new(|t| cents_up(440.0, 30.0 * (TAU * 5.0 * t).sin())),
        amp: Box::new(|_| 0.5),
    };
    let slide = Tone {
        a: 0.1,
        b: 0.7,
        pitch: Box::new(|t| cents_up(440.0, 1000.0 * t)),
        amp: Box::new(|_| 0.5),
    };
    for (t, len) in [(vib, 1.3), (slide, 0.9)] {
        let notes = run(&c, &render(RATE, &[t], len));
        assert_eq!(notes.len(), 1, "{notes:?}");
    }
    let step = Tone {
        a: 0.1,
        b: 0.7,
        pitch: Box::new(|t| {
            if t < 0.3 {
                440.0
            } else {
                cents_up(440.0, 100.0)
            }
        }),
        amp: Box::new(|_| 0.5),
    };
    let notes = run(&c, &render(RATE, &[step], 0.9));
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!(NoteNumber::try_from(notes[0].note).map(u8::from), Ok(69));
    assert_eq!(NoteNumber::try_from(notes[1].note).map(u8::from), Ok(70));
    // The onset the change makes cuts first, so the notes meet within a hop.
    assert!(notes[0].span.end() <= notes[1].span.start());
    assert!(notes[1].span.start().get() - notes[0].span.end().get() <= c.hop.get());
    assert!(
        near(notes[0].span.end(), 0.4, RATE, timing_tolerance(&c)),
        "{:?}",
        notes[0].span.end()
    );
}

/// The rule is a rate: a slide of 3 cents a frame (30 over a 10 frame
/// span, under the split) stays one note over 180 cents, and one of 8 cents
/// a frame (80 over a span) splits once, in the middle of the slide.
#[test]
fn a_fast_slide_splits_and_a_slow_one_does_not() {
    let c = cfg().min_length(NoteSpan::of(Seconds::new(0.0)));
    assert_eq!(c.smoothing_frames(), FrameCount(10));
    let slide = |per_frame: Cents| {
        let t: Vec<Option<f64>> = (0..60)
            .map(|i| Some(440.0 * (per_frame * f64::from(i)).to_pitch_ratio()))
            .collect();
        notes_from_track(&c, &track(&t), &loud(60), &[]).unwrap()
    };
    assert_eq!(slide(Cents::new(3.0)).len(), 1);
    let fast = slide(Cents::new(8.0));
    assert_eq!(fast.len(), 2, "{fast:?}");
    // Every position with the medians 10 frames apart more than 50 apart:
    // 8 c/frame * 10 = 80, so the stretch is the whole run of positions
    // 10..=50 and its middle is 30.
    let (hop, half) = (c.hop.get(), c.frame().get() / 2);
    let centre = |f: usize| f * hop + half / 2;
    assert_eq!(fast[0].span.end().get(), (centre(29) + centre(30)) / 2);

    // The split is the threshold: 80 cents over a span is under 200.
    let loose = c.pitch_split(PitchSplit::of(Cents::new(200.0)));
    let t: Vec<Option<f64>> = (0..60)
        .map(|i| Some(440.0 * (Cents::new(8.0) * f64::from(i)).to_pitch_ratio()))
        .collect();
    assert_eq!(
        notes_from_track(&loose, &track(&t), &loud(60), &[])
            .unwrap()
            .len(),
        1
    );
}

/// The sliding median agrees with sorting every window, on sequences with
/// repeats, negative zero and extremes.
#[test]
fn the_sliding_median_matches_sorting_each_window() {
    let mut rng = SplitMix64::new(11);
    for span in [1, 2, 3, 7, 10] {
        let cents: Vec<f64> = (0..200)
            .map(|_| match rng.next_u64() % 6 {
                0 => 0.0,
                1 => -0.0,
                2 => 100.0,
                _ => uniform(&mut rng, -80.0, 80.0),
            })
            .collect();
        let mut want = Vec::new();
        let mut stretch: Option<(usize, usize)> = None;
        for pos in span..=cents.len() - span {
            let med = |w: &[f64]| median(w).unwrap();
            let diff = (med(&cents[pos..pos + span]) - med(&cents[pos - span..pos])).abs();
            if diff > 40.0 {
                stretch = Some(stretch.map_or((pos, pos), |(f, _)| (f, pos)));
            } else if let Some((f, l)) = stretch.take() {
                want.push((f + l) / 2);
            }
        }
        if let Some((f, l)) = stretch {
            want.push((f + l) / 2);
        }
        assert!(!want.is_empty());
        assert_eq!(pitch_changes(&cents, span, 40.0), want, "span {span}");
    }
}

/// A tone shorter than the minimum is dropped, and kept when the minimum is
/// lowered; a zero minimum keeps everything.
#[test]
fn a_note_shorter_than_the_minimum_is_dropped() {
    let x = render(
        RATE,
        &[tone(0.1, 0.16, 440.0, 0.5), tone(0.4, 0.7, 523.2511, 0.5)],
        0.9,
    );
    let c = cfg().min_length(NoteSpan::of(Seconds::new(0.1)));
    let notes = run(&c, &x);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(NoteNumber::try_from(notes[0].note).map(u8::from), Ok(72));
    let kept = run(&cfg().min_length(NoteSpan::of(Seconds::new(0.02))), &x);
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert_eq!(NoteNumber::try_from(kept[0].note).map(u8::from), Ok(69));
}

/// Silence is `NoSignal`, noise gives no notes, a NaN or infinity and a
/// buffer shorter than a frame are refused.
#[test]
fn silence_noise_and_bad_input_are_defined() {
    let c = cfg();
    assert_eq!(transcribe(&c, &vec![0.0; 44100]), Err(Error::NoSignal));
    assert_eq!(transcribe(&c, &vec![1e-6; 44100]), Err(Error::NoSignal));
    let mut rng = SplitMix64::new(7);
    let noise: Vec<f64> = (0..88200).map(|_| uniform(&mut rng, -0.3, 0.3)).collect();
    assert_eq!(transcribe(&c, &noise), Ok(Vec::new()));
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut x = render(RATE, &[tone(0.1, 0.5, 440.0, 0.5)], 0.7);
        x[10_000] = bad;
        assert_eq!(transcribe(&c, &x), Err(Error::NonFiniteInput));
    }
    let n = c.frame().get() + 10 * c.hop.get() + c.hop.get() / 2;
    let mut tail: Vec<f64> = (0..n)
        .map(|i| (TAU * 440.0 * i as f64 / RATE).sin() * 0.5)
        .collect();
    *tail.last_mut().unwrap() = f64::NAN;
    assert_eq!(transcribe(&c, &tail), Err(Error::NonFiniteInput));
    assert!(matches!(
        transcribe(&c, &[0.5; 100]),
        Err(Error::InsufficientInput { .. })
    ));
}

/// Louder notes read louder: the level of a sine is its RMS in dBFS, to a
/// tenth of a decibel, and the MIDI velocity follows it. A note that
/// swells reads at its peak, not its start.
#[test]
fn velocity_follows_amplitude() {
    let amps = [0.05, 0.2, 0.8];
    let tones: Vec<_> = amps
        .iter()
        .enumerate()
        .map(|(i, &a)| {
            tone(
                0.1 + 0.5 * i as f64,
                0.4 + 0.5 * i as f64,
                440.0 * 1.2f64.powi(i as i32),
                a,
            )
        })
        .collect();
    let notes = run(&cfg(), &render(RATE, &tones, 1.8));
    assert_eq!(notes.len(), 3, "{notes:?}");
    let range = VelocityRange::default();
    for (n, &a) in notes.iter().zip(&amps) {
        let want = 20.0 * (a / 2f64.sqrt()).log10();
        assert!(
            (n.velocity.get() - want).abs() < 0.1,
            "{} vs {want}",
            n.velocity
        );
    }
    assert!(notes[0].velocity < notes[1].velocity && notes[1].velocity < notes[2].velocity);
    let v: Vec<u8> = notes.iter().map(|n| range.midi(n.velocity)).collect();
    assert!(v[0] < v[1] && v[1] < v[2], "{v:?}");

    let swell = Tone {
        a: 0.1,
        b: 0.7,
        pitch: Box::new(|_| 440.0),
        amp: Box::new(|t| 0.1 + 0.7 * t / 0.6),
    };
    let n = run(&cfg(), &render(RATE, &[swell], 0.9));
    assert_eq!(n.len(), 1, "{n:?}");
    assert!(n[0].velocity.get() > -8.0, "{}", n[0].velocity);
}

/// The map is linear in dB between its bounds, clamps outside them, and
/// reads NaN as the quietest.
#[test]
fn the_velocity_map_is_linear_and_clamped() {
    let r = VelocityRange::new(
        FiniteLevel::of(Db::new(-60.0)),
        FiniteLevel::of(Db::new(-20.0)),
    )
    .unwrap();
    assert_eq!(r.midi(Db::new(-60.0)), 1);
    assert_eq!(r.midi(Db::new(-20.0)), 127);
    assert_eq!(r.midi(Db::new(-40.0)), 64);
    assert_eq!(r.midi(Db::new(-100.0)), 1);
    assert_eq!(r.midi(Db::new(0.0)), 127);
    assert_eq!(r.midi(Db::new(f64::NEG_INFINITY)), 1);
    assert_eq!(r.midi(Db::new(f64::NAN)), 1);
    let level = |v: f64| FiniteLevel::of(Db::new(v));
    assert!(VelocityRange::new(level(-20.0), level(-20.0)).is_none());
    assert_eq!(
        VelocityRange::try_from(level(-20.0)..=level(-30.0)),
        Err(Error::EmptyVelocityRange {
            floor: Db::new(-20.0),
            ceiling: Db::new(-30.0)
        })
    );
    // A NaN or infinite bound is not a `FiniteLevel`.
    assert!(FiniteLevel::new(Db::new(f64::NAN)).is_none());
    assert!(FiniteLevel::new(Db::new(f64::INFINITY)).is_none());
}

/// Scaling the input by a power of two (exact in floating point) changes
/// nothing: pitch and timing are the same, bit for bit, and only the level
/// moves, by the gain in dB. Below the floor the note is gone.
#[test]
fn gain_changes_only_the_level() {
    let c = cfg();
    let base = render(
        RATE,
        &[tone(0.1, 0.4, 440.0, 0.25), tone(0.6, 0.9, 523.2511, 0.25)],
        1.1,
    );
    let want = run(&c, &base);
    assert_eq!(want.len(), 2);
    for (gain, db) in [(2.0f64, 6.0206), (0.25, -12.0412), (0.125, -18.0618)] {
        let scaled: Vec<f64> = base.iter().map(|s| s * gain).collect();
        let got = run(&c, &scaled);
        assert_eq!(got.len(), 2, "{gain}");
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(
                (g.note, g.cents, g.span.start(), g.span.end()),
                (w.note, w.cents, w.span.start(), w.span.end()),
                "{gain}"
            );
            assert!(
                (g.velocity.get() - w.velocity.get() - db).abs() < 0.01,
                "{gain}"
            );
        }
    }
    // -60 dBFS RMS is an amplitude of 0.00141; this is under it.
    let quiet: Vec<f64> = base.iter().map(|s| s * 0.001).collect();
    assert_eq!(run(&c, &quiet), Vec::new());
    let lowered = c.floor(FiniteLevel::of(Db::new(-80.0)));
    assert_eq!(run(&lowered, &quiet).len(), 2);
}

/// A dropout of up to the bridge is inside one note; one more frame is a
/// rest. On real audio a rest as short as 20 ms is never bridged, even
/// with a minute of hold.
#[test]
fn dropouts_are_bridged_and_rests_are_not() {
    let c = cfg();
    let bridge = c.bridge_frames().get();
    assert_eq!(bridge, 3);
    let with_gap = |gap: usize| {
        let mut t = vec![Some(440.0); 40];
        t.extend(vec![None; gap]);
        t.extend(vec![Some(440.0); 40]);
        notes_from_track(&c, &track(&t), &loud(80 + gap), &[]).unwrap()
    };
    assert_eq!(with_gap(bridge).len(), 1);
    assert_eq!(with_gap(bridge + 1).len(), 2);

    let x = render(
        RATE,
        &[tone(0.1, 0.4, 440.0, 0.5), tone(0.42, 0.7, 440.0, 0.5)],
        0.9,
    );
    let long = c.hold(NoteSpan::of(Seconds::new(60.0)));
    let t = yin_track(&long.yin, &x, long.hop).unwrap();
    let notes = notes_from_track(&long, &t, &frame_levels(&long, &x).unwrap(), &[]).unwrap();
    assert_eq!(notes.len(), 2, "{notes:?}");
}

/// An onset at `t` cuts before the first whole frame at or after `t`: the
/// earlier note ends at `t` and the later starts on that frame. Onsets in
/// any order, repeated, or at the run's own start give the same cut.
#[test]
fn an_onset_splits_at_its_first_whole_frame() {
    let c = cfg().min_length(NoteSpan::of(Seconds::new(0.0)));
    let hop = c.hop.get();
    let t = track(&vec![Some(440.0); 40]);
    let at = 17 * hop + 50;
    let notes = notes_from_track(&c, &t, &loud(40), &[Samples::new(at)]).unwrap();
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0].span.start(), Samples::new(0));
    assert_eq!(notes[0].span.end(), Samples::new(at));
    assert_eq!(notes[1].span.start(), Samples::new(18 * hop));
    assert_eq!(
        notes[1].span.end(),
        Samples::new(39 * hop + c.frame().get() / 2)
    );

    let extra = [
        Samples::new(30 * hop - 3),
        Samples::new(0),
        Samples::new(at),
        Samples::new(at),
    ];
    let mut reversed = extra;
    reversed.reverse();
    let a = notes_from_track(&c, &t, &loud(40), &extra).unwrap();
    let b = notes_from_track(&c, &t, &loud(40), &reversed).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.len(), 3);
    // An onset within one onset window (1024) of the piece's start is its own
    // attack: one sample inside cuts nothing, at the window it cuts.
    let guard = c.onset.geometry().window().get();
    let inside = notes_from_track(&c, &t, &loud(40), &[Samples::new(guard - 1)]).unwrap();
    assert_eq!(inside.len(), 1);
    let outside = notes_from_track(&c, &t, &loud(40), &[Samples::new(guard)]).unwrap();
    assert_eq!(outside.len(), 2);
    // Nor does one within a window of the run's end (`39 * hop + frame / 2`).
    let end = 39 * hop + c.frame().get() / 2;
    let late = [Samples::new(end - guard + 1)];
    assert_eq!(notes_from_track(&c, &t, &loud(40), &late).unwrap().len(), 1);
    let early = [Samples::new(end - guard)];
    assert_eq!(
        notes_from_track(&c, &t, &loud(40), &early).unwrap().len(),
        2
    );
    // An onset at the very start, or past the last frame, cuts nothing.
    let none = [Samples::new(0), Samples::new(1000 * hop)];
    assert_eq!(notes_from_track(&c, &t, &loud(40), &none).unwrap().len(), 1);
}

/// A pitch step in a hand-made track splits at the midpoint of the two
/// frames' centres; a single octave-error frame and a short blip do not.
#[test]
fn a_pitch_step_splits_at_the_frame_midpoint_and_blips_do_not() {
    let c = cfg();
    let (hop, half) = (c.hop.get(), c.frame().get() / 2);
    let mut step = vec![Some(440.0); 20];
    step.extend(vec![Some(523.2511); 20]);
    let notes = notes_from_track(&c, &track(&step), &loud(40), &[]).unwrap();
    assert_eq!(notes.len(), 2);
    let boundary = ((19 * hop + half / 2) + (20 * hop + half / 2)) / 2;
    assert_eq!(
        (notes[0].span.end().get(), notes[1].span.start().get()),
        (boundary, boundary)
    );
    assert_eq!(NoteNumber::try_from(notes[1].note).map(u8::from), Ok(72));

    let mut octave = vec![Some(440.0); 40];
    octave[20] = Some(880.0);
    for blip in 1..c.smoothing_frames().get() / 2 {
        octave[20..20 + blip].fill(Some(880.0));
        let notes = notes_from_track(&c, &track(&octave), &loud(40), &[]).unwrap();
        assert_eq!(notes.len(), 1, "{blip}: {notes:?}");
        assert_eq!(NoteNumber::try_from(notes[0].note).map(u8::from), Ok(69));
    }
}

/// The earlier note of an onset split reads its own level: frames that
/// reach past the cut see the louder note and are not counted.
#[test]
fn a_note_does_not_read_its_neighbours_level() {
    let c = cfg();
    let (hop, frame) = (c.hop.get(), c.frame().get());
    let k = 20;
    let mut levels = vec![Amplitude::new(0.1); 40];
    for l in &mut levels[k - frame / hop + 1..] {
        *l = Amplitude::new(0.8);
    }
    let t = track(&vec![Some(440.0); 40]);
    let notes = notes_from_track(&c, &t, &levels, &[Samples::new(k * hop)]).unwrap();
    assert_eq!(notes.len(), 2);
    assert!((notes[0].velocity.get() - Amplitude::new(0.1).to_db().get()).abs() < 1e-3);
    assert!((notes[1].velocity.get() - Amplitude::new(0.8).to_db().get()).abs() < 1e-3);
}

/// The rules across settings and the shape of the inputs are validated, so
/// absurd ones cannot allocate or overflow.
#[test]
fn settings_and_inputs_are_validated() {
    let rate = SampleRate::of(RATE);
    let flux = OnsetConfig::new(DetectionFunction::SpectralFlux)
        .framing(Framing::new(Samples::new(1024), Samples::new(256)));
    let notes = |yin: YinConfig, onset: OnsetConfig| {
        super::NoteConfig::new().pitch(yin).onsets(onset).plan(rate)
    };
    let huge = YinConfig::new().band(band(0.01, 1000.0));
    assert!(matches!(
        notes(huge, flux).unwrap_err(),
        Error::YinFrameTooLong { .. }
    ));
    let yin = YinConfig::new().band(band(80.0, 1000.0));
    let energy = OnsetConfig::new(DetectionFunction::Energy);
    assert_eq!(notes(yin, energy).unwrap_err(), Error::OnsetNotSpectralFlux);
    // A pitch band above Nyquist is refused where the rate binds.
    let high = YinConfig::new().band(band(80.0, 30_000.0));
    assert!(matches!(
        notes(high, flux).unwrap_err(),
        Error::AboveNyquist { .. }
    ));
    // A zero onset window is refused too.
    let zero = flux.framing(Framing::new(Samples::new(0), Samples::new(256)));
    assert!(matches!(notes(yin, zero).unwrap_err(), Error::ZeroWindow));

    let c = cfg();
    let frame = c.frame().get();
    // No hop writes zero; one wider than the frame, or under a sixty-fourth
    // of it, is refused by the calls that take the config.
    assert!(PitchHop::new(Samples::new(0)).is_none());
    let hop = |h: usize| c.hop(PitchHop::of(Samples::new(h)));
    let levels = |c: &NoteConfig| frame_levels(c, &vec![0.0; frame]).map(|_| ());
    assert!(matches!(
        levels(&hop(frame + 1)),
        Err(Error::HopWiderThanWindow { .. })
    ));
    assert!(matches!(
        levels(&hop(frame / 64 - 1)),
        Err(Error::TooManyOverlaps { .. })
    ));
    assert!(levels(&hop(frame)).is_ok());
    assert!(levels(&hop(frame.div_ceil(64))).is_ok());
    let t = track(&[Some(440.0); 3]);
    assert!(matches!(
        notes_from_track(&hop(frame + 1), &t, &loud(3), &[]),
        Err(Error::HopWiderThanWindow { .. })
    ));
    assert!(matches!(
        transcribe(&hop(1), &vec![0.1; frame]),
        Err(Error::TooManyOverlaps { .. })
    ));

    let t = track(&[Some(440.0); 10]);
    assert_eq!(
        notes_from_track(&c, &t, &loud(9), &[]),
        Err(Error::LevelsLengthMismatch {
            levels: 9,
            track: 10
        })
    );
    let mut neg = loud(10);
    neg[3] = Amplitude::new(-0.1);
    assert_eq!(
        notes_from_track(&c, &t, &neg, &[]),
        Err(Error::NegativeInput)
    );
    neg[3] = Amplitude::new(f64::NAN);
    assert_eq!(
        notes_from_track(&c, &t, &neg, &[]),
        Err(Error::NonFiniteInput)
    );
    let zero = track(&[Some(0.0); 10]);
    assert_eq!(
        notes_from_track(&c, &zero, &loud(10), &[]),
        Err(Error::NonPositiveFrequency)
    );
    let nan = track(&[Some(f64::NAN); 10]);
    assert_eq!(
        notes_from_track(&c, &nan, &loud(10), &[]),
        Err(Error::NonFiniteInput)
    );
    let wide = c.smoothing(NoteSpan::of(Seconds::new(60.0)));
    assert!(matches!(
        notes_from_track(&wide, &t, &loud(10), &[]),
        Err(Error::SmoothingTooLong { .. })
    ));
    assert!(notes_from_track(&c, &[], &[], &[]).unwrap().is_empty());
    assert!(
        notes_from_track(&c, &track(&[None; 10]), &loud(10), &[])
            .unwrap()
            .is_empty()
    );
}

/// A hop so small the input would make over 4 M frames is refused before
/// anything is allocated.
#[test]
fn too_many_frames_are_refused() {
    let yin = YinConfig::new().band(band(5000.0, 10000.0));
    let c = Case::new(
        super::NoteConfig::new().pitch(yin).onsets(
            OnsetConfig::new(DetectionFunction::SpectralFlux)
                .framing(Framing::new(Samples::new(1024), Samples::new(256))),
        ),
        SampleRate::of(RATE),
    );
    assert!(c.frame().get() < 64, "{:?}", c.frame());
    let c = c.hop(PitchHop::of(Samples::new(1)));
    let x = vec![0.1f64; MAX_FRAMES.get() + c.frame().get() + 1];
    assert_eq!(
        transcribe(&c, &x),
        Err(Error::TooManyFrames {
            frames: FrameCount(MAX_FRAMES.get() + 2),
            max: MAX_FRAMES
        })
    );
}

/// Exactly `MAX_FRAMES` frames are accepted and one more is not, for a length
/// that leaves a partial hop: `frame + (MAX_FRAMES - 1) * hop` is the last
/// length with `MAX_FRAMES` frames, `hop - 1` samples more still is.
#[test]
fn the_frame_limit_is_exact_with_a_partial_hop() {
    let c = Case::new(
        super::NoteConfig::new().pitch(YinConfig::new().band(band(5000.0, 10000.0))),
        SampleRate::of(RATE),
    );
    let hop = 2;
    let c = c.hop(PitchHop::of(Samples::new(hop)));
    let frame = c.frame().get();
    let last_ok = frame + (MAX_FRAMES.get() - 1) * hop;
    let check =
        |len: usize| check_frame_count(c.framing().frames_for(Samples::new(len)), MAX_FRAMES);
    assert!(check(last_ok).is_ok());
    assert!(check(last_ok + hop - 1).is_ok());
    assert!(check(last_ok + hop).is_err());
}

/// A note that runs to the end of the buffer ends at its last whole
/// frame's start plus half a frame, short of the end by at least that half;
/// a tone that starts inside the last half frame is lost.
#[test]
fn a_note_at_the_end_of_the_buffer_stops_short_of_it() {
    let c = cfg();
    let (frame, hop) = (c.frame().get(), c.hop.get());
    let n = (0.5 * RATE) as usize;
    let x = render(
        RATE,
        &[tone(0.1, n as f64 / RATE, 440.0, 0.5)],
        n as f64 / RATE,
    );
    let notes = run(&c, &x);
    assert_eq!(notes.len(), 1, "{notes:?}");
    let last = (n - frame) / hop;
    assert_eq!(notes[0].span.end().get(), last * hop + frame / 2);
    assert!(n - notes[0].span.end().get() >= frame / 2);

    // A tone in the final half frame has no frame whose window holds it.
    let start = n - frame / 2 + 1;
    let late = render(
        RATE,
        &[tone(start as f64 / RATE, n as f64 / RATE, 440.0, 0.5)],
        n as f64 / RATE,
    );
    assert_eq!(run(&c, &late), Vec::new());
}

/// The default config's figures, and `secs` for reading them.
#[test]
fn the_defaults_are_the_documented_ones() {
    let c = cfg();
    assert_eq!(c.min_length, Samples::new(2646));
    assert_eq!(c.smoothing, Samples::new(1323));
    assert_eq!(c.hold, Samples::new(441));
    assert_eq!(c.pitch_split, PitchSplit::of(Cents::new(50.0)));
    assert_eq!(c.floor, Db::new(-60.0));
    assert!((secs(c.min_length) - 0.06).abs() < 1e-6);
    let d = Case::new(
        super::NoteConfig::new().pitch(YinConfig::new().band(band(80.0, 1000.0))),
        SampleRate::of(RATE),
    );
    assert_eq!(d.hop.get(), d.frame().get() / 8);
}
