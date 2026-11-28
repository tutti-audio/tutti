use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_tempo::TempoMap;
use tutti_timeline::TimeSignatureSlot;
use tutti_units::{Beat, SampleRate, Samples};

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
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .outputs(ChannelLayout::MONO)
        .sample_rate(SampleRate::SR_48K)
        .block_dates(crate::BlockDates::Frames)
        .build()
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

/// **An update with no edit publishes no compensation table**: the table goes
/// out with a plan, and a clean session sends none, so a sentinel written into
/// the table after the first commit survives a second update.
#[test]
fn an_update_with_no_edit_publishes_no_compensation_table() {
    let mut s = detached();
    let _ = s.update();
    assert!(!s.graph_ref().has_uncommitted_edits());
    let sentinel = [Samples::new(99)];
    s.compensation().set(&sentinel);

    let _ = s.update();

    assert_eq!(s.compensation().to_vec(), sentinel, "no table published");
}

fn detached() -> Session {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .outputs(ChannelLayout::MONO)
        .sample_rate(SampleRate::SR_48K)
        .block_dates(crate::BlockDates::Frames)
        .build()
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
    let mut s = detached();
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
/// and the audio thread still reads the one in force**: a reader's lend shows
/// the old meter and tempo until the update publishes.
#[test]
fn a_read_before_the_update_is_the_pending_value_and_the_audio_thread_reads_the_old_one() {
    let mut s = detached();
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
        s.get(tempo.effective()).unwrap(),
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
    let mut s = detached();
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

/// **The update's `Reclaim` phase frees the MPE mode a block held across a
/// replacement**, in the session's own cell: a reader the mode's cell
/// lends from stands in for a block in progress across the publish.
#[cfg(feature = "midi")]
#[test]
fn an_update_frees_the_mpe_mode_a_block_held_across_a_replacement() {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .outputs(ChannelLayout::MONO)
        .sample_rate(SampleRate::SR_48K)
        .block_dates(crate::BlockDates::Frames)
        .build()
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

/// **A take's start is refused `Full` once `RECORDING_CAPACITY` takes are
/// open, and accepted again when a render passes their stop.** Sixteen takes
/// start and stop at one far frame: a seventeenth start is handed back, and
/// is taken once the session has rendered past that frame. A repeat of a
/// start, and a stop of a take that never started, send nothing.
#[cfg(feature = "capture")]
#[test]
fn a_take_is_refused_while_the_capacity_is_open_and_taken_once_a_render_passes_their_stop() {
    use crate::transport_handle::When;
    use tutti_buffers::InterleavedMut;
    use tutti_timeline::{RECORDING_CAPACITY, Recording, TimedError};
    use tutti_units::Frame;

    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build()
        .expect("builds");
    let handle = s.transport_handle();
    let stop_at = Frame::new(1_000);
    let takes: Vec<_> = (0..RECORDING_CAPACITY)
        .map(|_| handle.next_take_id())
        .collect();
    for &take in &takes {
        handle
            .send_recording(Recording::Start(take), When::NextBlock)
            .expect("room");
        handle
            .send_recording(Recording::Stop(take), When::Frame(stop_at))
            .expect("room");
    }
    let sent = handle.scheduled_outstanding();
    handle
        .send_recording(Recording::Start(takes[0]), When::NextBlock)
        .expect("a take already open is a no-op");
    handle
        .send_recording(Recording::Stop(handle.next_take_id()), When::NextBlock)
        .expect("a take never started is a no-op");
    assert_eq!(handle.scheduled_outstanding(), sent, "neither sent");

    let extra = Recording::Start(handle.next_take_id());
    assert_eq!(
        handle.send_recording(extra, When::NextBlock),
        Err(TimedError::Full(extra))
    );

    let mut block = vec![0.0f32; 512];
    for _ in 0..4 {
        s.render(&mut InterleavedMut::new(&mut block, tutti_buffers::ChannelLayout::MONO).unwrap());
    }
    handle
        .send_recording(extra, When::NextBlock)
        .expect("the render passed every stop");
}

/// **A take's stop the transport's queue refuses is owed, and the next update
/// sends it.** Sixteen takes record; next-block commands fill the queue, so
/// the first take's stop is refused. A render frees the queue, but the take
/// still holds its place (a seventeenth start is refused) until an update
/// sends the owed stop and a render passes it.
#[cfg(feature = "capture")]
#[test]
fn a_takes_stop_the_queue_refuses_is_sent_by_the_next_update() {
    use crate::transport_handle::When;
    use tutti_buffers::InterleavedMut;
    use tutti_timeline::{MotionCommand, RECORDING_CAPACITY, Recording, TimedError};
    use tutti_units::At;

    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build()
        .expect("builds");
    let handle = s.transport_handle();
    let mut block = vec![0.0f32; 512];
    let takes: Vec<_> = (0..RECORDING_CAPACITY)
        .map(|_| handle.next_take_id())
        .collect();
    for &take in &takes {
        handle
            .send_recording(Recording::Start(take), When::NextBlock)
            .expect("room");
    }
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO).unwrap());
    while handle.schedule(At::NextBlock, MotionCommand::Play).is_ok() {}
    let stop = Recording::Stop(takes[0]);
    assert_eq!(
        handle.send_recording(stop, When::NextBlock),
        Err(TimedError::Full(stop))
    );

    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO).unwrap());
    let extra = Recording::Start(handle.next_take_id());
    assert_eq!(
        handle.send_recording(extra, When::NextBlock),
        Err(TimedError::Full(extra)),
        "the first take records on: its stop was not sent"
    );

    let _ = s.update();
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO).unwrap());
    assert_eq!(handle.scheduled_outstanding(), 0, "the owed stop applied");
    handle
        .send_recording(extra, When::NextBlock)
        .expect("the render passed the owed stop");
}
