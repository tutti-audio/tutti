//! [`CCNumber`]: which Control Change controller a message addresses.
//!
//! An address, not a quantity: the index selects a controller, and the value
//! it carries is a [`ControlValue`](crate::ControlValue) (MIDI 2.0) or a
//! [`U7`](crate::U7) (MIDI 1.0). The index is 7 bits in both protocols
//! (M2-104 §7.4.6), so the range is `0..=127`.

tutti_units::ranged! {
    /// One of the 128 MIDI Control Change controllers.
    ///
    /// Always in `0..=MAX`: [`new`](Self::new) refuses a larger number, and
    /// [`masked`](Self::masked) is the one constructor that masks. Serialized
    /// as the bare number, read back through `TryFrom<u8>`, so a document
    /// cannot carry a value the type refuses.
    #[repr(transparent)]
    #[derive(Default, Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    CCNumber(u8), CCNumberOutOfRange, bits 7
}

impl CCNumber {
    /// The first controller number. Also [`BANK_SELECT`](Self::BANK_SELECT).
    pub const FIRST: CCNumber = CCNumber(0);
    /// The last controller number. (Poly Mode On, in the channel-mode block.)
    pub const LAST: CCNumber = CCNumber(127);
    /// How many Control Change controllers the format defines.
    pub const COUNT: u8 = 128;

    // --- The named roster ---------------------------------------------------
    //
    // These live on the type rather than as loose module constants so that
    // `MOD_WHEEL` cannot be handed to something expecting a channel: that is
    // the whole payoff of the newtype.
    //
    // Numbers are from the MIDI 1.0 Control Change table (and carried forward
    // unchanged by M2-104 §7.4.6).

    // Continuous controllers (MSB).
    /// CC 0.
    pub const BANK_SELECT: CCNumber = CCNumber(0);
    /// CC 1.
    pub const MOD_WHEEL: CCNumber = CCNumber(1);
    /// CC 2.
    pub const BREATH: CCNumber = CCNumber(2);
    /// CC 4.
    pub const FOOT: CCNumber = CCNumber(4);
    /// CC 5.
    pub const PORTAMENTO_TIME: CCNumber = CCNumber(5);
    /// CC 6 — the MSB of an (N)RPN value.
    pub const DATA_ENTRY: CCNumber = CCNumber(6);
    /// CC 7.
    pub const VOLUME: CCNumber = CCNumber(7);
    /// CC 8.
    pub const BALANCE: CCNumber = CCNumber(8);
    /// CC 10.
    pub const PAN: CCNumber = CCNumber(10);
    /// CC 11.
    pub const EXPRESSION: CCNumber = CCNumber(11);

    /// CC 32 — Bank Select LSB, the low 7 bits of a bank
    /// (MSB is [`BANK_SELECT`](Self::BANK_SELECT)).
    pub const BANK_SELECT_LSB: CCNumber = CCNumber(32);

    /// CC 38 — Data Entry LSB, the low 7 bits of an (N)RPN value
    /// (MSB is [`DATA_ENTRY`](Self::DATA_ENTRY)).
    pub const DATA_ENTRY_LSB: CCNumber = CCNumber(38);

    // Sound controllers.
    /// CC 71 — Sound Controller 2 (filter resonance).
    pub const RESONANCE: CCNumber = CCNumber(71);
    /// CC 72 — Sound Controller 3 (release time).
    pub const RELEASE_TIME: CCNumber = CCNumber(72);
    /// CC 73 — Sound Controller 4 (attack time).
    pub const ATTACK_TIME: CCNumber = CCNumber(73);
    /// CC 74 — Sound Controller 5 (brightness). Under MPE this is the *slide*
    /// dimension, which is why it is routed per-note rather than per-channel.
    pub const BRIGHTNESS: CCNumber = CCNumber(74);

    // Switches.
    /// CC 64 — sustain (damper) pedal.
    pub const SUSTAIN: CCNumber = CCNumber(64);
    /// CC 65.
    pub const PORTAMENTO_SWITCH: CCNumber = CCNumber(65);
    /// CC 66.
    pub const SOSTENUTO: CCNumber = CCNumber(66);
    /// CC 67.
    pub const SOFT_PEDAL: CCNumber = CCNumber(67);
    /// CC 68 — Legato Footswitch.
    pub const LEGATO: CCNumber = CCNumber(68);

