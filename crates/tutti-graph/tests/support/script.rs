//! A scripted transport for the random-graph suites.

use crate::generator::Rng;

/// A transport, played: it rolls at `tempo`, and `block` sometimes seeks,
/// stops or starts, changes the tempo inside a block (a linear ramp, or a
/// step at a random offset), or toggles a short loop.
pub struct Script {
    pub beat: f64,
    pub playing: bool,
    pub tempo: f64,
    pub looping: Option<(f64, f64)>,
}

impl Script {
    /// The transport for the next block of `n` frames at 48 kHz, then
    /// advance past it.
    pub fn block(&mut self, n: usize, rng: &mut Rng) -> tutti_graph::TransportPosition {
        // A tempo change is ramped linearly across this block, as a host
        // automating tempo reports it: the block says its starting tempo,
        // and the position advances by the average.
        let mut next_tempo = self.tempo;
        match rng.below(40) {
            0 | 1 => self.beat = rng.below(4000) as f64 / 1000.0,
            2 => self.playing = !self.playing,
            3 | 4 => next_tempo = 600.0 + rng.below(1800) as f64,
            5 | 6 => {
                self.looping = match self.looping {
                    Some(_) => None,
                    None => {
                        let start = (self.beat + rng.below(100) as f64 / 1000.0 - 0.02).max(0.0);
                        Some((start, start + 0.05 + rng.below(250) as f64 / 1000.0))
                    }
                }
            }
            _ => {}
        }
        let t = tutti_graph::TransportPosition::new(
            self.playing,
            tutti_units::Bpm::of(self.tempo),
            tutti_units::Beat::new(self.beat),
            self.looping.map(|(start, end)| {
                tutti_music::LoopRange::new(
                    tutti_units::Beat::new(start),
                    tutti_units::Beat::new(end),
                )
                .expect("a loop")
            }),
        );
        // Half the changes ramp linearly across the block; half step at a
        // random offset inside it. Either way the block reports its starting
        // tempo, and the next block the new one.
        let beats = |frames: f64, tempo: f64| frames * tempo / 60.0 / 48_000.0;
        let advance = if next_tempo != self.tempo && rng.chance(50) {
            let at = rng.below(n as u64 + 1) as f64;
            beats(at, self.tempo) + beats(n as f64 - at, next_tempo)
        } else {
            beats(n as f64, 0.5 * (self.tempo + next_tempo))
        };
        self.tempo = next_tempo;
        if self.playing {
            let x = self.beat + advance;
            self.beat = match self.looping {
                Some((start, end)) if self.beat < end && x >= end => {
                    start + (x - end) % (end - start)
                }
                _ => x,
            };
        }
        t
    }
}
