//! The controllers one channel holds, as one value.
//!
//! [`ChannelState`] is what a channel has been told that outlives its notes:
//! the 14-bit controllers (modulation, volume, pan, expression), channel
//! pressure, the pitch wheel with its sensitivity and tuning, the Modulation
//! Depth Range, and the portamento and legato switches with the pending
//! Portamento Control. [`VoiceAllocator`](crate::VoiceAllocator) keeps one for each of a
//! Group's sixteen channels and reads every message into it, so the two
//! instruments (and a future one) share one reading of them; an instrument asks
//! for the value it needs when it renders ([`VoiceAllocator::channel`](crate::VoiceAllocator::channel)).
//! The note logic that follows from them (a glide, a legato move) is the core's
//! and reaches the instrument as [`VoiceCommand`](crate::VoiceCommand)s.
//!
//! A channel's whole state is this value and nothing else of the core's is
//! per-channel controller state, so it can be handed on whole (a SoundFont swap
//! keeps what a performer set, and takes a new font).

use tutti_ump::{BendValue, ControlValue};
use tutti_ump::{CCNumber, NoteNumber, RegisteredData, Rpn, U7, U14, Voice2};
use tutti_units::{Bipolar, Cents, Normalized, Pan, Seconds, Semitones};

/// What a full Modulation Wheel spans until RPN 5 says otherwise: General MIDI
/// 2 section 3.4.4, default 00H/40H (+/-50 cents) (MSB semitones, LSB
/// 100/128 cent: 64 of them).
pub const DEFAULT_MODULATION_RANGE: Cents = Cents::new(50.0);

const MOD_WHEEL_LSB: CCNumber = CCNumber::MOD_WHEEL.lsb().unwrap();
const PORTAMENTO_TIME_LSB: CCNumber = CCNumber::PORTAMENTO_TIME.lsb().unwrap();
const VOLUME_LSB: CCNumber = CCNumber::VOLUME.lsb().unwrap();
const PAN_LSB: CCNumber = CCNumber::PAN.lsb().unwrap();
const EXPRESSION_LSB: CCNumber = CCNumber::EXPRESSION.lsb().unwrap();

/// A 14-bit controller with its most significant 7 bits replaced (CC 0 to 31).
/// The least significant 7 bits go to zero: M1 p.12, a receiver sets its concept of the
/// LSB to zero when an MSB is received.
fn with_msb(msb: U7) -> U14 {
    U14::from_halves(msb, U7::of(0))
}

/// A 14-bit controller with its least significant 7 bits replaced (CC 32 to 63).
fn with_lsb(current: U14, lsb: U7) -> U14 {
    U14::from_halves(current.msb(), lsb)
}

/// The reading of a 14-bit controller, `0.0..=1.0`.
fn position(value: U14) -> Normalized {
    Normalized::clamped(f64::from(value.get()) / f64::from(U14::MAX.get()))
}

/// A channel's pitch: the wheel, its sensitivity and the Channel Tuning
/// (RP-022: RPN 1 and 2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bend {
    /// The wheel, `-1..=1`.
    pub(crate) position: Bipolar,
    pub(crate) range: Semitones,
    /// Channel Coarse Tuning (RPN 2), whole semitones.
    pub(crate) coarse: Semitones,
    /// Channel Fine Tuning (RPN 1), under a semitone either way.
    pub(crate) fine: Semitones,
}

impl Bend {
    pub(crate) fn semitones(self) -> Semitones {
        self.range * self.position.get() + self.coarse + self.fine
    }
}

/// What a channel has been told, apart from its notes. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelState {
    modulation: U14,
    volume: U14,
    pan: U14,
    expression: U14,
    /// Channel Pressure.
    pressure: Normalized,
    modulation_range: Cents,
    pub(crate) bend: Bend,
    /// Portamento On/Off (CC 65).
    pub(crate) portamento: bool,
    /// Portamento Time (CC 5 and 37).
    portamento_time: U14,
    /// Legato Footswitch (CC 68).
    pub(crate) legato: bool,
    /// The source key of the next note's glide (CC 84), until a Note On uses it.
    pub(crate) glide_from: Option<NoteNumber>,
    /// The key last struck on the channel: what a glide with Portamento on
    /// starts from.
    pub(crate) last_key: Option<NoteNumber>,
    /// The tuning bank (RPN 4) and program (RPN 3) the channel plays by.
    pub(crate) tuning_bank: U7,
    pub(crate) tuning_program: U7,
}

