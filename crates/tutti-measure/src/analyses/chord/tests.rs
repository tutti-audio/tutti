use super::*;
use crate::test_rng::uniform;
use crate::{FramingError, TooManyFrames};
use tutti_dsp::{BinCount, FrameCount};
use tutti_rt::SplitMix64;
use tutti_units::Cents;

fn framing(window: usize, hop: usize) -> Framing {
    Framing::new(Samples::new(window), Samples::new(hop))
}

fn cfg() -> ChordConfig {
    ChordConfig::new()
}

/// A triad's chroma: the three notes at 1.
fn triad(root: usize, minor: bool) -> [f64; CLASSES] {
    let mut x = [0.0; CLASSES];
    for n in if minor { MINOR } else { MAJOR } {
        x[(root + n) % CLASSES] = 1.0;
    }
    x
}

fn grid(rows: &[[f64; CLASSES]]) -> Grid<f64> {
    Grid::try_from((rows.concat(), FrameCount(rows.len()), BinCount(CLASSES))).unwrap()
}

fn run(rows: &[[f64; CLASSES]], cfg: &ChordConfig) -> Vec<ChordSpan> {
    detect(&grid(rows), framing(4096, 1024), cfg).unwrap()
}

fn roots(spans: &[ChordSpan]) -> Vec<(PitchClass, ChordQuality)> {
    spans.iter().map(|s| (s.root, s.quality)).collect()
}

/// Each of the 24 triads, as exact chroma, is its own chord, and the
/// similarity of a frame to its own template is 1.
#[test]
fn every_triad_is_itself() {
    for minor in [false, true] {
        for root in 0..CLASSES {
            let x = triad(root, minor);
            let sims = similarities(&x);
            let own = if minor { root + CLASSES } else { root };
            assert!((sims[own] - 1.0).abs() < 1e-12);
            let spans = run(&[x; 5], &cfg());
            let quality = if minor {
                ChordQuality::Minor
            } else {
                ChordQuality::Major
            };
            assert_eq!(
                roots(&spans),
                [(PitchClass::from_semitones_above_c(root as u8), quality)]
            );
        }
    }
}

/// Silence is no chord, with similarity 1 for the "no chord" state only;
/// a flat or noise-like chroma is no chord too.
#[test]
fn silence_and_noise_are_no_chord() {
    let sims = similarities(&[0.0; CLASSES]);
    assert_eq!(sims[NO_CHORD], 1.0);
    assert!(sims[..NO_CHORD].iter().all(|&s| s == 0.0));
    assert!(run(&[[0.0; CLASSES]; 8], &cfg()).is_empty());
    assert!(run(&[[1.0; CLASSES]; 8], &cfg()).is_empty());
    let mut rng = SplitMix64::new(7);
    let noise: Vec<[f64; CLASSES]> = (0..200)
        .map(|_| {
            let mut x: [f64; CLASSES] = core::array::from_fn(|_| uniform(&mut rng, 0.3, 1.0));
            let max = x.iter().copied().fold(0.0, f64::max);
            x.iter_mut().for_each(|v| *v /= max);
            x
        })
        .collect();
    assert!(run(&noise, &cfg()).is_empty());
}

/// A single pitch class, a bass line of them and a bare fifth are no chord:
/// a single note matches each of its six triads at `1/sqrt(3) = 0.577` and
/// a fifth matches two of them at `sqrt(2/3) = 0.816`, both under the 0.85
/// of "no chord"; none names a root and mode.
#[test]
fn a_lone_note_a_bass_line_and_a_fifth_are_no_chord() {
    let note = |class: usize| {
        let mut x = [0.0; CLASSES];
        x[class] = 1.0;
        x
    };
    let best = |x: &[f64]| {
        similarities(x)[..NO_CHORD]
            .iter()
            .copied()
            .fold(0.0, f64::max)
    };
    assert!((best(&note(2)) - 1.0 / 3.0f64.sqrt()).abs() < 1e-12);
    assert!(run(&[note(2); 6], &cfg()).is_empty());
    let bass: Vec<_> = [0, 7, 9, 5, 0, 7, 5, 0]
        .iter()
        .flat_map(|&c| [note(c); 4])
        .collect();
    assert!(run(&bass, &cfg()).is_empty());
    let mut fifth = [0.0; CLASSES];
    (fifth[0], fifth[7]) = (1.0, 1.0);
    assert!((best(&fifth) - (2.0f64 / 3.0).sqrt()).abs() < 1e-12);
    assert!(run(&[fifth; 6], &cfg()).is_empty());
    // A third at 0.15 of the others is still a triad.
    let mut weak = triad(0, false);
    weak[4] = 0.15;
    assert_eq!(run(&[weak; 6], &cfg()).len(), 1);
}

