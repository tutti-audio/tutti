use super::*;

fn drained(reader: &mut SeqSlotsReader<f64>) -> Vec<(usize, f64)> {
    let mut out = Vec::new();
    reader.drain_in_epoch(|slot, value, _| out.push((slot, value)));
    out
}

/// **Many writes to one slot before a drain are one value, the last**, given up
/// once.
#[test]
fn writes_before_a_drain_coalesce_to_the_last() {
    let (mut writer, mut reader) = seq_slots::<f64>(2);
    for step in 0..100 {
        writer.write(1, f64::from(step));
    }
    assert_eq!(drained(&mut reader), [(1, 99.0)]);
    assert!(drained(&mut reader).is_empty());
}

/// **A new epoch makes the writes before it stale; a write after it is not.**
#[test]
fn a_new_epoch_voids_the_writes_before_it() {
    let (mut writer, mut reader) = seq_slots::<f64>(2);
    writer.write(0, 1.0);
    writer.new_epoch();
    assert!(drained(&mut reader).is_empty());
    writer.write(1, 2.0);
    assert_eq!(drained(&mut reader), [(1, 2.0)]);
    assert!(
        drained(&mut reader).is_empty(),
        "the stale cell is not given up later"
    );
    writer.write(0, 3.0);
    assert_eq!(drained(&mut reader), [(0, 3.0)]);
}

/// **A new epoch between the reads and the epoch load drops what was read.**
#[test]
fn a_new_epoch_inside_a_drain_voids_what_it_had_read() {
    let (mut writer, mut reader) = seq_slots::<f64>(2);
    writer.write(0, 1.0);
    writer.write(1, 2.0);
    reader.collect();
    writer.new_epoch();
    let mut applied = Vec::new();
    reader.finish(|slot, value, _| applied.push((slot, value)));
    assert!(applied.is_empty(), "{applied:?}");
}

/// **Epochs compare as serial numbers across the 32-bit wrap.**
#[test]
fn epochs_compare_across_the_wrap() {
    assert!(before(u32::MAX, 0));
    assert!(before(u32::MAX - 5, 3));
    assert!(!before(0, u32::MAX));
    assert!(!before(3, u32::MAX - 5));
    assert!(!before(7, 7));
    assert!(before(6, 7));
    assert!(!before(7, 6));
}

/// **A drain over slots no write has touched skips the sweep.** Only the
/// generation keeps it off once the slot's own record is forgotten.
#[test]
fn an_unchanged_generation_skips_the_sweep() {
    let (mut writer, mut reader) = seq_slots::<f64>(1);
    assert!(drained(&mut reader).is_empty());
    writer.write(0, 1.0);
    assert_eq!(drained(&mut reader), [(0, 1.0)]);
    reader.seen[0] = 0;
    assert!(drained(&mut reader).is_empty(), "the sweep was skipped");
    writer.write(0, 2.0);
    assert_eq!(drained(&mut reader), [(0, 2.0)]);
}

/// **A value of any size round-trips whole**, with its epoch beside it.
#[test]
fn a_wide_value_round_trips() {
    let (mut writer, mut reader) = seq_slots::<[u32; 3]>(1);
    writer.write(0, [1, 2, 3]);
    let mut got = Vec::new();
    reader.drain_in_epoch(|slot, value, _| got.push((slot, value)));
    assert_eq!(got, [(0, [1, 2, 3])]);
}

/// **The epoch watch sees a new epoch without the reader**: a thread that does
/// not hold the reader learns of a reset from the word the writer bumps.
#[test]
fn an_epoch_watch_sees_a_new_epoch_without_the_reader() {
    let (mut writer, reader) = seq_slots::<f64>(1);
    let watch = reader.epoch_watch();
    assert_eq!(watch.get(), reader.epoch());
    writer.new_epoch();
    assert_eq!(watch.get(), 1);
    assert_eq!(reader.epoch(), 1);
}

/// **A drain says which epoch it checked its values against**, so a value kept
/// past it is known stale once the epoch moves on.
#[test]
fn a_drain_in_epoch_yields_the_epoch_it_checked() {
    let (mut writer, mut reader) = seq_slots::<f64>(1);
    writer.write(0, 1.0);
    let mut seen = Vec::new();
    reader.drain_in_epoch(|slot, value, epoch| seen.push((slot, value, epoch)));
    assert_eq!(seen, [(0, 1.0, 0)]);
    writer.new_epoch();
    writer.write(0, 2.0);
    seen.clear();
    reader.drain_in_epoch(|slot, value, epoch| seen.push((slot, value, epoch)));
    assert_eq!(seen, [(0, 2.0, 1)]);
}

/// **A drain no write overlaps brings `swept` up to `written`**, and a drain
/// with nothing written leaves it there; a new epoch voids the write and still
/// counts it as swept.
#[test]
fn a_quiet_drain_sweeps_up_to_what_was_written() {
    let (mut writer, mut reader) = seq_slots::<f64>(2);
    assert_eq!(writer.written(), reader.swept(), "nothing written");
    writer.write(0, 1.0);
    writer.write(1, 2.0);
    assert_eq!(writer.written(), Generation::new(2));
    assert!(!reader.swept().has_reached(writer.written()));
    assert_eq!(drained(&mut reader).len(), 2);
    assert_eq!(reader.swept(), writer.written());
    assert!(drained(&mut reader).is_empty());
    assert_eq!(reader.swept(), writer.written());

    writer.write(0, 3.0);
    writer.new_epoch();
    assert!(drained(&mut reader).is_empty(), "stale by the epoch");
    assert!(reader.swept().has_reached(writer.written()));
}

/// **Generations compare as serial numbers across the wrap.**
#[test]
fn a_generation_has_reached_an_earlier_one_across_the_wrap() {
    let (late, early) = (Generation::new(3), Generation::new(u32::MAX - 2));
    assert!(late.has_reached(early) && !early.has_reached(late));
    assert!(late.has_reached(late));
}
