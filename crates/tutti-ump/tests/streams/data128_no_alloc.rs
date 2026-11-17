//! `Sysex8Assembler`, `MixedDataAssembler` and their packet writers allocate
//! nothing once built.

use assert_no_alloc::assert_no_alloc;
use tutti_ump::{
    ManufacturerId, MdsChunkUmps, MdsIdentity, MidiGroup, MixedDataAssembler, MixedDataSet,
    MixedDataStep, Sysex8Assembler, Sysex8Step, Sysex8StreamIds, U4,
};

#[test]
fn sysex8_split_and_assemble_allocate_nothing() {
    let data = [0xAB; 100];
    let mut assembler = Sysex8Assembler::new(vec![0; 128], 128, 1);
    let mut done = 0;
    let ids = Sysex8StreamIds::undiscovered();
    assert_no_alloc(|| {
        for ump in ids.open().unwrap().packets(MidiGroup::of(0), &data) {
            if let Sysex8Step::Complete(m) = assembler.push(ump) {
                done += m.payload.len();
            }
        }
    });
    assert_eq!(done, 100);
}

#[test]
fn mixed_data_split_and_assemble_allocate_nothing() {
    let data = [0xAB; 100];
    let mut assembler = MixedDataAssembler::new(vec![0; 128], 128, 1);
    let mut done = 0;
    assert_no_alloc(|| {
        let set = MixedDataSet {
            group: MidiGroup::of(0),
            mds_id: U4::of(1),
            identity: MdsIdentity {
                manufacturer: ManufacturerId::NON_COMMERCIAL,
                device: 0,
                sub_id_1: 0,
                sub_id_2: 0,
            },
            data: &data,
        };
        for ump in set.packets(MdsChunkUmps::of(2)).unwrap() {
            if let MixedDataStep::Complete(s) = assembler.push(ump) {
                done += s.data.len();
            }
        }
    });
    assert_eq!(done, 100);
}