/// A triad at 1 standing over a floor of 0.2 to 0.4 in every other column
/// is that triad: the floor is removed before the templates are scored.
/// Unremoved, a floor of 0.4 leaves the triad matching at 0.82, under the
/// 0.85 of "no chord" (at a floor of 0.2 to 0.4 on average, 0.88 would
/// still pass, so the constant 0.4 case is the one that pins it).
#[test]
fn a_triad_over_a_realistic_floor_is_still_the_triad() {
    let mut rng = SplitMix64::new(5);
    for (root, minor, constant) in [(0, false, None), (9, true, None), (7, false, Some(0.4))] {
        let rows: Vec<[f64; CLASSES]> = (0..30)
            .map(|_| {
                let mut x = triad(root, minor);
                for v in x.iter_mut().filter(|v| **v == 0.0) {
                    *v = constant.unwrap_or_else(|| uniform(&mut rng, 0.2, 0.4));
                }
                x
            })
            .collect();
        let quality = if minor {
            ChordQuality::Minor
        } else {
            ChordQuality::Major
        };
        assert_eq!(
            roots(&run(&rows, &cfg())),
            [(PitchClass::from_semitones_above_c(root as u8), quality)]
        );
    }
}

/// Scaling the chroma by 1e30 or 1e-30 changes nothing: the squares are
/// held in `f64`, and the scores are cosines. The sequence has noise,
/// where an unscaled score would turn it into chords or a triad into none.
#[test]
fn the_chords_do_not_depend_on_the_scale() {
    let mut rng = SplitMix64::new(9);
    let mut rows: Vec<[f64; CLASSES]> = (0..40)
        .map(|_| core::array::from_fn(|_| uniform(&mut rng, 0.3, 1.0)))
        .collect();
    rows.extend([triad(0, false); 10]);
    rows.extend([triad(9, true); 10]);
    let at = |scale: f64| -> Vec<ChordSpan> {
        let scaled: Vec<_> = rows.iter().map(|r| r.map(|v| v * scale)).collect();
        run(&scaled, &cfg())
    };
    let unit = at(1.0);
    assert_eq!(unit.len(), 2);
    assert_eq!(at(1e30), unit);
    assert_eq!(at(1e-30), unit);
}

/// A stay is not a switch into oneself. At a self-transition of 0.02 a
/// switch (`ln(0.98 / 24) = -3.20`) costs less than a stay (`ln 0.02 =
/// -3.91`), so the best way into the leading state is from the runner-up.
/// After a C triad, a frame that matches C at 0.90 (above "no chord" by
/// 0.05, 0.4 of an emission) is best read as "no chord": it is worth
/// `0.71` more to come into that state from C by a switch than into C by a
/// stay, which a search that let C "switch" into itself would miss.
#[test]
fn a_state_cannot_switch_into_itself() {
    let mut second = [0.0; CLASSES];
    (second[0], second[4], second[7]) = (1.0, 0.4, 0.4);
    let sim = similarities(&second)[0];
    assert!(sim > 0.85 && sim < 0.939, "{sim}");
    let config = cfg().self_transition(SelfTransition::of(0.02));
    let path = viterbi(&grid(&[triad(0, false), second]), &config);
    assert_eq!(path, [0, NO_CHORD as u16]);
}

/// Long input keeps the stay prior exact. At a self-transition of `1 - 2^-24`
/// a switch costs 19.8 and a stay nothing, so one odd frame (an F triad among
/// 300000 C triads, worth `sharpness * 2/3` over C) is kept exactly when
/// `sharpness * 2/3 > 2 * 19.8`. The margin either side is 0.3, which the
/// scores must still resolve after 300000 frames.
#[test]
fn a_long_input_keeps_the_stay_prior() {
    let p = 1.0 - 2.0f64.powi(-24);
    let (stay, switch) = (p.ln(), ((1.0 - p) / 24.0).ln());
    let cost = 2.0 * (stay - switch);
    let (c, f) = (triad(0, false), triad(5, false));
    let mut rows = vec![c; 300_000];
    rows.push(f);
    rows.extend([c; 100]);
    let grid = grid(&rows);
    for (offset, kept) in [(0.3, true), (-0.3, false)] {
        // The F frame's margin over C is two thirds of the sharpness.
        let sharpness = (cost + offset) * 1.5;
        let config = cfg()
            .self_transition(SelfTransition::new(p).unwrap())
            .sharpness(Sharpness::new(sharpness).unwrap());
        let spans = detect(&grid, framing(4096, 1024), &config).unwrap();
        assert_eq!(spans.len(), if kept { 3 } else { 1 }, "offset {offset}");
    }
}