impl ChannelState {
    /// The product of [`volume`](Self::volume) and [`expression`](Self::expression)
    /// at the defaults (Volume 100, Expression 127 of 16,383, as 14-bit values
    /// with a zero LSB): the level an instrument that gives a channel nothing to
    /// change takes as its own.
    pub const DEFAULT_LEVEL: Normalized =
        Normalized::of((100.0 * 128.0 / 16383.0) * (127.0 * 128.0 / 16383.0));

    /// The defaults a device starts with: General MIDI 2 section 3.3 (modulation
    /// 0, volume 100, pan 64, expression 127, portamento off with time 0, no
    /// pressure) and the Modulation Depth Range of section 3.4.4, with a pitch
    /// wheel that spans `bend_range`.
    #[must_use]
    pub fn new(bend_range: Semitones) -> Self {
        Self {
            modulation: U14::of(0),
            volume: with_msb(U7::of(100)),
            pan: with_msb(U7::of(64)),
            expression: with_msb(U7::of(127)),
            pressure: Normalized::of(0.0),
            modulation_range: DEFAULT_MODULATION_RANGE,
            bend: Bend {
                position: Bipolar::ZERO,
                range: bend_range,
                coarse: Semitones::new(0.0),
                fine: Semitones::new(0.0),
            },
            portamento: false,
            portamento_time: U14::of(0),
            legato: false,
            glide_from: None,
            last_key: None,
            tuning_bank: U7::of(0),
            tuning_program: U7::of(0),
        }
    }

    /// Reset All Controllers (RP-015) for the controllers this value holds:
    /// Expression (#11) to 127, Modulation (#1) to 0, the pedals (#64, #65,
    /// #66, #67) to 0 and channel pressure to 0, and nothing else: not Volume
    /// (#7), Pan (#10), the sound controllers, nor the registered or
    /// non-registered parameters (the sensitivity, the tuning and the
    /// Modulation Depth Range stay), nor the Legato Footswitch (#68), which the
    /// list does not name. The pitch wheel centres ([`VoiceAllocator`](crate::VoiceAllocator)
    /// does it, for it tells the instrument), and the pedals #64 and #66 are
    /// the note logic's.
    pub(crate) fn reset_controllers(&mut self) {
        self.modulation = U14::of(0);
        self.expression = with_msb(U7::of(127));
        self.pressure = Normalized::of(0.0);
        self.portamento = false;
    }

    /// Sets what Control Change `controller` at `value` sets in the value, if
    /// it is one of the 14-bit controllers or Portamento Time. A Note On or the
    /// notes are never its business.
    pub(crate) fn control(&mut self, controller: CCNumber, value: U7) {
        match controller {
            CCNumber::MOD_WHEEL => self.modulation = with_msb(value),
            MOD_WHEEL_LSB => self.modulation = with_lsb(self.modulation, value),
            CCNumber::VOLUME => self.volume = with_msb(value),
            VOLUME_LSB => self.volume = with_lsb(self.volume, value),
            CCNumber::PAN => self.pan = with_msb(value),
            PAN_LSB => self.pan = with_lsb(self.pan, value),
            CCNumber::EXPRESSION => self.expression = with_msb(value),
            EXPRESSION_LSB => self.expression = with_lsb(self.expression, value),
            CCNumber::PORTAMENTO_TIME => self.portamento_time = with_msb(value),
            PORTAMENTO_TIME_LSB => {
                self.portamento_time = with_lsb(self.portamento_time, value);
            }
            _ => {}
        }
    }

    /// Channel Pressure.
    pub(crate) fn set_pressure(&mut self, pressure: Normalized) {
        self.pressure = pressure;
    }

    /// The Modulation Depth Range (RPN 5, CA-026) from its data word, MSB
    /// semitones and LSB 100/128 cent (GM2 section 3.4.4).
    pub(crate) fn set_modulation_range(&mut self, data: RegisteredData) {
        let (semitones, eighths) = (f64::from(data.msb().get()), f64::from(data.lsb().get()));
        self.modulation_range = Cents::new(semitones * 100.0 + eighths * 100.0 / 128.0);
    }