    /// CC 84 — Portamento Control: the source key of the next note's glide.
    pub const PORTAMENTO_CONTROL: CCNumber = CCNumber(84);

    /// CC 88 — High Resolution Velocity Prefix (CA-031): the low 7 bits of the
    /// next Note On or Off's 14-bit velocity.
    pub const VELOCITY_PREFIX: CCNumber = CCNumber(88);

    // Channel mode.
    /// CC 120.
    pub const ALL_SOUND_OFF: CCNumber = CCNumber(120);
    /// CC 121 — Reset All Controllers.
    pub const RESET_ALL: CCNumber = CCNumber(121);
    /// CC 122 — Local Control.
    pub const LOCAL_CONTROL: CCNumber = CCNumber(122);
    /// CC 123.
    pub const ALL_NOTES_OFF: CCNumber = CCNumber(123);
    /// CC 124 — Omni Mode Off.
    pub const OMNI_MODE_OFF: CCNumber = CCNumber(124);
    /// CC 125 — Omni Mode On.
    pub const OMNI_MODE_ON: CCNumber = CCNumber(125);
    /// CC 126 — Mono Mode On (Omni-Off/Mono): its value is a
    /// [`MonoChannels`](crate::MonoChannels) count.
    pub const MONO_MODE_ON: CCNumber = CCNumber(126);
    /// CC 127 — Poly Mode On.
    pub const POLY_MODE_ON: CCNumber = CCNumber(127);
    /// CC 96 — Data Increment (an event, not a value).
    pub const DATA_INCREMENT: CCNumber = CCNumber(96);
    /// CC 97 — Data Decrement (an event, not a value).
    pub const DATA_DECREMENT: CCNumber = CCNumber(97);

    // Effects depths.
    /// CC 91 — Effects 1 Depth (reverb send level).
    pub const REVERB_SEND: CCNumber = CCNumber(91);
    /// CC 93 — Effects 3 Depth (chorus send level).
    pub const CHORUS_SEND: CCNumber = CCNumber(93);

    // RPN / NRPN parameter select.
    /// CC 98.
    pub const NRPN_LSB: CCNumber = CCNumber(98);
    /// CC 99.
    pub const NRPN_MSB: CCNumber = CCNumber(99);
    /// CC 100.
    pub const RPN_LSB: CCNumber = CCNumber(100);
    /// CC 101.
    pub const RPN_MSB: CCNumber = CCNumber(101);

    /// The controller that carries the low 7 bits of this one's 14-bit value:
    /// controllers 0 to 31 are the MSBs and 32 to 63 their LSBs, 32 apart (MIDI
    /// 1.0 Control Change table). `None` for any other controller, an LSB
    /// included.
    ///
    /// ```
    /// use tutti_ump::CCNumber;
    ///
    /// assert_eq!(CCNumber::DATA_ENTRY.lsb(), Some(CCNumber::DATA_ENTRY_LSB));
    /// assert_eq!(CCNumber::DATA_ENTRY_LSB.lsb(), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn lsb(self) -> Option<CCNumber> {
        if self.0 < 32 {
            Some(CCNumber(self.0 + 32))
        } else {
            None
        }
    }

    /// Whether a MIDI 2.0 Protocol receiver ignores this controller as a
    /// Control Change (M2-104 §7.4.6): Bank Select (0 and 32), Data Entry (6
    /// and 38) and the RPN and NRPN selects (98 to 101), which the Program
    /// Change, Registered Controller and Assignable Controller messages
    /// carry instead.
    ///
    /// ```
    /// use tutti_ump::CCNumber;
    ///
    /// assert!(CCNumber::BANK_SELECT.is_ignored_in_midi2());
    /// assert!(CCNumber::RPN_MSB.is_ignored_in_midi2());
    /// assert!(!CCNumber::VOLUME.is_ignored_in_midi2());
    /// ```
    #[inline]
    #[must_use]
    pub const fn is_ignored_in_midi2(self) -> bool {
        matches!(self.0, 0 | 6 | 32 | 38 | 98..=101)
    }
}

// Deliberately no infallible `From<u8> for CCNumber`, for the same reason `MidiChannel`
// refuses one: the conversion would mask or fail, and a lossy conversion inside
// a trait that promises not to lose data is exactly what `ChannelLayout`
// refused a reverse `From` for.

