use super::*;
use crate::testing::uniform;
use tutti_rt::SplitMix64;

const STATES: usize = 25;

/// The linear-time search finds the same best path as trying every path:
/// all 25^4 over four random frames, at three self-transitions (one under
/// the even share, where a state must not "switch" into itself).
#[test]
fn viterbi_is_the_best_path() {
    let mut rng = SplitMix64::new(11);
    for p in [0.9f64, 0.5, 0.02] {
        for _ in 0..3 {
            let emit: Vec<[f64; STATES]> = (0..4)
                .map(|_| core::array::from_fn(|_| uniform(&mut rng, 0.0, 1.0) * 6.0))
                .collect();
            let (stay, switch) = (p.ln(), ((1.0 - p) / 24.0).ln());
            let mut best = (f64::MIN, [0usize; 4]);
            for code in 0..STATES.pow(4) {
                let path: [usize; 4] =
                    core::array::from_fn(|i| code / STATES.pow(i as u32) % STATES);
                let mut score = emit[0][path[0]];
                for t in 1..4 {
                    score += if path[t] == path[t - 1] { stay } else { switch };
                    score += emit[t][path[t]];
                }
                if score > best.0 {
                    best = (score, path);
                }
            }
            let found = sticky_viterbi(STATES as u16, 4, stay, switch, |t, row| {
                row.copy_from_slice(&emit[t]);
            });
            let found: Vec<usize> = found.iter().map(|&s| usize::from(s)).collect();
            assert_eq!(found, best.1, "p {p}");
        }
    }
}

/// With no frames there is no path, and with one state the path is that state
/// throughout.
#[test]
fn an_empty_input_has_no_path_and_a_lone_state_holds_throughout() {
    assert!(sticky_viterbi(3, 0, -0.1, -2.0, |_, _| unreachable!()).is_empty());
    assert_eq!(sticky_viterbi(1, 5, -0.1, -2.0, |_, _| {}), [0; 5]);
    assert_eq!(sticky_viterbi(0, 2, -0.1, -2.0, |_, _| {}), [0; 2]);
}

/// A switch is taken exactly when it pays for its two crossings: the middle
/// frame favours state 1 by 1.5, and going there and back costs two switches,
/// so the detour wins when `1.5 + 2 * switch > 0`, that is `switch > -0.75`.
#[test]
fn a_switch_is_taken_exactly_when_it_pays_for_its_two_crossings() {
    let rows = [[1.0, 0.0], [0.0, 1.5], [1.0, 0.0]];
    let run =
        |switch: f64| sticky_viterbi(2, 3, 0.0, switch, |t, row| row.copy_from_slice(&rows[t]));
    assert_eq!(run(-0.7), [0, 1, 0]);
    assert_eq!(run(-0.8), [0, 0, 0]);
}

/// Scores shifted to a zero best keep a path exact over a long input: one
/// odd frame is kept exactly when it is worth more than its two switches
/// (20).
#[test]
fn a_long_input_keeps_the_stay_prior() {
    let (stay, switch) = (0.0, -10.0);
    let frames = 300_001;
    for (margin, kept) in [(20.3f64, true), (19.7, false)] {
        let path = sticky_viterbi(2, frames, stay, switch, |t, row| {
            row.copy_from_slice(&if t == 150_000 {
                [0.0, margin]
            } else {
                [margin, 0.0]
            });
        });
        let changes = path.windows(2).filter(|w| w[0] != w[1]).count();
        // The odd frame is worth `margin` in state 1 and nothing in state 0,
        // against two switches of 10.
        assert_eq!(changes, if kept { 2 } else { 0 }, "margin {margin}");
    }
}

fn banded(pitches: u16, layers: u16, half_width: u16, stay_p: f64) -> BandedTransition<f64> {
    let switch_p = (1.0 - stay_p) / f64::from(layers - 1);
    BandedTransition::triangular(pitches, layers, half_width, stay_p.ln(), switch_p.ln()).unwrap()
}

