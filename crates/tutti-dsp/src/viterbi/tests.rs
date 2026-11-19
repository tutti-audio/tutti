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