/// A one-frame flicker is gone under the default prior, and kept when the
/// self-transition is 1/25, the even share, where every frame takes its own
/// best state. A change that lasts is kept either way.
#[test]
fn smoothing_removes_a_flicker_that_argmax_keeps() {
    let (c, f) = (triad(0, false), triad(5, false));
    let flicker = [c, c, c, c, f, c, c, c, c];
    let smoothed = run(&flicker, &cfg());
    assert_eq!(roots(&smoothed), [(PitchClass::C, ChordQuality::Major)]);
    let raw = cfg().self_transition(SelfTransition::of(1.0 / 25.0));
    let per_frame = run(&flicker, &raw);
    assert_eq!(
        roots(&per_frame),
        [
            (PitchClass::C, ChordQuality::Major),
            (PitchClass::F, ChordQuality::Major),
            (PitchClass::C, ChordQuality::Major)
        ]
    );
    let change = [c, c, c, c, f, f, f, f, f];
    assert_eq!(run(&change, &cfg()).len(), 2);
}

/// Span boundaries sit at the midpoints between frame centres, whatever
/// the geometry, including a hop wider than the window: the change before
/// frame `j` is half-way between centres `(j-1) hop + window/2` and
/// `j hop + window/2`; the first span starts at 0 and the last ends at the
/// end of the last window.
#[test]
fn spans_sit_between_frame_centres() {
    let (c, f) = (triad(0, false), triad(5, false));
    for (window, hop) in [
        (2048, 512),
        (4096, 1024),
        (4096, 4096),
        (1000, 333),
        (512, 1500),
    ] {
        let g = framing(window, hop);
        let rows: Vec<_> = (0..12).map(|i| if i < 7 { c } else { f }).collect();
        let spans = detect(&grid(&rows), g, &cfg()).unwrap();
        let centre = |i: usize| (i * hop + window / 2) as f64;
        let boundary = (centre(6) + centre(7)) / 2.0;
        assert_eq!(spans.len(), 2, "{window}/{hop}");
        assert_eq!(spans[0].span.start(), Samples::new(0));
        assert!(
            (spans[0].span.end().get() as f64 - boundary).abs() <= 1.0,
            "{window}/{hop}"
        );
        assert_eq!(spans[1].span.start(), spans[0].span.end());
        assert_eq!(spans[1].span.end(), Samples::new(11 * hop + window));
    }
}

/// A gap where nothing is detected: spans are separated by it.
#[test]
fn no_chord_leaves_a_gap() {
    let rows: Vec<_> = (0..24)
        .map(|i| {
            if (8..16).contains(&i) {
                [0.0; CLASSES]
            } else {
                triad(7, true)
            }
        })
        .collect();
    let spans = run(&rows, &cfg());
    assert_eq!(spans.len(), 2);
    assert!(spans[0].span.end() < spans[1].span.start());
    assert_eq!(roots(&spans), [(PitchClass::G, ChordQuality::Minor); 2]);
}

