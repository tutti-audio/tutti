use tutti_ump::{CCNumber, MidiChannel, U7};

use super::super::testing::{OURS, PEER, channel, header_from};
use super::super::*;
use CcFunction as F;

fn cc(n: u8) -> SuggestedCc {
    SuggestedCc::of(CCNumber::of(n))
}

fn reset(n: u8) -> CcReset {
    CcReset::To(U7::of(n))
}

#[test]
fn the_profile_id_is_7e_21_00_01_and_the_level() {
    // M2-113 §2.2.
    assert_eq!(
        DefaultCcLevel::MeetsMinimum.profile().to_bytes(),
        [0x7E, 0x21, 0x00, 0x01, 0x01]
    );
    assert_eq!(
        DefaultCcLevel::Partial.profile().to_bytes(),
        [0x7E, 0x21, 0x00, 0x01, 0x00]
    );
}

fn table_with(address: CiDeviceId, level: DefaultCcLevel, enabled: bool) -> ProfileTable {
    let (mut peer, _, _) = {
        let now = std::time::Instant::now();
        let (p, a) = ProfileInitiator::new(Muid::of(OURS), Muid::of(PEER), now);
        (p, a, now)
    };
    let header = header_from(PEER, address);
    let profile = level.profile();
    let channels = ProfileChannels::of(1);
    peer.on_message(&Decoded::Message(if enabled {
        CiMessage::ProfileEnabled {
            header,
            profile,
            channels,
        }
    } else {
        CiMessage::ProfileDisabled {
            header,
            profile,
            channels,
        }
    }));
    peer.table().clone()
}

#[test]
fn the_profile_is_per_channel_and_only_an_enabled_one_counts() {
    // M2-113 §2.2.1: the Destination is a channel; §2.3: enabled by Set Profile On.
    let ch = MidiChannel::of(3);
    for level in [DefaultCcLevel::Partial, DefaultCcLevel::MeetsMinimum] {
        assert_eq!(
            DefaultCcLevel::enabled_on(&table_with(channel(3), level, true), ch),
            Some(level)
        );
        assert_eq!(
            DefaultCcLevel::enabled_on(&table_with(channel(3), level, true), MidiChannel::of(4)),
            None
        );
        assert_eq!(
            DefaultCcLevel::enabled_on(&table_with(channel(3), level, false), ch),
            None
        );
    }
}

#[test]
fn a_group_wide_enable_is_not_a_channel_enable() {
    let table = table_with(CiDeviceId::Group, DefaultCcLevel::MeetsMinimum, true);
    assert_eq!(DefaultCcLevel::enabled_on(&table, MidiChannel::of(0)), None);
}

#[test]
fn the_named_functions_are_where_appendix_a_puts_them() {
    for (n, function) in [
        (0, F::BankSelect),
        (1, F::ModulationWheel),
        (2, F::BreathController),
        (4, F::FootController),
        (5, F::PortamentoTime),
        (6, F::DataEntry),
        (7, F::ChannelVolume),
        (8, F::Balance),
        (10, F::Pan),
        (11, F::Expression),
        (64, F::DamperPedal),
        (65, F::PortamentoSwitch),
        (66, F::Sostenuto),
        (67, F::SoftPedal),
        (68, F::LegatoFootswitch),
        (69, F::Hold2),
        (74, F::Brightness),
        (91, F::Effects1Depth),
        (93, F::Effects3Depth),
        (98, F::NrpnSelect),
        (99, F::NrpnSelect),
        (100, F::RpnSelect),
        (101, F::RpnSelect),
        (121, F::ResetAllControllers),
        (123, F::AllNotesOff),
        (127, F::PolyModeOn),
    ] {
        assert_eq!(cc(n).function(), function, "CC {n}");
    }
}

#[test]
fn a_number_32_above_a_fine_control_is_its_lsb_with_the_same_function_and_reset() {
    // Appendix A rows 32 to 63: the LSB for Control 0 to 31.
    for n in 0..32 {
        let (msb, lsb) = (cc(n), cc(n + 32));
        assert_eq!(msb.part(), CcPart::Msb, "CC {n}");
        assert_eq!(lsb.part(), CcPart::Lsb, "CC {}", n + 32);
        assert_eq!(
            (msb.function(), msb.reset(), msb.value()),
            (lsb.function(), lsb.reset(), lsb.value()),
            "CC {n} and CC {}",
            n + 32
        );
    }
}

#[test]
fn reset_all_controllers_values_are_those_of_appendix_a() {
    for (n, expected) in [
        (0, CcReset::Keep),
        (1, reset(0)),
        (3, CcReset::DeviceSpecific),
        (7, CcReset::Keep),
        (8, reset(64)),
        (40, reset(64)),
        (10, CcReset::Keep),
        (11, reset(127)),
        (43, reset(127)),
        (64, reset(0)),
        (69, reset(0)),
        (70, CcReset::Keep),
        (80, CcReset::DeviceSpecific),
        (91, CcReset::Keep),
        (98, reset(127)),
        (101, reset(127)),
        (102, CcReset::DeviceSpecific),
        (120, CcReset::Keep),
    ] {
        assert_eq!(cc(n).reset(), expected, "CC {n}");
    }
}

#[test]
fn values_are_read_as_appendix_a_says() {
    for (n, value) in [
        (1, CcValue::Range),
        (64, CcValue::Switch),
        (67, CcValue::Switch),
        (68, CcValue::LegatoSwitch),
        (69, CcValue::Switch),
        (96, CcValue::Unused),
        (97, CcValue::Unused),
        (102, CcValue::Undefined),
        (120, CcValue::Zero),
        (122, CcValue::OnOff127),
        (126, CcValue::ChannelCount),
        (127, CcValue::Zero),
    ] {
        assert_eq!(cc(n).value(), value, "CC {n}");
    }
}

#[test]
fn the_table_has_one_row_per_number_in_order() {
    let rows: Vec<u8> = SuggestedCc::all().map(|r| r.controller().get()).collect();
    assert_eq!(rows, (0..=127).collect::<Vec<u8>>());
}

#[test]
fn numbers_the_profile_leaves_undefined_have_no_function() {
    for n in [3, 9, 14, 15, 20, 31, 35, 85, 87, 89, 90, 102, 119] {
        assert_eq!(cc(n).function(), F::Undefined, "CC {n}");
        assert_eq!(cc(n).reset(), CcReset::DeviceSpecific, "CC {n}");
    }
}
