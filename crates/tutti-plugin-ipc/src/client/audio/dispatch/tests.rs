use super::super::channels::COMMAND_QUEUE_SIZE;
use super::*;
use tutti_plugin_types::Timeouts;
use tutti_units::Chunks;
use tutti_units::ParamId;

fn processed(seq: u64) -> AudioReply {
    AudioReply::AudioProcessed {
        latency_us: 0,
        seq,
        midi_out: Default::default(),
        status: Default::default(),
        refused: false,
        writes: crate::BlockWrites::Handed,
    }
}

fn value(id: u64) -> ControlFrame {
    ControlFrame::Reply {
        id: ReqId::new(id),
        reply: ControlReply::ParameterValue {
            value: Some(tutti_units::Normalized::of(0.25)),
        },
    }
}

const WINDOW: Duration = Duration::from_secs(5);

fn audio_state() -> (AudioState, super::super::channels::Replies, Channels) {
    let (writer, replies, _lost) = response_ring();
    let channels = Channels::new(SampleRate::SR_48K, std::sync::Arc::default())
        .expect("the channels")
        .0;
    (
        AudioState::new(Timeouts::default().drain, writer),
        replies,
        channels,
    )
}

/// Opens `key` on `state` and gives up on it: its reply is late.
fn abandoned(state: &mut AudioState, key: AudioKey, late: AbandonedAudio) {
    state.conv.open(key, late, Instant::now(), WINDOW, WINDOW);
    state.give_up(key).expect("room to give up");
}

/// **An abandoned block's reply is forwarded and counted late, and the next
/// block's answer is its own**: the reply is recognised by its sequence, so
/// it is neither taken as the answer nor dropped, whatever order the two
/// arrive in.
#[test]
fn a_late_block_reply_is_forwarded_and_counted_late() {
    let (mut state, mut replies, channels) = audio_state();
    abandoned(
        &mut state,
        AudioKey::Block(3),
        AbandonedAudio::Block { ord: 0 },
    );
    state.conv.open(
        AudioKey::Block(4),
        AbandonedAudio::Block { ord: 0 },
        Instant::now(),
        WINDOW,
        WINDOW,
    );

    let late = state.hear(processed(3), AudioKey::Block(4), &channels);
    assert!(matches!(late, Ok(None)), "late, not the answer");
    assert!(matches!(
        replies.pop(),
        Some(AudioResponse::AudioProcessed { seq: 3, .. })
    ));
    assert_eq!(channels.block_counts().late, Chunks::new(1));

    let answer = state.hear(processed(4), AudioKey::Block(4), &channels);
    assert!(matches!(
        answer,
        Ok(Some(AudioReply::AudioProcessed { seq: 4, .. }))
    ));
    assert_eq!(
        channels.block_counts().late,
        Chunks::new(1),
        "an answer is not late"
    );
}

/// **A flush carries what skipped blocks kept**: a change of a block skipped
/// as stale is in the frame a flush sends.
#[test]
fn a_flush_frame_carries_what_a_skipped_block_kept() {
    let (mut state, _replies, channels) = audio_state();
    let mut skipped = ParameterChanges::reserved_bridged(1, 1);
    let _ = skipped.add_change(ParamId::new(4), Offset::ZERO, unit_value(0.4));
    state.carry(&skipped);
    let (frame, _) = state.flush_frame(&channels);
    let queue = frame.queue(ParamId::new(4)).expect("the carried write");
    assert_eq!(queue.points.len(), 1);
    assert!((queue.points[0].value.get() - 0.4).abs() < 1e-12);
}

/// The host's slots on `channels`, and the writer of them.
fn host_slots(channels: &Channels) -> tutti_plugin_types::EditWriter {
    let (writer, reader) = tutti_plugin_types::edit_slots([ParamId::new(1), ParamId::new(2)]);
    channels.install_host_edits(reader);
    writer
}

/// One request's drain of the host's slots, as a block takes it: the
/// request's number.
fn drain_request(state: &mut AudioState, channels: &Channels) -> u64 {
    let mut fresh = Vec::new();
    let drained = channels
        .drain_host_edits(|id, value, epoch| fresh.push(Taken { id, value, epoch }))
        .map(|drain| (fresh, drain));
    state.begin_flight(drained)
}

