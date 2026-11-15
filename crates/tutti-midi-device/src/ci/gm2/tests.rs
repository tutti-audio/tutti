use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_ump::MidiChannel;

use super::super::testing::{channel, profile_table};
use super::super::*;

#[test]
fn the_function_block_profile_id_is_7e_00_00_01_01() {
    // M2-118 Table 4.
    assert_eq!(
        ProfileId::GM2_FUNCTION_BLOCK.to_bytes(),
        [0x7E, 0x00, 0x00, 0x01, 0x01]
    );
}

#[test]
fn the_single_channel_profile_id_is_7e_20_02_01_01() {
    // M2-119 Table 4.
    assert_eq!(
        ProfileId::GM2_SINGLE_CHANNEL.to_bytes(),
        [0x7E, 0x20, 0x02, 0x01, 0x01]
    );
}

#[test]
fn the_function_block_profile_is_addressed_to_7f() {
    // M2-118 §3.3: Device ID 0x7F in every Profile Configuration message.
    assert_eq!(Gm2FunctionBlock::ADDRESS.get(), 0x7F);
}

#[test]
fn channel_10_of_each_group_starts_as_rhythm_and_the_rest_as_melody() {
    // M2-118 §3.3: Channel 10 of every Group is a Rhythm Channel.
    for n in 0..16 {
        let role = Gm2FunctionBlock::default_role(MidiChannel::of(n));
        assert_eq!(
            role,
            if n == 9 {
                Gm2Role::Rhythm
            } else {
                Gm2Role::Melody
            },
            "channel {}",
            n + 1
        );
    }
    assert_eq!(Gm2SingleChannel::ROLE, Gm2Role::Melody);
}

#[test]
fn a_channel_count_is_a_multiple_of_16_up_to_256() {
    // M2-118 §3.3.
    check_range_contract(&RangeContract {
        valid: &[16, 32, 112, 256],
        invalid: &[0, 1, 15, 17, 24, 255, 257, 272],
        new: Gm2ChannelCount::new,
        get: Gm2ChannelCount::get,
        of: Some(Gm2ChannelCount::of),
    });
    assert_eq!(Gm2ChannelCount::of(48).groups(), 3);
    assert_eq!(Gm2ChannelCount::of(256).groups(), 16);
}

#[test]
fn the_function_block_profile_counts_only_when_enabled_on_the_block() {
    let on = profile_table(
        CiDeviceId::FunctionBlock,
        ProfileId::GM2_FUNCTION_BLOCK,
        true,
    );
    assert!(Gm2FunctionBlock::enabled_on(&on));
    let off = profile_table(
        CiDeviceId::FunctionBlock,
        ProfileId::GM2_FUNCTION_BLOCK,
        false,
    );
    assert!(!Gm2FunctionBlock::enabled_on(&off));
    let on_channel = profile_table(channel(0), ProfileId::GM2_FUNCTION_BLOCK, true);
    assert!(!Gm2FunctionBlock::enabled_on(&on_channel));
    let single = profile_table(
        CiDeviceId::FunctionBlock,
        ProfileId::GM2_SINGLE_CHANNEL,
        true,
    );
    assert!(!Gm2FunctionBlock::enabled_on(&single));
}

#[test]
fn the_single_channel_profile_counts_only_when_enabled_on_that_channel() {
    let on = profile_table(channel(4), ProfileId::GM2_SINGLE_CHANNEL, true);
    assert!(Gm2SingleChannel::enabled_on(&on, MidiChannel::of(4)));
    assert!(!Gm2SingleChannel::enabled_on(&on, MidiChannel::of(5)));
    let off = profile_table(channel(4), ProfileId::GM2_SINGLE_CHANNEL, false);
    assert!(!Gm2SingleChannel::enabled_on(&off, MidiChannel::of(4)));
    let block = profile_table(
        CiDeviceId::FunctionBlock,
        ProfileId::GM2_SINGLE_CHANNEL,
        true,
    );
    assert!(!Gm2SingleChannel::enabled_on(&block, MidiChannel::of(4)));
}