    /// Writes what this value holds that `fresh` does not, as the messages
    /// that set it (the chase of a node swap, [`VoiceAllocator::chase`](crate::VoiceAllocator::chase)):
    /// each 14-bit controller as its MSB then, when nonzero, its LSB (M1
    /// p.12: an MSB zeroes the LSB), Portamento On and the Legato Footswitch
    /// as switches, the pending Portamento Control, Channel Pressure, the
    /// pitch wheel, and RPN 0 to 5 as MIDI 2.0 Registered Controllers
    /// with the data words [`VoiceAllocator::step`](crate::VoiceAllocator::step) reads them
    /// from. A value equal to `fresh`'s is left out, so a channel nobody
    /// touched writes nothing. At most [`ChannelState::CHASE_MESSAGES`].
    ///
    /// Not carried: the key last struck, which a portamento glides from.
    pub(crate) fn chase(&self, fresh: &Self, mut emit: impl FnMut(Voice2)) {
        let cc = |controller: CCNumber, value: U7| Voice2::ControlChange {
            controller,
            value: ControlValue::from(value),
        };
        let mut fourteen = |msb: CCNumber, now: U14, then: U14| {
            if now == then {
                return;
            }
            emit(cc(msb, now.msb()));
            if let (Some(lsb), low) = (msb.lsb(), now.lsb())
                && low.get() != 0
            {
                emit(cc(lsb, low));
            }
        };
        fourteen(CCNumber::MOD_WHEEL, self.modulation, fresh.modulation);
        fourteen(CCNumber::VOLUME, self.volume, fresh.volume);
        fourteen(CCNumber::PAN, self.pan, fresh.pan);
        fourteen(CCNumber::EXPRESSION, self.expression, fresh.expression);
        fourteen(
            CCNumber::PORTAMENTO_TIME,
            self.portamento_time,
            fresh.portamento_time,
        );
        let switch = |on: bool| U7::of(if on { 127 } else { 0 });
        if self.portamento != fresh.portamento {
            emit(cc(CCNumber::PORTAMENTO_SWITCH, switch(self.portamento)));
        }
        if self.legato != fresh.legato {
            emit(cc(CCNumber::LEGATO, switch(self.legato)));
        }
        if let Some(key) = self.glide_from {
            emit(cc(CCNumber::PORTAMENTO_CONTROL, U7::masked(key.get())));
        }
        if self.pressure != fresh.pressure {
            emit(Voice2::ChannelPressure(ControlValue::from_normalized(
                self.pressure,
            )));
        }
        if self.bend.position != fresh.bend.position {
            emit(Voice2::PitchBend(BendValue::from_bipolar(
                self.bend.position,
            )));
        }
        for rpn in Self::REGISTERED {
            let (now, then) = (self.registered_data(rpn), fresh.registered_data(rpn));
            if let (Some(data), true) = (now, now != then) {
                emit(Voice2::Registered {
                    bank: rpn.bank(),
                    index: rpn.index(),
                    value: ControlValue::new(data.to_word()),
                });
            }
        }
    }

    /// The Registered Controllers this state holds: RPN 0, 1, 2, 3, 4 and 5.
    const REGISTERED: [Rpn; 6] = [
        Rpn::PITCH_BEND_SENSITIVITY,
        Rpn::CHANNEL_FINE_TUNING,
        Rpn::CHANNEL_COARSE_TUNING,
        Rpn::TUNING_PROGRAM_SELECT,
        Rpn::TUNING_BANK_SELECT,
        Rpn::MODULATION_DEPTH_RANGE,
    ];