fn write(writer: &mut tutti_plugin_types::EditWriter, id: u32, value: f64) {
    writer
        .write(ParamId::new(id), tutti_units::Normalized::of(value))
        .expect("declared");
}

fn sent(state: &AudioState, ord: u64) -> Vec<(u32, f64)> {
    let flight = state.flights.iter().find(|f| f.ord == ord).expect("open");
    flight
        .writes
        .iter()
        .map(|w| (w.id.get(), w.value.get()))
        .collect()
}

/// **A request the server handed over is applied once it is answered, not
/// when it is drained or sent**: the write stays pending until the answer.
#[test]
fn a_handed_request_applies_its_writes_when_answered() {
    let (mut state, _replies, channels) = audio_state();
    let mut writer = host_slots(&channels);
    write(&mut writer, 1, 0.5);
    let ord = drain_request(&mut state, &channels);
    assert_eq!(sent(&state, ord), [(1, 0.5)]);
    assert_eq!(writer.pending().len(), 1, "drained and sent, not answered");
    state.answered(&channels, ord, true);
    assert!(writer.pending().is_empty(), "answered as handed over");
}

/// **A request the server did not hand over loses its writes, counted, and
/// owes them again: nothing is called applied, and the next request carries
/// them.**
#[test]
fn a_lost_request_owes_its_writes_to_the_next() {
    let (mut state, _replies, channels) = audio_state();
    let mut writer = host_slots(&channels);
    write(&mut writer, 1, 0.5);
    let lost = drain_request(&mut state, &channels);
    state.answered(&channels, lost, false);
    assert_eq!(channels.unapplied_edits_cell().get(), 1, "counted");
    assert_eq!(writer.pending().len(), 1, "a lost write is not applied");

    let next = drain_request(&mut state, &channels);
    assert_eq!(sent(&state, next), [(1, 0.5)], "owed again");
    state.answered(&channels, next, true);
    assert!(writer.pending().is_empty());
}

/// **A newer write of a parameter replaces the owed one, whichever request
/// carried it**: a request lost after a later one carried the parameter owes
/// nothing of it, and a write made since the loss is the one sent.
#[test]
fn a_newer_write_replaces_an_owed_one() {
    let (mut state, _replies, channels) = audio_state();
    let mut writer = host_slots(&channels);
    write(&mut writer, 1, 0.25);
    write(&mut writer, 2, 0.75);
    let first = drain_request(&mut state, &channels);
    write(&mut writer, 1, 0.5);
    let second = drain_request(&mut state, &channels);
    assert_eq!(sent(&state, second), [(1, 0.5)]);
    // The first request's late answer says it was lost: parameter 1 was
    // carried since by a newer write, parameter 2 was not.
    state.answered(&channels, first, false);
    let third = drain_request(&mut state, &channels);
    assert_eq!(
        sent(&state, third),
        [(2, 0.75)],
        "only what nothing newer carried"
    );

    write(&mut writer, 2, 0.125);
    state.answered(&channels, third, false);
    let fourth = drain_request(&mut state, &channels);
    assert_eq!(
        sent(&state, fourth),
        [(2, 0.125)],
        "a write made since the loss replaces the owed one"
    );
}

/// **A reset voids what is owed**: a write made before it is sent by no
/// request after it.
#[test]
fn a_reset_voids_the_writes_owed() {
    let (mut state, _replies, channels) = audio_state();
    let mut writer = host_slots(&channels);
    write(&mut writer, 1, 0.5);
    let lost = drain_request(&mut state, &channels);
    state.answered(&channels, lost, false);
    writer.reset();
    let next = drain_request(&mut state, &channels);
    assert!(sent(&state, next).is_empty(), "the owed write is stale");
    state.answered(&channels, next, true);
    assert!(writer.pending().is_empty());
}

