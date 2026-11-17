//! [`GateBank`]: the noise gate over buffers views.
//!
//! A gate reads a **key**: the side-chain as wide as the signal (an unwired one is
//! silence and holds the gate closed) or the signal itself, as the caller hands the bank
//! its [`KeySource`](super::KeySource). It finds the key's level (the loudest channel at
//! each frame), decides *open* when the level is at or over the threshold, and runs a
//! follower on that decision: an attack toward 1 while open, a hold of whole frames after
//! the level falls, then a release toward 0. The signal is multiplied, on **every**
//! channel, by one gain between the closed floor (the `range`) and 1:
//! `range + open * (1 - range)`. The gain is linked, so the stereo image does not move when
//! one side is louder.
//!
//! The decision has no hysteresis of its own: the hold is what stops a gate chattering on a
//! key that hovers at the threshold.
//!
//! The threshold is the one control read **per frame** when the graph drives it
//! ([`GateLanes`]): it is a level to compare against, not a coefficient. The range multiplies
//! the output directly, so a step in it is a click: it is a ramp, a control moving to a
//! target, and glides over [`GAIN_GLIDE`] as a [`Glide`]. The times only move what the
//! follower is smoothing toward, and the follower is the smoother: they land whole.
//!
//! The gate has no latency (the detector is on the key, never ahead of it) and no tail (the
//! gain only scales, so a silent signal is a silent output whatever the follower is doing).

use tutti_buffers::{Float, Mono, Planes, PlanesMut, Sample};
use tutti_types::{Amplitude, ChannelLayout, Db, Hz, Q, SampleRate, Seconds};

use super::curves::{amplitude_to_db, gate_gain_at};
use super::key_filter::{KeyFilter, KeyStage};
use super::shared::{Ballistics, CHUNK, GAIN_GLIDE, key_levels};
use crate::delay::finite_or;
use crate::smoother::Smoother;
use crate::{Glide, Steps};

/// The controls of a [`GateBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateControls {
    /// The key level, in dB, at or above which the gate opens.
    pub threshold: Db,
    /// How fast the gate opens: the time constant of a one-pole smoother toward 1. Below
    /// 0 reads as 0 (instant).
    pub attack: Seconds,
    /// How long the gate stays fully open after the key falls under the threshold: whole
    /// frames, `floor(hold * rate)`. Below 0 reads as 0. Hold is what stops a gate
    /// chattering on a key that hovers at the threshold.
    pub hold: Seconds,
    /// How fast the gate closes once the hold is over: the time constant of a one-pole
    /// smoother toward 0. Below 0 reads as 0.
    pub release: Seconds,
    /// The gain of a closed gate, in dB, at most 0: a floor, not a mute (`-80` is
    /// effectively silent, a gentler value ducks, `0` is no gate at all). Above 0 reads
    /// as 0.
    pub range: Db,
    /// The cutoff of the key filter: read only by a bank built with
    /// [`key_filter`](GateBank::key_filter). Held in the SVF's range.
    pub key_cutoff: Hz,
    /// The Q of the key filter, read as `key_cutoff` is.
    pub key_q: Q,
}

impl GateControls {
    /// A gate that passes everything: threshold at -40 dB, no times and a range of 0 dB.
    /// What a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        threshold: Db(-40.0),
        attack: Seconds(0.0),
        hold: Seconds(0.0),
        release: Seconds(0.0),
        range: Db(0.0),
        key_cutoff: KeyFilter::DEFAULT_CUTOFF,
        key_q: KeyFilter::DEFAULT_Q,
    };
}

impl Default for GateControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The figures the detector runs on, which land whole: the comparison, the times as
/// one-pole coefficients at the rate, the hold in frames and the closed gain.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Design {
    /// The threshold as a linear level: the detector compares in the linear domain, so no
    /// `log10` runs per frame.
    threshold_linear: f32,
    /// Whether a silent key (pinned at [`Db::FLOOR`]) is at or above the threshold. Only
    /// a threshold at or under the floor opens on silence.
    silence_opens: bool,
    /// The one-pole coefficient while the gate opens.
    attack: f32,
    /// The one-pole coefficient while it closes.
    release: f32,
    /// `floor(hold * rate)`.
    hold_samples: usize,
    /// The closed gain's level in dB, at most 0.
    range: Db,
}