    /// The data [`VoiceAllocator::step`](crate::VoiceAllocator::step) would read `rpn`'s
    /// value from, so that the state is the value a Registered Controller set
    /// (the inverse of what it reads), or `None` for a parameter this state
    /// does not hold. The data carries only the resolution the parameter has:
    /// the bits below it are zero (M2-104 section 7.4.7: they are undefined
    /// and ignored).
    pub(crate) fn registered_data(&self, rpn: Rpn) -> Option<RegisteredData> {
        match rpn {
            Rpn::PITCH_BEND_SENSITIVITY => {
                // MSB semitones, LSB cents: `bend_sensitivity`'s inverse.
                let hundredths =
                    libm::round(self.bend.range.get() * 100.0).clamp(0.0, 12_799.0) as u32;
                Some(RegisteredData::new(
                    U7::masked((hundredths / 100) as u8),
                    U7::masked((hundredths % 100) as u8),
                ))
            }
            Rpn::CHANNEL_FINE_TUNING => {
                let fourteen =
                    libm::round((self.bend.fine.get() + 1.0) * 8192.0).clamp(0.0, 16_383.0);
                Some(RegisteredData::from_fourteen(U14::masked(fourteen as u16)))
            }
            Rpn::CHANNEL_COARSE_TUNING => {
                let msb = libm::round(self.bend.coarse.get() + 64.0).clamp(0.0, 127.0);
                Some(RegisteredData::new(U7::masked(msb as u8), U7::of(0)))
            }
            Rpn::TUNING_PROGRAM_SELECT => Some(RegisteredData::new(self.tuning_program, U7::of(0))),
            Rpn::TUNING_BANK_SELECT => Some(RegisteredData::new(self.tuning_bank, U7::of(0))),
            Rpn::MODULATION_DEPTH_RANGE => {
                // MSB semitones, LSB 100/128 cent: `set_modulation_range`'s inverse.
                let cents = self.modulation_range.get().clamp(0.0, 12_700.0);
                let semitones = libm::floor(cents / 100.0);
                let eighths = libm::round((cents - semitones * 100.0) * 128.0 / 100.0).min(127.0);
                Some(RegisteredData::new(
                    U7::masked(semitones as u8),
                    U7::masked(eighths as u8),
                ))
            }
            _ => None,
        }
    }

    /// The most messages `ChannelState::chase` writes: ten 14-bit halves, two
    /// switches, a Portamento Control, pressure, the wheel and six registered
    /// controllers.
    pub const CHASE_MESSAGES: usize = 21;

    /// The tuning bank the channel plays by (RPN 4), 0 until set.
    #[must_use]
    pub fn tuning_bank(&self) -> U7 {
        self.tuning_bank
    }

    /// The tuning program the channel plays by (RPN 3), 0 until set.
    #[must_use]
    pub fn tuning_program(&self) -> U7 {
        self.tuning_program
    }

    /// The Modulation Wheel's position, `0.0..=1.0` (CC 1 and 33).
    #[must_use]
    pub fn modulation(&self) -> Normalized {
        position(self.modulation)
    }

    /// What a full Modulation Wheel spans: the Modulation Depth Range (RPN 5),
    /// [`DEFAULT_MODULATION_RANGE`] until set.
    #[must_use]
    pub fn modulation_range(&self) -> Cents {
        self.modulation_range
    }

    /// The modulation depth now: the wheel's position over the range. General
    /// MIDI 2 section 3.3.2: the depth of change at the maximum value is the
    /// Modulation Depth Range, linear in cents.
    #[must_use]
    pub fn modulation_depth(&self) -> Cents {
        self.modulation_range * self.modulation().get()
    }

    /// Channel Volume's position, `0.0..=1.0` (CC 7 and 39). The law that
    /// turns it into a gain is the instrument's.
    #[must_use]
    pub fn volume(&self) -> Normalized {
        position(self.volume)
    }

    /// Expression's position, `0.0..=1.0` (CC 11 and 43).
    #[must_use]
    pub fn expression(&self) -> Normalized {
        position(self.expression)
    }

    /// Channel Pressure.
    #[must_use]
    pub fn pressure(&self) -> Normalized {
        self.pressure
    }