/// **An answer for a table the host has replaced publishes nothing and owes
/// nothing to the new one**: its generations count from another writer's.
#[test]
fn an_answer_from_a_replaced_table_is_not_the_new_ones() {
    let (mut state, _replies, channels) = audio_state();
    let mut old = host_slots(&channels);
    write(&mut old, 1, 0.5);
    let ord = drain_request(&mut state, &channels);
    let mut new = host_slots(&channels);
    write(&mut new, 1, 0.75);
    // The new table is drained after the old request was sent.
    let next = drain_request(&mut state, &channels);
    state.answered(&channels, ord, true);
    assert_eq!(new.pending().len(), 1, "the old table's answer");
    state.answered(&channels, next, false);
    let again = drain_request(&mut state, &channels);
    assert_eq!(sent(&state, again), [(1, 0.75)], "owed from the new table");
}

/// **A write owed is not covered by a later answer**: a request handed over
/// while another's writes are owed publishes nothing (one generation cannot
/// say that a later write is applied and an earlier is not), so both stay
/// pending until the request that carries the owed one is answered.
#[test]
fn an_answer_does_not_cover_a_write_still_owed() {
    let (mut state, _replies, channels) = audio_state();
    let mut writer = host_slots(&channels);
    write(&mut writer, 1, 0.5);
    let first = drain_request(&mut state, &channels);
    write(&mut writer, 2, 0.25);
    let second = drain_request(&mut state, &channels);
    state.answered(&channels, first, false);
    state.answered(&channels, second, true);
    assert_eq!(writer.pending().len(), 2, "the owed write is not applied");

    let third = drain_request(&mut state, &channels);
    assert_eq!(sent(&state, third), [(1, 0.5)]);
    state.answered(&channels, third, true);
    assert!(writer.pending().is_empty(), "the owed write landed");
}

/// **A late acknowledgement of `ApplyParameters` is dropped without
/// answering anything**, and is not counted as a late block.
#[test]
fn a_late_apply_parameters_acknowledgement_is_dropped() {
    let (mut state, mut replies, channels) = audio_state();
    let id = state.fresh();
    abandoned(
        &mut state,
        AudioKey::Params(id),
        AbandonedAudio::Params { ord: 0 },
    );
    let heard = state.hear(
        AudioReply::ParametersApplied {
            id,
            outcome: ApplyOutcome::Applied,
        },
        AudioKey::Block(1),
        &channels,
    );
    assert!(matches!(heard, Ok(None)));
    assert!(replies.pop().is_none());
    assert_eq!(channels.block_counts().late, Chunks::ZERO);
}

/// **A reply to a request this lane never made is the server's fault**: a
/// block never sent, an acknowledgement nobody asked for.
#[test]
fn a_reply_to_a_request_never_made_is_a_fault() {
    let (mut state, _replies, channels) = audio_state();
    state.conv.open(
        AudioKey::Block(1),
        AbandonedAudio::Block { ord: 0 },
        Instant::now(),
        WINDOW,
        WINDOW,
    );
    for reply in [
        processed(9),
        AudioReply::ParametersApplied {
            id: ReqId::new(77),
            outcome: ApplyOutcome::Applied,
        },
    ] {
        assert!(
            matches!(
                state.hear(reply, AudioKey::Block(1), &channels),
                Err(IpcError::ProtocolError(_))
            ),
            "a reply to nothing this lane sent was accepted"
        );
    }
}

/// **A reply to a request this lane has open but is not awaiting is the
/// server's fault**: block 2 answered while block 1 is the one waited on.
#[test]
fn an_answer_to_another_open_block_is_a_fault() {
    let (mut state, _replies, channels) = audio_state();
    for seq in [1, 2] {
        state.conv.open(
            AudioKey::Block(seq),
            AbandonedAudio::Block { ord: 0 },
            Instant::now(),
            WINDOW,
            WINDOW,
        );
    }
    assert!(matches!(
        state.hear(processed(2), AudioKey::Block(1), &channels),
        Err(IpcError::ProtocolError(_))
    ));
}

