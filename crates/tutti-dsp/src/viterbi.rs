//! Viterbi decoding over a transition model.
//!
//! The best state path through a sequence of frames, where each frame scores
//! every state by an emission and a [`Transition`] scores each move between
//! frames. [`viterbi`] owns the search (the back pointers, the per-frame
//! shift of scores, the trace back); a model owns only how a frame's scores
//! turn into the best way into each state. Two models ship:
//!
//! - [`StickyTransition`]: the path pays one cost to stay and another to switch
//!   to any other state. Every switch costs the same, so the best predecessor
//!   of a state is its own previous score or the best of the others, and a
//!   frame takes time linear in the number of states ([`sticky_viterbi`]).
//! - [`BandedTransition`]: states are pitches in layers, a move changes the
//!   pitch by at most a half width with a triangular weight and may change the
//!   layer. A frame takes time `states * layers * (2 * half_width + 1)`.

use tutti_buffers::Float;

/// How a frame's scores turn into the best way into each state of the next.
///
/// A model has one boundary: for each state, the best predecessor and its
/// score after the move.
pub trait Transition<F: Float> {
    /// The number of states.
    fn states(&self) -> u16;

    /// For every state `s`, writes into `via[s]` the best `score[p] +
    /// log P(p -> s)` over predecessors `p` and into `from[s]` that `p`.
    /// `score`, `via` and `from` are `states()` long. A tie takes the
    /// predecessor the model names first in its own order.
    fn advance(&mut self, score: &[F], via: &mut [F], from: &mut [u16]);
}