/// Every row of the transition matrix is a distribution, edge rows included
/// (the band is cut by the grid there and renormalised): the probabilities of
/// the moves out of a state sum to 1, to rounding of a few hundred terms.
#[test]
fn every_row_of_the_banded_transition_sums_to_one() {
    for (pitches, layers, half_width) in [(12u16, 2u16, 3u16), (9, 3, 8), (5, 2, 40), (7, 2, 0)] {
        let model = banded(pitches, layers, half_width, 0.9);
        for from in 0..pitches * layers {
            let sum: f64 = (0..pitches * layers)
                .filter_map(|to| model.log_probability(from, to))
                .map(f64::exp)
                .sum();
            assert!(
                (sum - 1.0).abs() < 1e-12,
                "{pitches}x{layers} w{half_width} row {from}: {sum}"
            );
        }
    }
}

/// A move farther than the half width in pitch has no probability, and inside
/// it the weight is triangular: at distance `d` it is `(w + 1 - d)` over the
/// row's sum, so with room on both sides the ratio of two distances is the
/// ratio of their weights.
#[test]
fn the_band_is_triangular_and_closed_beyond_the_half_width() {
    let model = banded(20, 2, 4, 0.8);
    assert_eq!(model.log_probability(10, 15), None);
    assert_eq!(model.log_probability(10, 20 + 15), None);
    let near = model.log_probability(10, 11).unwrap();
    let far = model.log_probability(10, 13).unwrap();
    // Weights 4 and 2.
    assert!(((near - far).exp() - 2.0).abs() < 1e-12);
    // The layer term: a switch of layer at the same pitch is `(1 - 0.8)`.
    let same = model.log_probability(10, 10).unwrap();
    let other = model.log_probability(10, 30).unwrap();
    assert!(((same - other).exp() - 4.0).abs() < 1e-12);
}

/// The path moves at most the half width in pitch per frame, however much a
/// frame favours a distant state: a frame that scores pitch 15 far above all
/// else, after frames that score pitch 0, is reached only by stepping `w`
/// pitches a frame.
#[test]
fn a_banded_path_moves_at_most_the_half_width_a_frame() {
    let half_width = 3u16;
    let frames = 6;
    let path = viterbi(banded(16, 2, half_width, 0.9), frames, |t, row| {
        row.fill(0.0);
        let want = if t == 0 { 0 } else { 15 };
        // Frame 0 outweighs every later frame together.
        let reward = if t == 0 { 1000.0 } else { 50.0 };
        row[want] = reward;
        row[16 + want] = reward;
    });
    assert_eq!(path[0] % 16, 0);
    for w in path.windows(2) {
        assert!((w[0] % 16).abs_diff(w[1] % 16) <= half_width, "{path:?}");
    }
    // It does arrive: 15 pitches in five moves of three.
    assert_eq!(path[frames - 1] % 16, 15);
}

/// The banded search finds the same best path as trying every path: all 8^4
/// over four random frames of 4 pitches in 2 layers with a half width of 1.
#[test]
fn the_banded_search_is_the_best_path() {
    let mut rng = SplitMix64::new(5);
    let model = banded(4, 2, 1, 0.7);
    for _ in 0..4 {
        let emit: Vec<[f64; 8]> = (0..4)
            .map(|_| core::array::from_fn(|_| uniform(&mut rng, 0.0, 1.0) * 6.0))
            .collect();
        let mut best = (f64::MIN, [0usize; 4]);
        for code in 0..8usize.pow(4) {
            let path: [usize; 4] = core::array::from_fn(|i| code / 8usize.pow(i as u32) % 8);
            let mut score = emit[0][path[0]];
            let mut possible = true;
            for t in 1..4 {
                match model.log_probability(path[t - 1] as u16, path[t] as u16) {
                    Some(step) => score += step + emit[t][path[t]],
                    None => possible = false,
                }
            }
            if possible && score > best.0 {
                best = (score, path);
            }
        }
        let found = viterbi(model.clone(), 4, |t, row| row.copy_from_slice(&emit[t]));
        let found: Vec<usize> = found.iter().map(|&s| usize::from(s)).collect();
        assert_eq!(found, best.1);
    }
}

/// Counts that do not fit a state index are refused.
#[test]
fn a_banded_model_too_large_for_a_state_index_is_refused() {
    assert!(BandedTransition::<f64>::triangular(400, 200, 1, -0.1, -2.0).is_none());
}