/// **A second `Hello`, and a `LaneOpened` nobody asked for, are the server's
/// fault** on a control connection that is in use: the version is said once,
/// and a lane is opened only by a request.
#[test]
fn a_second_hello_and_an_unrequested_lane_answer_are_faults() {
    let mut state = control_state();
    let mut inbox = Unsolicited::new();
    let id = open(&mut state, 1, Abandoned::Discard);
    let hello = ControlFrame::Hello {
        protocol_version: crate::PROTOCOL_VERSION,
    };
    let unrequested = ControlFrame::Reply {
        id: ReqId::new(9),
        reply: ControlReply::LaneOpened,
    };
    for frame in [hello, unrequested] {
        assert!(
            matches!(
                state.hear(frame, Some(id), Instant::now(), &mut inbox),
                Err(IpcError::ProtocolError(_))
            ),
            "the frame was accepted"
        );
    }
}

fn control_state() -> ControlState {
    ControlState::new(Timeouts::default().drain)
}

fn open(state: &mut ControlState, id: u64, late: Abandoned) -> ReqId {
    let id = ReqId::new(id);
    state.conv.open(id, late, Instant::now(), WINDOW, WINDOW);
    id
}

/// **A late control reply is dropped by its id, and the answer to the next
/// request is its own.**
#[test]
fn a_late_control_reply_is_dropped_and_the_next_answer_is_its_own() {
    let mut state = control_state();
    let mut inbox = Unsolicited::new();
    let query = open(&mut state, 1, Abandoned::Discard);
    state.give_up(query).unwrap();
    let next = open(&mut state, 3, Abandoned::Discard);

    assert!(matches!(
        state.hear(value(1), Some(next), Instant::now(), &mut inbox),
        Ok(None)
    ));
    let list = ControlFrame::Reply {
        id: next,
        reply: ControlReply::ParameterList {
            parameters: Vec::new(),
        },
    };
    assert!(matches!(
        state.hear(list, Some(next), Instant::now(), &mut inbox),
        Ok(Some(ControlReply::ParameterList { .. }))
    ));
}

/// **A state request opened with the state's window lives on the server's
/// beats, a window after the last one, and never past its ceiling.** The
/// window and the ceiling are the ones `Deadline::State` hands the lane;
/// the beats come every two seconds short of the window, so five of them
/// carry the request past three windows.
#[test]
fn a_state_request_lives_on_beats_past_its_window_and_stops_at_the_ceiling() {
    let (window, ceiling) = super::super::messages::Deadline::State.window(&Timeouts::default());
    let start = Instant::now();
    let at = |secs: u64| start + Duration::from_secs(secs);
    let mut state = control_state();
    let mut inbox = Unsolicited::new();
    let id = ReqId::new(1);
    state
        .conv
        .open(id, Abandoned::Discard, start, window, ceiling);
    let beat = window.as_secs() - 2;
    for step in 1..=5u64 {
        let now = at(step * beat);
        assert!(!state.conv.is_due(id, now), "beat {step} came in time");
        let working = ControlFrame::Working {
            id,
            stage: crate::Working::Opening,
        };
        assert!(matches!(
            state.hear(working, Some(id), now, &mut inbox),
            Ok(None)
        ));
    }
    assert!(5 * beat > 3 * window.as_secs(), "past three windows");
    let last = at(5 * beat);
    assert!(
        !state
            .conv
            .is_due(id, last + window - Duration::from_secs(1))
    );
    assert!(state.conv.is_due(id, last + window));
    let working = ControlFrame::Working {
        id,
        stage: crate::Working::Opening,
    };
    assert!(
        state
            .hear(working, Some(id), at(ceiling.as_secs() - 1), &mut inbox)
            .is_ok()
    );
    assert!(
        state.conv.is_due(id, at(ceiling.as_secs())),
        "the ceiling stands"
    );
}