/// The shape, values and sizes of the input are validated.
#[test]
fn inputs_are_validated() {
    let invalid = |g: &Grid<f64>| detect(g, framing(4096, 1024), &cfg()).unwrap_err();
    let wide = Grid::try_from((vec![0.0; 13], FrameCount(1), BinCount(13))).unwrap();
    assert_eq!(invalid(&wide), ChordError::NotTwelveClasses { columns: 13 });
    let mut x = [0.5; CLASSES];
    x[4] = f64::NAN;
    assert_eq!(invalid(&grid(&[x])), ChordError::NonFiniteInput);
    x[4] = f64::NEG_INFINITY;
    assert_eq!(invalid(&grid(&[x])), ChordError::NonFiniteInput);
    x[4] = -0.5;
    assert_eq!(invalid(&grid(&[x])), ChordError::NegativeInput);
    let huge = Grid::try_from((
        vec![0.0; (MAX_FRAMES.get() + 1) * CLASSES],
        FrameCount(MAX_FRAMES.get() + 1),
        BinCount(CLASSES),
    ))
    .unwrap();
    assert_eq!(
        invalid(&huge),
        ChordError::TooManyFrames(TooManyFrames {
            frames: FrameCount(MAX_FRAMES.get() + 1),
            max: MAX_FRAMES
        })
    );
    let empty = Grid::try_from((Vec::new(), FrameCount(0), BinCount(CLASSES))).unwrap();
    assert!(
        detect(&empty, framing(4096, 1024), &cfg())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        detect(
            &grid(&[triad(0, false); 3]),
            framing(1024, usize::MAX),
            &cfg()
        ),
        Err(ChordError::SpanBeyondSampleCount)
    );
    // A zero window or hop is refused, not placed.
    for (zero, error) in [
        (
            framing(0, 1024),
            ChordError::Framing(FramingError::ZeroWindow),
        ),
        (framing(4096, 0), ChordError::Framing(FramingError::ZeroHop)),
    ] {
        assert_eq!(
            detect(&grid(&[triad(0, false); 3]), zero, &cfg()),
            Err(error)
        );
    }
    // The most frames still smooth, in one pass.
    let max = Grid::try_from((
        vec![0.0; MAX_FRAMES.get() * CLASSES],
        MAX_FRAMES,
        BinCount(CLASSES),
    ))
    .unwrap();
    assert!(
        detect(&max, framing(4096, 1024), &cfg())
            .unwrap()
            .is_empty()
    );
}

/// The spans sit at the samples of the framing the chroma was taken at:
/// the same frames read as a chroma of 2048/512 and of 4096/1024 place the
/// same change at different samples, each at its own frame centres.
#[test]
fn spans_follow_the_chromas_own_framing() {
    let (c, f) = (triad(0, false), triad(5, false));
    let rows: Vec<_> = (0..12).map(|i| if i < 7 { c } else { f }).collect();
    for (window, hop) in [(2048, 512), (4096, 1024)] {
        let chroma = Chroma::new(grid(&rows), Cents::new(0.0), framing(window, hop));
        let spans = detect_chords(&cfg(), &chroma).unwrap();
        let centre = |i: usize| (i * hop + window / 2) as f64;
        let boundary = (centre(6) + centre(7)) / 2.0;
        assert_eq!(spans.len(), 2, "{window}/{hop}");
        assert!((spans[0].span.end().get() as f64 - boundary).abs() <= 1.0);
        assert_eq!(spans[1].span.end(), Samples::new(11 * hop + window));
    }
}

/// A meter of a C major triad reads one C major span from the start to the end
/// of the audio, in any chunking; a chroma band above Nyquist is refused when
/// the meter is made, for a chord and a key alike.
#[test]
fn the_chord_meter_reads_the_triad_and_refuses_a_chroma_it_cannot_hold() {
    let rate = SampleRate::of(22_050.0);
    let x = crate::test_rng::tones(22_050.0, &[261.626, 329.628, 391.995], 44_100);
    let spans = crate::measure::chunk_independent(&cfg(), rate, &x).unwrap();
    assert_eq!(roots(&spans), [(PitchClass::C, ChordQuality::Major)]);
    assert_eq!(spans[0].span.start(), Samples::new(0));
    let last = (Framing::new(Samples::new(4096), Samples::new(1024)))
        .frames_for(Samples::new(x.len()))
        .get()
        - 1;
    assert_eq!(spans[0].span.end(), Samples::new(last * 1024 + 4096));

    let above = crate::meters::chroma::FrequencyBand::new(
        tutti_units::Hz::new(100.0),
        tutti_units::Hz::new(20_000.0),
    )
    .unwrap();
    let chroma = crate::ChromaConfig::new().band(above);
    assert!(matches!(
        Measure::<f64>::measure(&cfg().chroma(chroma), rate),
        Err(MeterError::Nyquist(_))
    ));
    assert!(matches!(
        Measure::<f64>::measure(
            &crate::KeyConfig::new(crate::KeyProfile::Krumhansl).chroma(chroma),
            rate
        ),
        Err(MeterError::Nyquist(_))
    ));
}