impl Design {
    /// The design of `c` at `rate`: a non-finite control is the default's, the times are
    /// floored at 0 and the range is capped at 0 dB.
    fn of(c: &GateControls, rate: SampleRate) -> Self {
        let d = GateControls::DEFAULT;
        let threshold = Db(finite_or(c.threshold.get(), d.threshold.get()));
        // A NaN hold reads as 0 (`max` ignores it); an infinite one saturates to the
        // longest count. `Ballistics::of` floors the attack and release.
        let hold = Seconds(c.hold.get().max(0.0));
        let ballistics = Ballistics::of(c.attack, c.release, rate);
        Self {
            threshold_linear: threshold.to_amplitude().get(),
            silence_opens: Db::FLOOR >= threshold,
            attack: ballistics.attack,
            release: ballistics.release,
            hold_samples: hold.to_samples_floor(rate).get(),
            range: Db(finite_or(c.range.get(), d.range.get()).min(0.0)),
        }
    }
}

/// The decision against the member's threshold, in the linear domain.
///
/// Exact equality is unreachable: `10^(t/20)` rounds, so no level a test can name sits on
/// the threshold, and `>=` versus `>` cannot be pinned by a test.
#[inline]
fn opens(level: f32, threshold_linear: f32, silence_opens: bool) -> bool {
    level >= threshold_linear || (level <= 0.0 && silence_opens)
}

/// The per-frame lane a gate reads beside its audio, in the audio's sample format, `None`
/// where nothing drives it. A lane is at least as long as the frames a call renders; a frame
/// past its end is not driven.
#[derive(Clone, Copy, Debug, Default)]
pub struct GateLanes<'a, S: Float = f32> {
    /// The threshold in dB. A frame that is not finite is the aimed threshold.
    pub threshold: Option<Mono<'a, S>>,
}

/// A noise gate of any width: one open/closed decision from the loudest key channel, one
/// gain applied to every channel, with the follower and the hold that smooth it and the
/// range that glides.
///
/// Built on the control side for one rate; the state is `f32`, and a view of another sample
/// format is converted at the edges. Nothing it holds grows.
#[derive(Clone, Debug)]
pub struct GateBank {
    design: Design,
    /// The closed gain in dB, gliding over [`GAIN_GLIDE`].
    range: Glide<f32>,
    /// The steps a range glide takes: [`GAIN_GLIDE`] at the rate.
    steps: u32,
    rate: SampleRate,
    /// The smoothed open fraction, 0 closed to 1 open.
    follower: f32,
    /// The frames of hold left.
    hold_counter: usize,
    /// The filter on the key, if the bank has one.
    key: Option<KeyStage>,
}

impl GateBank {
    /// A gate that passes everything at `rate`, to be aimed.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        // 5 ms is a few hundred frames at any real rate; an absurd rate saturates rather
        // than panic, since a prepare must not.
        let steps = u32::try_from(GAIN_GLIDE.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        let design = Design::of(&GateControls::DEFAULT, rate);
        Self {
            range: Glide::landed(design.range.get()),
            design,
            steps,
            rate,
            follower: 0.0,
            hold_counter: 0,
            key: None,
        }
    }

    /// This bank with the detector hearing its key through `filter`, over `width` key
    /// channels at the cutoff and Q [`aim`](Self::aim) gives it. [`KeyFilter::Off`] is no
    /// filter; key channels past `width` are not heard. Allocates: build it on the control
    /// side.
    #[must_use]
    pub fn key_filter(mut self, filter: KeyFilter, width: ChannelLayout) -> Self {
        self.key = filter
            .response()
            .map(|ty| KeyStage::new(ty, width, self.rate));
        self
    }

    /// Back to silence: the gate closed and no hold left, and the range landed on its
    /// target. No allocation.
    pub fn reset(&mut self) {
        self.follower = 0.0;
        self.hold_counter = 0;
        self.range.settle();
        if let Some(key) = &mut self.key {
            key.reset();
        }
    }