/// **The server's `Error` answers the request it names**, like any reply,
/// and no other.
#[test]
fn an_error_answers_only_its_own_request() {
    let mut state = control_state();
    let mut inbox = Unsolicited::new();
    let first = open(&mut state, 1, Abandoned::Discard);
    let error = |id| ControlFrame::Reply {
        id,
        reply: ControlReply::Error {
            error: tutti_plugin_types::ServerError::StateWithoutObject,
        },
    };
    assert!(matches!(
        state.hear(error(first), Some(first), Instant::now(), &mut inbox),
        Ok(Some(ControlReply::Error { .. }))
    ));
    assert!(
        state
            .hear(
                error(ReqId::new(9)),
                Some(first),
                Instant::now(),
                &mut inbox
            )
            .is_err(),
        "an error for a request nobody made"
    );
    let second = open(&mut state, 2, Abandoned::Discard);
    assert!(
        state
            .hear(error(second), Some(first), Instant::now(), &mut inbox)
            .is_err(),
        "an error for an open request that is not the one awaited"
    );
}

/// **A sign of life restarts the request's wait, and news does not**: the
/// frames of a chatty plugin do not hold a request open. Driven with
/// constructed instants.
#[test]
fn a_sign_of_life_restarts_the_wait_and_news_does_not() {
    let mut state = control_state();
    let mut inbox = Unsolicited::new();
    let t = Instant::now();
    let id = ReqId::new(1);
    state
        .conv
        .open(id, Abandoned::Discard, t, WINDOW, Duration::from_secs(600));
    let at = |secs| t + Duration::from_secs(secs);

    let news = ControlFrame::News(News::PluginReloaded);
    assert!(matches!(
        state.hear(news, Some(id), at(3), &mut inbox),
        Ok(None)
    ));
    assert_eq!(state.conv.left(id, at(3)), Some(Duration::from_secs(2)));

    let working = ControlFrame::Working {
        id,
        stage: crate::Working::Opening,
    };
    assert!(matches!(
        state.hear(working, Some(id), at(3), &mut inbox),
        Ok(None)
    ));
    assert_eq!(state.conv.left(id, at(3)), Some(WINDOW));
}

/// **A carried value goes into a reserved queue of the block's buffer**,
/// so the buffer returns to the pool with its point room intact. A
/// queue pushed past the spares (the carry's own, with the inline room)
/// later took a reserved queue's place on `clear` and left the buffer
/// with a 10-point queue among its reserved ones.
#[test]
fn a_carried_value_fills_a_reserved_queue() {
    let (mut state, _replies, _channels) = audio_state();
    let mut skipped = ParameterChanges::reserved(1, 1);
    skipped.add_change(ParamId::new(7), at_frame(3), unit_value(0.25));
    state.carry(&skipped);

    let mut block = ParameterChanges::reserved(4, 40);
    assert!(block.open_queue(ParamId::new(1)));
    if let Some(q) = block.queues.last_mut() {
        q.add_point(at_frame(5), unit_value(0.5));
    }
    state.merge_carried(&mut block);

    let carried = block.queue(ParamId::new(7)).expect("carried");
    assert_eq!(carried.points.len(), 1);
    assert_eq!(carried.points[0].sample_offset.get(), 0);
    assert!((carried.points[0].value.get() - 0.25).abs() < 1e-12);
    assert!(
        carried.points.capacity() >= 40,
        "the carried queue is a reserved one: {}",
        carried.points.capacity()
    );
    block.clear();
    assert_eq!(block.point_storage(), 4 * 41, "the reserved storage");
}

/// **A reserved buffer with no spare queue drops a carried value,
/// counted, rather than growing; the empty placeholder takes every one.**
/// A one-queue buffer with its queue open for another parameter: the
/// carried value is dropped inside the no-alloc gate and counted, and the
/// buffer still has one queue. The placeholder of a block with no
/// changes (`ParameterChanges::new`) gets a queue for each of two
/// carried values, though the first gives it queue room.
#[test]
fn a_carried_value_past_the_queues_is_dropped_but_the_placeholder_takes_all() {
    let mut skipped = ParameterChanges::reserved(2, 1);
    skipped.add_change(ParamId::new(7), at_frame(3), unit_value(0.25));
    skipped.add_change(ParamId::new(8), at_frame(3), unit_value(0.5));

    let mut state = audio_state().0;
    state.carry(&skipped);
    let mut block = ParameterChanges::reserved_bridged(1, 40);
    assert!(block.add_change(ParamId::new(1), at_frame(5), unit_value(0.5)));
    assert_no_alloc::assert_no_alloc(|| state.merge_carried(&mut block));
    assert_eq!(block.queues.len(), 1);
    assert_eq!(block.points_overflowed(), 2);

    let mut state = audio_state().0;
    state.carry(&skipped);
    let mut placeholder = ParameterChanges::new();
    state.merge_carried(&mut placeholder);
    assert_eq!(placeholder.queues.len(), 2);
    assert_eq!(placeholder.points_overflowed(), 0);
    for (id, value) in [(7, 0.25), (8, 0.5)] {
        let q = placeholder.queue(ParamId::new(id as u32)).expect("queued");
        assert_eq!(q.points.len(), 1);
        assert_eq!(
            (q.points[0].sample_offset.get(), q.points[0].value.get()),
            (0, value)
        );
    }
}

