use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_tempo::TempoMap;
use tutti_transport::TimeSignatureSlot;
use tutti_units::{Beat, SampleRate};

use crate::Session;

/// **The update's `Reclaim` phase frees what a block held across a
/// replacement**, in a cell nothing publishes to again: the meter a
/// following plugin slot (a `TimeSignatureSlot`, as a plugin node holds) read when
/// the project meter changed. A held `Loan` stands in for a block in
/// progress across the publish.
///
/// The nodes' cells are the graph's table, pinned by
/// `the_update_sweeps_each_nodes_reclaimer_while_it_is_in_the_graph`; the
/// modulation lanes by `tests/modulation/modulation.rs`, the hardware ports by
/// `tests/midi/midi.rs` (`midi-hardware`), a plugin's meter by `plugin.rs`.
#[test]
fn an_update_frees_what_a_block_held_across_a_replacement() {
    let (mut s, _bg) = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let _ = s.update();

    let slot =
        TimeSignatureSlot::following_meter(Some(s.transport.settings().project_meter().clone()));
    let meter = s.transport_handle().meter();
    let old = Arc::new(MeterMap::new([MeterChange::new(
        Beat::new(0.0),
        TimeSignature::of(3, 4),
    )]));
    let gone = Arc::downgrade(&old);
    s.set(meter.to(old)).unwrap();
    let _ = s.update();

    let mut node = slot.reader();
    {
        // A block in progress across the publish.
        let _held = node.lend();
        s.set(meter.to(MeterMap::default())).unwrap();
        // Still held: this update's own reclaim leaves it.
        let _ = s.update();
    }
    assert!(gone.upgrade().is_some(), "the meter is retired");

    let _ = s.update();
    assert!(gone.upgrade().is_none(), "the meter is freed");
}