    /// The stereo position of CC 10 (and 42), by RP-036: the effective range
    /// for CC#10 is 1 to 127, values 0 and 1 both pan hard left, and the
    /// recommended method is to subtract 1 and saturate at zero, so 64 is true centre
    /// (`Pan::CENTER`), 0 and 1 are `Pan::LEFT` and 127 is `Pan::RIGHT`. The
    /// 14-bit value is read the same way, the subtracted 1 being 128 of its
    /// steps.
    ///
    /// The left and right gains are `cos` and `sin` of `Pi/2` times
    /// `(pan + 1) / 2`: RP-036's `cos (Pi/2 * max(0,CC#10 - 1)/126)` and the
    /// `sin`. Constant power.
    #[must_use]
    pub fn pan(&self) -> Pan {
        const STEPS: f64 = 126.0 * 128.0;
        let along = (f64::from(self.pan.get().saturating_sub(128)) / STEPS).min(1.0);
        Pan::new(2.0 * along - 1.0)
    }

    /// Where the channel's pitch stands: the wheel over its sensitivity, and the
    /// Channel Tuning (RP-022).
    #[must_use]
    pub fn pitch(&self) -> Semitones {
        self.bend.semitones()
    }

    /// Whether the Legato Footswitch (CC 68) is down.
    #[must_use]
    pub fn legato(&self) -> bool {
        self.legato
    }

    /// Whether Portamento (CC 65) is on.
    #[must_use]
    pub fn portamento(&self) -> bool {
        self.portamento
    }

    /// How fast a glide goes, in cents a millisecond, from Portamento Time (CC
    /// 5 and 37). General MIDI 2 section 3.3.3 gives a recommended example
    /// as a figure on a log axis from 0.01 to 1,000 cent/msec, falling from the
    /// first value to the last; `PORTAMENTO_RATE` is its curve read at the
    /// points below and interpolated in log, so the end points are the figure's
    /// own. The default time, 0, is the fastest.
    #[must_use]
    pub(crate) fn glide_rate(&self) -> f64 {
        let at = (f64::from(self.portamento_time.get()) / 128.0).min(127.0);
        let i = PORTAMENTO_RATE
            .iter()
            .rposition(|&(v, _)| v <= at)
            .unwrap_or(0)
            .min(PORTAMENTO_RATE.len() - 2);
        let ((v0, r0), (v1, r1)) = (PORTAMENTO_RATE[i], PORTAMENTO_RATE[i + 1]);
        let along = ((at - v0) / (v1 - v0)).clamp(0.0, 1.0);
        r0 * libm::pow(r1 / r0, along)
    }

    /// How long a glide from key `from` to key `to` takes at the channel's
    /// portamento rate: the interval in cents (100 to the key) over
    /// [`glide_rate`](Self::glide_rate).
    pub(crate) fn glide_time(&self, from: NoteNumber, to: NoteNumber) -> Seconds {
        let interval = Cents::from(Semitones::new(f64::from(from.get().abs_diff(to.get()))));
        Seconds::new(interval.get() / (self.glide_rate() * 1000.0))
    }
}