/// **The carried values reach a chunk at the pool budget whole, in real
/// slots, without allocating or growing it.** 2 000 parameters at a
/// 512-frame block, the block's buffer built `reserved_bridged` as the
/// buffer pool builds it. The block moves all but the last ten
/// parameters, every one at its floor (first point after frame 0) and
/// the last `POOLED_QUEUES` of them at their full room; a skipped block
/// left a value state for every parameter. Inside the no-alloc gate,
/// `merge_carried` puts each on frame 0 (the ten unmoved ones in spare
/// queues): nothing is counted, every queue gained exactly its carried
/// point ahead of its own, and the buffer's storage is what it was.
#[test]
fn carried_values_take_real_slots_at_the_pool_budget_without_allocating() {
    const PARAMS: usize = 2_000;
    const UNMOVED: usize = 10;
    let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
    let moved = PARAMS - UNMOVED;
    let full = moved - ParameterChanges::POOLED_QUEUES..moved;
    let addr = |i: usize| ParamId::new((i as i32) as u32);

    let mut skipped = ParameterChanges::reserved(PARAMS, 1);
    for i in 0..PARAMS {
        assert!(skipped.add_change(addr(i), at_frame(7), unit_value(0.25)));
    }
    let mut state = audio_state().0;
    state.carry(&skipped);

    // Filled as the node's delivery fills it: a full queue takes points
    // while `make_room` gives it room (the producers' room, `room`).
    let mut block = ParameterChanges::reserved_bridged(PARAMS, room);
    for i in 0..moved {
        assert!(block.open_queue(addr(i)));
        if full.contains(&i) {
            let mut k = 0;
            while block.make_room(i) {
                assert!(block.push_point(i, at_frame(1 + k), unit_value(0.5)));
                k += 1;
            }
        } else {
            for k in 0..ParameterChanges::FLOOR_POINTS {
                assert!(block.push_point(i, at_frame(1 + k as i32), unit_value(0.5)));
            }
        }
    }
    let before: Vec<usize> = block.queues.iter().map(|q| q.points.len()).collect();
    let storage = block.point_storage();

    assert_no_alloc::assert_no_alloc(|| state.merge_carried(&mut block));

    assert_eq!(block.points_overflowed(), 0);
    assert_eq!(block.point_storage(), storage, "nothing grew");
    assert_eq!(block.queues.len(), PARAMS);
    for (i, q) in block.queues.iter().enumerate() {
        let own = before.get(i).copied().unwrap_or(0);
        assert_eq!(q.points.len(), own + 1, "queue {i}");
        assert_eq!(q.points[0].sample_offset.get(), 0, "queue {i}");
        assert!((q.points[0].value.get() - 0.25).abs() < 1e-12);
    }
}

/// Block sizes and rates a device can present, a long period included: a
/// chunk is one device callback, so the timeout must hold at every size.
const RATES: [SampleRate; 4] = [
    SampleRate::SR_44K1,
    SampleRate::SR_48K,
    SampleRate::SR_96K,
    SampleRate::SR_192K,
];
const BLOCKS: [usize; 7] = [64, 128, 256, 512, 2_400, 4_800, 8_192];

