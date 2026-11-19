//! Viterbi decoding under a sticky transition model.
//!
//! The best state path through a sequence of frames, where each frame scores
//! every state by an emission and the path pays one cost to stay in its state
//! and another to switch to any other. Every switch costs the same, so the
//! best predecessor of a state is its own previous score or the best of the
//! others, and a frame takes time linear in the number of states.

use tutti_buffers::Float;

/// The best state path over `frames` frames of `states` states (Viterbi, 1967),
/// as one state per frame.
///
/// `emission(frame, row)` fills `row` (`states` long) with the score of each
/// state at `frame`. `stay` is added to a state's score when the path remains
/// in it and `switch` when it moves to any other state; all three are log
/// scores in the float `F` the search runs in, so a larger sum is a likelier path. A state never counts as one of
/// the "other" states of itself, so a `switch` above `stay` does not let a
/// state switch into itself. Each frame's scores are shifted so the best is
/// zero, which keeps them exact over any number of frames.
///
/// A tie takes the state that stays, then the lower state. Fewer than two
/// states have nothing to switch to and decode to state 0 throughout. No
/// frames decode to an empty path. `states` is a `u16`, so a back pointer is
/// two bytes per frame and state. It allocates the table of back pointers
/// (`frames * states` of them) and the path, so it is for the control or
/// offline side, not the audio thread.
///
/// ```
/// use tutti_dsp::sticky_viterbi;
///
/// // Two states; the middle frame prefers state 1, but a switch costs more
/// // than that frame is worth.
/// let rows = [[0.0, -1.0], [-0.5, 0.0], [0.0, -1.0]];
/// let path = sticky_viterbi(2, 3, 0.0, -2.0, |t, row| row.copy_from_slice(&rows[t]));
/// assert_eq!(path, [0, 0, 0]);
/// ```
#[must_use]
pub fn sticky_viterbi<F: Float>(
    states: u16,
    frames: usize,
    stay: F,
    switch: F,
    mut emission: impl FnMut(usize, &mut [F]),
) -> Vec<u16> {
    let states = usize::from(states);
    if frames == 0 {
        return Vec::new();
    }
    if states < 2 {
        return vec![0; frames];
    }
    let mut score = vec![F::ZERO; states];
    let mut next = vec![F::ZERO; states];
    let mut row = vec![F::ZERO; states];
    emission(0, &mut score);
    let mut back = vec![0u16; frames * states];
    for t in 1..frames {
        // The two best states, so a state can take the best of the others.
        let (mut top, mut second) = (0usize, usize::MAX);
        for s in 1..states {
            if score[s] > score[top] {
                (top, second) = (s, top);
            } else if second == usize::MAX || score[s] > score[second] {
                second = s;
            }
        }
        emission(t, &mut row);
        for s in 0..states {
            let other = if s == top { second } else { top };
            let (from, via) = if score[s] + stay >= score[other] + switch {
                (s, score[s] + stay)
            } else {
                (other, score[other] + switch)
            };
            back[t * states + s] = from as u16;
            next[s] = via + row[s];
        }
        let best = next.iter().copied().fold(-F::MAX, F::max);
        for (s, n) in score.iter_mut().zip(&next) {
            *s = *n - best;
        }
    }

    let mut state = (0..states).fold(0, |b, s| if score[s] > score[b] { s } else { b });
    let mut path = vec![0u16; frames];
    for t in (0..frames).rev() {
        path[t] = state as u16;
        state = usize::from(back[t * states + state]);
    }
    path
}

#[cfg(test)]
mod tests;