#[cfg(test)]
mod tests {
    use super::*;

    /// The named roster against the MIDI 1.0 Control Change table. A wrong
    /// number here is silent at runtime — the synth just responds to the wrong
    /// knob — so the table is asserted rather than trusted.
    #[test]
    fn the_named_controllers_have_their_spec_numbers() {
        assert_eq!(CCNumber::BANK_SELECT.get(), 0);
        assert_eq!(CCNumber::MOD_WHEEL.get(), 1);
        assert_eq!(CCNumber::BREATH.get(), 2);
        assert_eq!(CCNumber::FOOT.get(), 4);
        assert_eq!(CCNumber::PORTAMENTO_TIME.get(), 5);
        assert_eq!(CCNumber::DATA_ENTRY.get(), 6);
        assert_eq!(CCNumber::VOLUME.get(), 7);
        assert_eq!(CCNumber::BALANCE.get(), 8);
        assert_eq!(CCNumber::PAN.get(), 10);
        assert_eq!(CCNumber::EXPRESSION.get(), 11);
        assert_eq!(CCNumber::BANK_SELECT_LSB.get(), 32);
        assert_eq!(CCNumber::DATA_ENTRY_LSB.get(), 38);
        assert_eq!(CCNumber::REVERB_SEND.get(), 91);
        assert_eq!(CCNumber::CHORUS_SEND.get(), 93);

        assert_eq!(CCNumber::SUSTAIN.get(), 64);
        assert_eq!(CCNumber::PORTAMENTO_SWITCH.get(), 65);
        assert_eq!(CCNumber::SOSTENUTO.get(), 66);
        assert_eq!(CCNumber::SOFT_PEDAL.get(), 67);
        assert_eq!(CCNumber::LEGATO.get(), 68);
        assert_eq!(CCNumber::PORTAMENTO_CONTROL.get(), 84);
        assert_eq!(CCNumber::VELOCITY_PREFIX.get(), 88);

        assert_eq!(CCNumber::RESONANCE.get(), 71);
        assert_eq!(CCNumber::RELEASE_TIME.get(), 72);
        assert_eq!(CCNumber::ATTACK_TIME.get(), 73);
        assert_eq!(CCNumber::BRIGHTNESS.get(), 74);

        assert_eq!(CCNumber::NRPN_LSB.get(), 98);
        assert_eq!(CCNumber::NRPN_MSB.get(), 99);
        assert_eq!(CCNumber::RPN_LSB.get(), 100);
        assert_eq!(CCNumber::RPN_MSB.get(), 101);

        assert_eq!(CCNumber::ALL_SOUND_OFF.get(), 120);
        assert_eq!(CCNumber::RESET_ALL.get(), 121);
        assert_eq!(CCNumber::ALL_NOTES_OFF.get(), 123);
    }

    /// The MSB/LSB pairs are adjacent in the way the (N)RPN decoder assumes:
    /// LSB is the lower number of each pair. Getting this backwards silently
    /// swaps bank and index in every RPN run.
    #[test]
    fn the_rpn_select_pairs_are_lsb_then_msb() {
        assert_eq!(CCNumber::NRPN_LSB.get() + 1, CCNumber::NRPN_MSB.get());
        assert_eq!(CCNumber::RPN_LSB.get() + 1, CCNumber::RPN_MSB.get());
        // Data Entry MSB (6) and its LSB (38) are 32 apart, the standard
        // MSB→LSB offset for the CC 0-31 continuous-controller block.
        assert_eq!(
            CCNumber::DATA_ENTRY.get() + 32,
            CCNumber::DATA_ENTRY_LSB.get()
        );
    }

    /// Only the continuous controllers 0 to 31 have an LSB, and it is 32
    /// above.
    #[test]
    fn the_lsb_of_a_controller_is_32_above_for_0_to_31_only() {
        for raw in 0..=127u8 {
            let cc = CCNumber::of(raw);
            let want = (raw < 32).then(|| CCNumber::of(raw + 32));
            assert_eq!(cc.lsb(), want, "CC {raw}");
        }
        assert_eq!(CCNumber::BANK_SELECT.lsb(), Some(CCNumber::BANK_SELECT_LSB));
        assert_eq!(CCNumber::DATA_ENTRY.lsb(), Some(CCNumber::DATA_ENTRY_LSB));
    }
}
