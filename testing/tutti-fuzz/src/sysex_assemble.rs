use tutti_ump::{
    MixedDataAssembler, MixedDataStep, Sysex8Assembler, Sysex8Step, SysexAssembler, SysexStep, Ump,
    split_stream,
};

/// The packets between two `begin_block` calls of the SysEx7 assembler.
const BLOCK: usize = 8;

/// Feeds the packets of `data` to a SysEx7, a SysEx8 and a Mixed Data Set
/// assembler. The first two bytes pick the limits and the lane count; the
/// rest is little-endian words.
///
/// A completed message is never longer than its assembler's limit, and a SysEx7
/// one reads back as a framed message from the assembler's buffer.
pub(crate) fn sysex_assemble(data: &[u8]) {
    let [limit, lanes, rest @ ..] = data else {
        return;
    };
    let max = 8 + usize::from(*limit % 64);
    let lanes = usize::from(*lanes % 17);
    let words = crate::words(rest);

    let mut sysex7 = SysexAssembler::new(
        vec![0u8; SysexAssembler::room_for(max, BLOCK, lanes)],
        max,
        lanes,
    );
    let mut sysex8 = Sysex8Assembler::new(vec![0u8; max * lanes], max, lanes);
    let mut mixed = MixedDataAssembler::new(vec![0u8; max * lanes], max, lanes);

    for (n, ump) in split_stream(&words).enumerate() {
        if n % BLOCK == 0 {
            sysex7.begin_block();
        }
        feed(&mut sysex7, &mut sysex8, &mut mixed, ump, max);
    }
    sysex7.lost_tail();
    sysex7.reset();
    sysex8.lost_tail();
    sysex8.reset();
    mixed.lost_tail();
    mixed.reset();
}

fn feed(
    sysex7: &mut SysexAssembler<Vec<u8>>,
    sysex8: &mut Sysex8Assembler<Vec<u8>>,
    mixed: &mut MixedDataAssembler<Vec<u8>>,
    ump: Ump,
    max: usize,
) {
    if let SysexStep::Complete(at) = sysex7.push(ump) {
        assert!(at.len() <= sysex7.max_message());
        assert_eq!(sysex7.message(at).payload().len() + 2, at.len());
    }
    if let Sysex8Step::Complete(whole) = sysex8.push(ump) {
        assert!(whole.payload.len() <= max);
    }
    if let MixedDataStep::Complete(set) = mixed.push(ump) {
        assert!(set.data.len() <= max);
    }
}