fn period(num_samples: usize, rate: SampleRate) -> Duration {
    Duration::from_secs_f64(num_samples as f64 / rate.get())
}

/// **The timeout outlasts the ring**: the bridge waits at least as long as
/// the ring takes to recycle a slot ([`MAX_BEHIND`] block periods), or it
/// would abandon a block whose reply is still wanted.
#[test]
fn the_timeout_outlasts_the_ring() {
    for rate in RATES {
        for n in BLOCKS {
            let t = process_timeout(n, rate);
            let ring = period(n, rate) * MAX_BEHIND as u32;
            assert!(
                t >= ring,
                "n={n} rate={rate}: timeout {t:?} is under the ring's lifetime {ring:?}"
            );
        }
    }
}

/// **One slow reply cannot fill the command queue**: while the bridge thread
/// waits it dequeues nothing and the audio thread queues one command per
/// block, so the wait must stay under [`COMMAND_QUEUE_SIZE`] block periods.
/// A fixed 500 ms against a 64-frame block at 48 kHz is some 750 periods.
#[test]
fn one_slow_reply_cannot_fill_the_command_queue() {
    for rate in RATES {
        for n in BLOCKS {
            let t = process_timeout(n, rate);
            let queue = period(n, rate) * COMMAND_QUEUE_SIZE as u32;
            assert!(
                t < queue,
                "n={n} rate={rate}: timeout {t:?} lets the audio thread fill the \
                 {COMMAND_QUEUE_SIZE}-slot queue ({queue:?})"
            );
        }
    }
}

/// **The floor is 2 ms**: at the shortest block and highest rate the period
/// is 333 us, under the scheduler's wake latency, so the timeout is the
/// floor and not a few periods of it.
#[test]
fn the_timeout_never_drops_under_two_milliseconds() {
    assert_eq!(
        process_timeout(64, SampleRate::SR_192K),
        Duration::from_millis(2)
    );
}

/// A backlog collapses to the live blocks in one pass rather than being
/// ground through. Sending all ten, each a socket round-trip, loses ground
/// while the audio thread keeps adding more.
#[test]
fn a_backlog_collapses_to_the_live_blocks() {
    let newest = 10u64;
    let survivors: Vec<u64> = (0..=newest).filter(|&s| !is_stale(s, newest)).collect();
    assert_eq!(survivors, vec![9, 10]);
}

/// **The invariant, stated as a property rather than as a number.**
///
/// No block this admits may share a ring slot with the newest one. If it
/// did, the server would write that block's output into the slot the host
/// is reading for `newest` and stamp the slot's sequence with its own —
/// tearing the block being read, or destroying the evidence for it.
///
/// Asserting specific numbers (`MAX_BEHIND == RING_SLOTS`, `survivors ==
/// [8, 9, 10]`) would pin an off-by-one as intended behaviour: at depth 2,
/// `slot_for(8) == slot_for(10)`. Asserting the property makes the tests
/// independent of the ring depth, so raising `RING_SLOTS` cannot silently
/// introduce it.
#[test]
fn no_admitted_block_shares_a_slot_with_the_newest() {
    // Mirrors `shm::header::slot_for`, which is private to that module.
    // Duplicated deliberately: importing it would couple this test to the
    // slab's internals, and the mapping (`seq % RING_SLOTS`) is the *wire*
    // contract both sides implement, not an implementation detail.
    let slot_of = |seq: u64| seq % RING_SLOTS as u64;

    let newest = 64u64;
    for seq in 0..=newest {
        if is_stale(seq, newest) {
            continue;
        }
        assert!(
            seq == newest || slot_of(seq) != slot_of(newest),
            "block {seq} is {} behind and admitted, but shares slot {} with \
             the newest block {newest} — the server would overwrite the slot \
             the host is reading",
            newest - seq,
            slot_of(seq),
        );
    }
}

/// `newest < seq` cannot occur — the bridge cannot dequeue a block that was
/// never pushed — but the comparison must be total, and "send it" is the
/// safe direction if it somehow did.
#[test]
fn an_impossible_future_block_is_not_dropped() {
    assert!(!is_stale(20, 10));
}