fn detached() -> (Session<&'static str, crate::Detached>, crate::Background) {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

fn meter_of(numerator: u32) -> Arc<MeterMap> {
    Arc::new(MeterMap::new([MeterChange::new(
        Beat::new(0.0),
        TimeSignature::of(numerator, 4),
    )]))
}

/// **A published slot written twice before an update publishes once, with
/// the second value**: the meter cell's sequence moves by one.
#[test]
fn a_slot_written_twice_before_an_update_publishes_once_with_the_second_value() {
    let (mut s, _bg) = detached();
    let _ = s.update();
    let mut reader = s.transport.settings().project_meter().reader();
    let seq = reader.lend().seq();
    let meter = s.transport_handle().meter();

    s.set(meter.to(meter_of(3))).unwrap();
    s.set(meter.to(meter_of(5))).unwrap();
    assert_eq!(reader.lend().seq(), seq, "nothing publishes at the write");
    let _ = s.update();
    let loan = reader.lend();
    assert_eq!(loan.seq(), seq + 1, "one publish for two writes");
    assert_eq!(*loan, *meter_of(5), "the second value");
}

/// **A read after a write and before the update answers the pending value,
/// and the audio side still reads the one in force**: a reader's lend shows
/// the old meter and tempo until the update publishes.
#[test]
fn a_read_before_the_update_is_the_pending_value_and_the_audio_side_reads_the_old_one() {
    let (mut s, _bg) = detached();
    let _ = s.update();
    let mut meters = s.transport.settings().project_meter().reader();
    let mut tempos = s.transport.settings().tempo_reader();
    let old_meter = (*meters.lend()).clone();
    let old_tempo = (*tempos.lend()).clone();
    let (meter, tempo) = (s.transport_handle().meter(), s.transport_handle().tempo());
    let new_tempo = Arc::new(TempoMap::constant(tutti_units::Bpm::of(77.0)).unwrap());

    s.set((meter.to(meter_of(7)), tempo.to(Arc::clone(&new_tempo))))
        .unwrap();
    assert_eq!(
        *s.get(meter).unwrap(),
        *meter_of(7),
        "the write is read back"
    );
    assert_eq!(s.get(tempo).unwrap(), new_tempo);
    assert_eq!(*meters.lend(), old_meter, "the block reads the old meter");
    assert_eq!(*tempos.lend(), old_tempo, "and the old tempo");
    assert_eq!(
        s.tempo_map(),
        Arc::new(old_tempo.clone()),
        "published is unchanged"
    );

    let _ = s.update();
    assert_eq!(*meters.lend(), *meter_of(7));
    assert_eq!(*tempos.lend(), *new_tempo);
}

/// **A batch of a tempo and a meter lands in one update**: a reader that
/// lends both after the update sees both new, and before it both old.
#[test]
fn a_batch_of_tempo_and_meter_lands_in_one_update() {
    let (mut s, _bg) = detached();
    let _ = s.update();
    let mut meters = s.transport.settings().project_meter().reader();
    let mut tempos = s.transport.settings().tempo_reader();
    let (meter_seq, tempo_seq) = (meters.lend().seq(), tempos.lend().seq());
    let new_tempo = TempoMap::constant(tutti_units::Bpm::of(77.0)).unwrap();

    s.set((
        s.transport_handle().tempo().to(new_tempo),
        s.transport_handle().meter().to(meter_of(7)),
    ))
    .unwrap();
    assert_eq!(
        (meters.lend().seq(), tempos.lend().seq()),
        (meter_seq, tempo_seq),
        "both old before the update"
    );
    let _ = s.update();
    assert_eq!(
        (meters.lend().seq(), tempos.lend().seq()),
        (meter_seq + 1, tempo_seq + 1),
        "both new after it"
    );
}

/// **A write the check refuses stages nothing**: a batch whose second item
/// is refused leaves the first unwritten too, and the update publishes
/// neither.
#[cfg(feature = "midi")]
#[test]
fn a_refused_write_stages_nothing() {
    use crate::{SetRefusal, ValueErrorKind};
    let (mut s, _bg) = detached();
    let _ = s.update();
    // A live tempo owns the map: `check` refuses a tempo write.
    s.midi
        .following()
        .store(true, std::sync::atomic::Ordering::Release);
    let mut meters = s.transport.settings().project_meter().reader();
    let seq = meters.lend().seq();
    let tempo_before = s.tempo_map();
    let (meter, tempo) = (s.transport_handle().meter(), s.transport_handle().tempo());

    let refused = s
        .set((
            meter.to(meter_of(7)),
            tempo.to(TempoMap::constant(tutti_units::Bpm::of(77.0)).unwrap()),
        ))
        .unwrap_err();
    assert_eq!(refused.kind, ValueErrorKind::Refused(SetRefusal::Following));
    assert_eq!(refused.at, 1);
    s.midi
        .following()
        .store(false, std::sync::atomic::Ordering::Release);
    let _ = s.update();
    assert_eq!(meters.lend().seq(), seq, "the meter was not staged");
    assert!(Arc::ptr_eq(&s.tempo_map(), &tempo_before), "nor the tempo");
    assert_eq!(
        *s.get(meter).unwrap(),
        *meters.lend(),
        "a read is the map in force"
    );
}

/// **The update's `Reclaim` phase frees the MPE mode a block held across a
/// replacement**, in the session's own cell: a reader the mode's cell
/// lends from stands in for a block in progress across the publish.
#[cfg(feature = "midi")]
#[test]
fn an_update_frees_the_mpe_mode_a_block_held_across_a_replacement() {
    let (mut s, _bg) = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let _ = s.update();
    let cell = s.midi.state.mpe().clone();
    let mut node = cell.reader();
    {
        let _held = node.lend();
        s.set(s.mpe().to(tutti_fixtures::midi::lower_zone(3)))
            .unwrap();
        let _ = s.update();
    }
    assert_eq!(cell.retired_len(), 1, "the first mode is retired");
    let _ = s.update();
    assert_eq!(cell.retired_len(), 0, "and freed by the reclaim");
}