/// General MIDI 2 section 3.3.3's recommended portamento rate: (Portamento
/// Time, cents a millisecond), read off the figure. The first and last are the
/// figure's axis ends, 1,000 and 0.01; the points between are where the curve
/// crosses the gridlines.
const PORTAMENTO_RATE: [(f64, f64); 7] = [
    (0.0, 1000.0),
    (8.0, 30.0),
    (32.0, 8.0),
    (64.0, 2.0),
    (96.0, 0.5),
    (120.0, 0.1),
    (127.0, 0.01),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn pan_at(controller: u8) -> Pan {
        let mut c = ChannelState::new(Semitones::new(2.0));
        c.control(CCNumber::PAN, U7::of(controller));
        c.pan()
    }

    /// RP-036: 64 (40H) is centre, values 0 and 1 both pan hard left, and
    /// 127 is hard right; the default, 64, is the centre.
    #[test]
    fn pan_64_is_true_centre_and_0_and_1_are_hard_left() {
        assert_eq!(ChannelState::new(Semitones::new(2.0)).pan(), Pan::CENTER);
        assert_eq!(pan_at(64), Pan::CENTER);
        assert_eq!(pan_at(0), Pan::LEFT);
        assert_eq!(pan_at(1), Pan::LEFT);
        assert_eq!(pan_at(127), Pan::RIGHT);
        assert!(pan_at(2) > Pan::LEFT);
    }

    /// RP-036's gains, `cos (Pi/2 * max(0,CC#10 - 1)/126)` and the `sin`, for
    /// every value of CC 10, from the formula as written.
    #[test]
    fn the_pan_angle_is_rp_036s_for_every_value() {
        for cc in 0..=127u8 {
            let expected = std::f64::consts::FRAC_PI_2 * f64::from(cc.saturating_sub(1)) / 126.0;
            let angle = std::f64::consts::FRAC_PI_4 * (pan_at(cc).get() + 1.0);
            assert!((angle - expected).abs() < 1e-14, "CC 10 = {cc}");
        }
    }

    /// General MIDI 2 section 3.3.3's figure, read in log: halfway along a
    /// segment of the curve the rate is the geometric mean of its ends, and a
    /// glide of `n` keys takes `100 n / rate` ms.
    #[test]
    fn the_glide_rate_is_the_geometric_mean_halfway_along_a_segment() {
        let mut c = ChannelState::new(Semitones::new(2.0));
        // CC 5 = 20 is halfway from the curve's (8, 30) to its (32, 8) cent/msec.
        c.control(CCNumber::PORTAMENTO_TIME, U7::of(20));
        let expected = (30.0f64 * 8.0).sqrt();
        // `pow` rounds: a few ulps from the exact mean.
        assert!((c.glide_rate() - expected).abs() < 1e-12 * expected);
        let time = c.glide_time(NoteNumber::of(60), NoteNumber::of(72));
        assert!((time.get() - 1200.0 / expected / 1000.0).abs() < 1e-12 * time.get());
        // The curve's own end points: CC 5 = 0 and 127.
        c.control(CCNumber::PORTAMENTO_TIME, U7::of(0));
        assert_eq!(c.glide_rate(), 1000.0);
        c.control(CCNumber::PORTAMENTO_TIME, U7::of(127));
        assert!((c.glide_rate() - 0.01).abs() < 1e-12 * 0.01);
    }

    /// M1 p.12: a receiver sets its concept of the LSB to zero when an MSB is
    /// received. After MSB 100, LSB 64, MSB 100 the 14-bit value is
    /// 100 * 128, not 100 * 128 + 64.
    #[test]
    fn an_msb_sets_the_lsb_to_zero() {
        let mut c = ChannelState::new(Semitones::new(2.0));
        c.control(CCNumber::VOLUME, U7::of(100));
        c.control(VOLUME_LSB, U7::of(64));
        c.control(CCNumber::VOLUME, U7::of(100));
        assert_eq!(c.volume(), position(U14::of(100 * 128)));
    }

    /// GM2 section 3.4.4: the default Modulation Depth Range is 00H/40H, +/-50
    /// cents, MSB 00H with LSB 08H is 6.25 cents, and MSB 01H with LSB 00H is a
    /// semitone. A full wheel reaches the range.
    #[test]
    fn the_modulation_depth_range_follows_gm2() {
        let mut c = ChannelState::new(Semitones::new(2.0));
        assert_eq!(c.modulation_range(), Cents::new(50.0));
        c.set_modulation_range(RegisteredData::new(U7::of(0), U7::of(8)));
        assert_eq!(c.modulation_range(), Cents::new(6.25));
        c.set_modulation_range(RegisteredData::new(U7::of(1), U7::of(0)));
        assert_eq!(c.modulation_range(), Cents::new(100.0));
        c.control(CCNumber::MOD_WHEEL, U7::of(127));
        c.control(MOD_WHEEL_LSB, U7::of(127));
        assert_eq!(c.modulation_depth(), Cents::new(100.0));
    }

    /// GM2 section 3.3.3's figure: the rate falls from 1,000 cent/msec at 0 to
    /// 0.01 at 127, and never rises.
    #[test]
    fn the_portamento_rate_falls_from_the_figures_ends() {
        let rate = |time: u8| {
            let mut c = ChannelState::new(Semitones::new(2.0));
            c.control(CCNumber::PORTAMENTO_TIME, U7::of(time));
            c.glide_rate()
        };
        assert_eq!(rate(0), 1000.0);
        assert!((rate(127) - 0.01).abs() < 1e-12 * 0.01);
        for t in 1..=127u8 {
            assert!(rate(t) < rate(t - 1), "time {t}");
        }
    }
}