    /// Lands the range on its target now.
    pub fn settle(&mut self) {
        self.range.settle();
        if let Some(key) = &mut self.key {
            key.settle();
        }
    }

    /// Aims at `controls`: the detector's figures land whole (the follower smooths them) and
    /// the range glides over [`GAIN_GLIDE`]. The key filter, if any, glides to the cutoff and
    /// Q over one grid period ([`COEFF_INTERVAL`](crate::COEFF_INTERVAL)); a non-finite one is
    /// the default's. No allocation.
    pub fn aim(&mut self, controls: &GateControls) {
        self.design = Design::of(controls, self.rate);
        if let Some(key) = &mut self.key {
            let d = GateControls::DEFAULT;
            key.aim(
                Hz(finite_or(controls.key_cutoff.get(), d.key_cutoff.get())),
                Q(finite_or(controls.key_q.get(), d.key_q.get())),
            );
        }
        self.range
            .aim_by(self.design.range.get(), Steps::Fixed(self.steps));
    }

    /// Runs `input` into `output`, scaled by one gain decided from the loudest channel of
    /// `key`.
    ///
    /// `key` is the signal the detector listens to: the side-chain bus, or `input` itself
    /// for a gate on its own signal, through the key filter if the bank has one (the signal
    /// it gains is never filtered). Channel `c` of `input` feeds channel `c` of `output`;
    /// a channel past the narrower of the two views is not touched, and the frames rendered
    /// are the fewest of the three views'. A NaN key sample reads as quiet and an infinite one
    /// as the largest finite level. Frame `i` reads the range at the glide's step
    /// `taken + 1 + i`, then the block's steps are taken. No allocation.
    pub fn process<S: Float>(
        &mut self,
        lanes: GateLanes<'_, S>,
        key: Planes<'_, S>,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input
            .frames()
            .get()
            .min(output.frames().get())
            .min(key.frames().get());
        let width = input.channels().min(output.channels());
        let d = self.design;
        let blended = self.range.blended(frames);
        let taken = self.range.taken();
        let held = Db(self.range.target()).to_amplitude().get();
        let mut gains = [0.0f32; CHUNK];
        let mut start = 0;
        while start < frames {
            let n = (frames - start).min(CHUNK);
            let end = start + n;
            let gains = &mut gains[..n];
            // The key's level: the loudest channel at each frame, after the key filter.
            match &mut self.key {
                Some(stage) => stage.levels(&key, start..end, gains),
                None => key_levels(&key, start..end, gains),
            }
            for (k, g) in gains.iter_mut().enumerate() {
                let i = start + k;
                let level = *g;
                let open = match lanes
                    .threshold
                    .and_then(|l| l.samples().get(i))
                    .map(|x| Db(x.convert_to::<f32>()))
                    .filter(|t| t.get().is_finite())
                {
                    Some(t) => amplitude_to_db(Amplitude::new(level)) >= t,
                    None => opens(level, d.threshold_linear, d.silence_opens),
                };
                if open {
                    self.hold_counter = d.hold_samples;
                    self.follower = Smoother::toward(self.follower, 1.0, d.attack);
                } else if self.hold_counter > 0 {
                    self.hold_counter -= 1;
                } else {
                    self.follower = Smoother::toward(self.follower, 0.0, d.release);
                }
                let range = if i < blended {
                    let db = self
                        .range
                        .at(taken.saturating_add(1).saturating_add(i as u32));
                    Db(db).to_amplitude().get()
                } else {
                    held
                };
                *g = gate_gain_at(self.follower, range).get();
            }
            for c in 0..width {
                let xs = &input.plane(c).samples()[start..end];
                let mut plane = output.plane_mut(c);
                let ys = &mut plane.samples_mut()[start..end];
                for ((y, x), &g) in ys.iter_mut().zip(xs).zip(gains.iter()) {
                    *y = (x.convert_to::<f32>() * g).convert_to::<S>();
                }
            }
            start = end;
        }
        self.range
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}

#[cfg(test)]
mod tests;