/// The best state path over `frames` frames (Viterbi, 1967), as one state per
/// frame, under `transition`.
///
/// `emission(frame, row)` fills `row` (`transition.states()` long) with the
/// score of each state at `frame`. Scores are log scores in the float `F` the
/// search runs in, so a larger sum is a likelier path. Each frame's scores
/// are shifted so the best is zero, which keeps them exact over any number of
/// frames.
///
/// Fewer than two states have nothing to choose between and decode to state 0
/// throughout. No frames decode to an empty path. It allocates the table of
/// back pointers (`frames * states` of them, two bytes each) and the path, so
/// it is for the control or offline side, not the audio thread.
#[must_use]
pub fn viterbi<F: Float, T: Transition<F>>(
    mut transition: T,
    frames: usize,
    mut emission: impl FnMut(usize, &mut [F]),
) -> Vec<u16> {
    let states = usize::from(transition.states());
    if frames == 0 {
        return Vec::new();
    }
    if states < 2 {
        return vec![0; frames];
    }
    let mut score = vec![F::ZERO; states];
    let mut via = vec![F::ZERO; states];
    let mut row = vec![F::ZERO; states];
    emission(0, &mut score);
    let mut back = vec![0u16; frames * states];
    for t in 1..frames {
        transition.advance(&score, &mut via, &mut back[t * states..(t + 1) * states]);
        emission(t, &mut row);
        let mut best = -F::MAX;
        for s in 0..states {
            score[s] = via[s] + row[s];
            best = best.max(score[s]);
        }
        for s in &mut score {
            *s = *s - best;
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

/// Staying costs one log score and moving to any other state another.
///
/// A state never counts as one of the "other" states of itself, so a `switch`
/// above `stay` does not let a state switch into itself. A tie takes the state
/// that stays, then the lower state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StickyTransition<F> {
    states: u16,
    stay: F,
    switch: F,
}

impl<F: Float> StickyTransition<F> {
    /// `states` states; `stay` is added to a state's score when the path
    /// remains in it and `switch` when it moves to any other state.
    #[must_use]
    pub fn new(states: u16, stay: F, switch: F) -> Self {
        Self {
            states,
            stay,
            switch,
        }
    }
}

impl<F: Float> Transition<F> for StickyTransition<F> {
    fn states(&self) -> u16 {
        self.states
    }

    fn advance(&mut self, score: &[F], via: &mut [F], from: &mut [u16]) {
        let (stay, switch) = (self.stay, self.switch);
        // The two best states, so a state can take the best of the others.
        let (mut top, mut second) = (0usize, usize::MAX);
        for s in 1..score.len() {
            if score[s] > score[top] {
                (top, second) = (s, top);
            } else if second == usize::MAX || score[s] > score[second] {
                second = s;
            }
        }
        for s in 0..score.len() {
            let other = if s == top { second } else { top };
            let (p, v) = if score[s] + stay >= score[other] + switch {
                (s, score[s] + stay)
            } else {
                (other, score[other] + switch)
            };
            from[s] = p as u16;
            via[s] = v;
        }
    }
}

/// States that are `pitches` positions in each of `layers` layers (state
/// `layer * pitches + pitch`), where a move changes the pitch by at most
/// `half_width` positions and may change the layer.
///
/// The probability of `(layer, pitch)` to `(layer', pitch')` is the layer
/// term (`stay` when the layer is kept, `switch` for each other layer, log
/// scores) times the pitch term: a triangular weight `half_width + 1 -
/// |pitch' - pitch|` over the targets that are on the grid, normalised so the
/// pitch terms of a row sum to 1. A row of the matrix therefore sums to 1
/// when `exp(stay) + (layers - 1) * exp(switch) = 1`. A tie takes the lower
/// layer, then the lower pitch.
#[derive(Debug, Clone, PartialEq)]
pub struct BandedTransition<F> {
    pitches: u16,
    layers: u16,
    half_width: u16,
    stay: F,
    switch: F,
    /// `ln` of the triangular weight at each pitch distance `0..=half_width`.
    log_weight: Vec<F>,
    /// `ln` of the sum of weights over the on-grid targets of each pitch.
    log_norm: Vec<F>,
}

impl<F: Float> BandedTransition<F> {
    /// The model over `pitches` positions in `layers` layers, or `None` when
    /// `pitches * layers` does not fit the `u16` a state is counted in.
    #[must_use]
    pub fn triangular(
        pitches: u16,
        layers: u16,
        half_width: u16,
        stay: F,
        switch: F,
    ) -> Option<Self> {
        pitches.checked_mul(layers)?;
        let weight = |d: u16| F::from_f64(f64::from(half_width + 1 - d));
        let log_weight: Vec<F> = (0..=half_width).map(|d| weight(d).ln()).collect();
        let log_norm = (0..pitches)
            .map(|p| {
                let lo = p.saturating_sub(half_width);
                let hi = p.saturating_add(half_width).min(pitches - 1);
                (lo..=hi)
                    .map(|q| weight(p.abs_diff(q)))
                    .fold(F::ZERO, |a, b| a + b)
                    .ln()
            })
            .collect();
        Some(Self {
            pitches,
            layers,
            half_width,
            stay,
            switch,
            log_weight,
            log_norm,
        })
    }

    /// The log probability of the move `from -> to`, or `None` when `to` is
    /// farther than the half width in pitch (probability zero).
    #[must_use]
    pub fn log_probability(&self, from: u16, to: u16) -> Option<F> {
        let (from_layer, from_pitch) = (from / self.pitches, from % self.pitches);
        let (to_layer, to_pitch) = (to / self.pitches, to % self.pitches);
        let distance = from_pitch.abs_diff(to_pitch);
        if distance > self.half_width {
            return None;
        }
        let layer = if from_layer == to_layer {
            self.stay
        } else {
            self.switch
        };
        Some(
            layer + self.log_weight[usize::from(distance)] - self.log_norm[usize::from(from_pitch)],
        )
    }
}

impl<F: Float> Transition<F> for BandedTransition<F> {
    fn states(&self) -> u16 {
        self.pitches * self.layers
    }

    fn advance(&mut self, score: &[F], via: &mut [F], from: &mut [u16]) {
        for to in 0..self.states() {
            let to_pitch = to % self.pitches;
            let lo = to_pitch.saturating_sub(self.half_width);
            let hi = to_pitch
                .saturating_add(self.half_width)
                .min(self.pitches - 1);
            let mut best = (to, -F::MAX);
            for layer in 0..self.layers {
                for pitch in lo..=hi {
                    let p = layer * self.pitches + pitch;
                    let Some(step) = self.log_probability(p, to) else {
                        continue;
                    };
                    let v = score[usize::from(p)] + step;
                    if v > best.1 {
                        best = (p, v);
                    }
                }
            }
            let (p, v) = best;
            from[usize::from(to)] = p;
            via[usize::from(to)] = v;
        }
    }
}

/// [`viterbi`] under a [`StickyTransition`]: the best state path over `frames`
/// frames of `states` states, as one state per frame.
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
    emission: impl FnMut(usize, &mut [F]),
) -> Vec<u16> {
    viterbi(
        StickyTransition::new(states, stay, switch),
        frames,
        emission,
    )
}

#[cfg(test)]
mod tests;
