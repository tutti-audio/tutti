use super::*;
use crate::MtcRate;
use tutti_units::Seconds;

/// **The transport commands are their RP-013 codes**: STOP `01`, PLAY `02`.
#[test]
fn stop_and_play_are_their_command_codes() {
    assert_eq!(MmcCommands::stop().as_bytes(), [0x01]);
    assert_eq!(MmcCommands::play().as_bytes(), [0x02]);
}

/// **LOCATE (target) is `44 06 01` and a time code** (RP-013 p.28): 1:01:01
/// and 15 frames at 30 fps is `61 01 01 0F 00` (the rate code in the hours
/// byte, no subframes), and DEFERRED PLAY `03` follows when the device is to
/// play once it has got there.
#[test]
fn locate_is_the_target_time_code_and_then_deferred_play() {
    let target =
        MmcPosition::from_seconds(Seconds::new(3661.5), MtcRate::Fps30).expect("a time of day");
    let locate = [0x44, 0x06, 0x01, 0x61, 0x01, 0x01, 0x0F, 0x00];
    assert_eq!(MmcCommands::locate(target, false).as_bytes(), locate);
    let mut played = locate.to_vec();
    played.push(0x03);
    assert_eq!(MmcCommands::locate(target, true).as_bytes(), played);
}
