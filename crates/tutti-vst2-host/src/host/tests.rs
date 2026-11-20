//! Tests of the host callbacks: what the plugin reads and what it queues.

use super::*;
use crate::transport_cell::TransportWriter;
use vst::PluginEvents;

/// The note-on a plugin emits, at frame 0.
fn a_note_on() -> WireMidi {
    WireMidi {
        offset: 0,
        ump: tutti_fixtures::midi::note_on(tutti_ump::MidiChannel::FIRST, 60, 0x8000),
    }
}

use vst::Host;

fn make_host(time_info: TransportReader, default_sample_rate: SampleRate) -> HostState {
    HostState::new(
        tutti_fixtures::main_thread(),
        Arc::new(ArrayQueue::new(PARAM_QUEUE_CAPACITY)),
        Arc::new(ArrayQueue::new(MIDI_OUT_QUEUE_CAPACITY)),
        time_info,
        512,
        default_sample_rate,
        0,
    )
}

/// The rate answers what the plugin was last told, not the block's
/// `TimeInfo` rate: right after a rate change the two differ until the
/// next block, and a plugin querying from inside `effSetSampleRate`
/// allocates for whatever it reads.
/// Spec: vst2.audioMaster.GetSampleRate
#[test]
fn get_sample_rate_is_the_rate_the_plugin_was_told() {
    let mut time_info = TransportWriter::new();
    let host = make_host(time_info.reader(), SampleRate::SR_44K1);
    assert_eq!(host.get_sample_rate(), 44_100.0, "the load-time rate");

    // A block's snapshot at another rate does not move the answer.
    time_info.write(vst::TimeInfo {
        sample_rate: 96_000.0,
        ..Default::default()
    });
    assert_eq!(host.get_sample_rate(), 44_100.0);

    host.set_sample_rate(SampleRate::SR_88K2);
    assert_eq!(host.get_sample_rate(), 88_200.0);
}

/// `audioMasterGetBlockSize` follows `effSetBlockSize`.
/// Spec: vst2.audioMaster.GetBlockSize
#[test]
fn get_block_size_follows_the_announced_block() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    assert_eq!(host.get_block_size(), 512);
    host.set_block_size(8192);
    assert_eq!(host.get_block_size(), 8192);
}

/// `audioMasterIOChanged` is answered "handled" and raises the I/O,
/// latency and tail requests, once each, for the main thread.
/// Spec: vst2.audioMaster.IOChanged
#[test]
fn io_changed_is_handled_and_raises_its_requests_once() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    assert_eq!(
        tutti_plugin_types::taken(host.requests()).count(),
        0,
        "nothing reported yet"
    );
    assert!(host.io_changed(), "answered 1: the host acts on it");
    let asked: Vec<_> = tutti_plugin_types::taken(host.requests()).collect();
    assert_eq!(
        asked,
        [
            Request::IoChanged,
            Request::LatencyChanged,
            Request::TailChanged
        ]
    );
    assert_eq!(
        tutti_plugin_types::taken(host.requests()).count(),
        0,
        "consumed"
    );
}

/// `audioMasterUpdateDisplay` asks for the parameters' names and values to
/// be read again.
///
/// A host nobody called asks for nothing, and the request is consumed by
/// taking it, so one call is one re-read.
/// Spec: vst2.audioMaster.UpdateDisplay
#[test]
fn update_display_asks_for_a_reread_of_names_and_values() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    assert_eq!(
        tutti_plugin_types::taken(host.requests()).count(),
        0,
        "a quiet plugin has asked for nothing"
    );
    host.update_display();
    let asked: Vec<_> = tutti_plugin_types::taken(host.requests()).collect();
    assert_eq!(
        asked,
        [
            Request::RescanParams(RescanScope::Values),
            Request::RescanParams(RescanScope::Info)
        ]
    );
    assert_eq!(
        tutti_plugin_types::taken(host.requests()).count(),
        0,
        "consumed on read"
    );
}

/// `audioMasterSizeWindow` (`[index]` width, `[value]` height) is answered 1 and kept for
/// the main thread, the latest request winning; a size that is not positive is refused
/// and kept nowhere.
/// Spec: vst2.audioMaster.SizeWindow
#[test]
fn size_window_keeps_the_latest_request_for_the_main_thread() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    assert_eq!(host.take_size_request(), None, "nothing asked yet");
    assert!(host.size_window(640, 480));
    assert!(host.size_window(800, 600));
    // At scale 1 a size is the same pair in either unit.
    assert_eq!(
        host.take_size_request().map(|s| s.to_physical::<u32>(1.0)),
        Some(dpi::PhysicalSize::new(800, 600))
    );
    assert_eq!(host.take_size_request(), None, "consumed");
    assert!(!host.size_window(0, 480), "no width");
    assert!(!host.size_window(640, 0), "no height");
    assert!(!host.size_window(-1, 480), "a negative width");
    assert!(!host.size_window(640, -5), "a negative height");
    assert_eq!(host.take_size_request(), None, "a refusal keeps nothing");
}

/// `audioMasterCanDo` claims what tutti does and declines what it does not.
#[test]
fn can_do_claims_what_the_host_supports() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    for yes in [
        "sendVstEvents",
        "sendVstMidiEvent",
        "sendVstTimeInfo",
        "receiveVstEvents",
        "receiveVstMidiEvent",
        "sizeWindow",
        "supportShell",
        "shellCategory",
        "acceptIOChanges",
        "startStopProcess",
        "receiveVstSysexEvent",
    ] {
        assert_eq!(host.can_do(yes), Supported::Yes, "{yes}");
    }
    for no in ["supplyIdle", "openFileSelector", "closeFileSelector"] {
        assert_eq!(host.can_do(no), Supported::No, "{no}");
    }
    // Not claimed: the engine's MIDI has no live/playback distinction to set the flag by.
    assert_eq!(
        host.can_do("sendVstMidiEventFlagIsRealtime"),
        Supported::Maybe
    );
    assert_eq!(host.can_do("somethingNew"), Supported::Maybe);
}

/// `audioMasterCurrentId` answers the sub-plugin the load chose, and `0` (a shell's
/// "no particular id") when it chose none (`aeffect.h`, JUCE's `shellUIDToCreate`).
/// Spec: vst2.audioMaster.CurrentId
#[test]
fn current_id_is_the_chosen_sub_plugin_and_zero_when_none() {
    let none = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    assert_eq!(none.get_plugin_id(), 0);
    let chosen = HostState::new(
        tutti_fixtures::main_thread(),
        Arc::new(ArrayQueue::new(PARAM_QUEUE_CAPACITY)),
        Arc::new(ArrayQueue::new(MIDI_OUT_QUEUE_CAPACITY)),
        TransportWriter::new().reader(),
        512,
        SampleRate::SR_48K,
        0x5348_414C,
    );
    assert_eq!(chosen.get_plugin_id(), 0x5348_414C);
}

/// A plugin polls this callback to decide whether it may spend more per
/// block. Reporting realtime during a bounce is what made an offline render
/// silently produce the live-quality result.
/// Spec: vst2.audioMaster.GetCurrentProcessLevel, vst2.level.kVstProcessLevelOffline, vst2.level.kVstProcessLevelRealtime
#[test]
fn process_level_inside_process_follows_the_render_mode() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);

    host.set_offline(true);
    {
        let _in_process = host.enter_process();
        assert_eq!(host.get_process_level(), ProcessLevel::Offline);
    }

    host.set_offline(false);
    let _in_process = host.enter_process();
    assert_eq!(host.get_process_level(), ProcessLevel::Realtime);
}

/// Outside a process call the answer is the caller's thread: the main thread is
/// `User`, another thread is `Unknown`, and the process scope ending ends `Realtime`.
#[test]
fn process_level_outside_process_is_user_on_main_and_unknown_elsewhere() {
    let host = Arc::new(make_host(
        TransportWriter::new().reader(),
        SampleRate::SR_48K,
    ));
    let _main = tutti_fixtures::main_thread();

    assert_eq!(host.get_process_level(), ProcessLevel::User);
    let other = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || host.get_process_level())
            .join()
            .unwrap()
    };
    assert_eq!(other, ProcessLevel::Unknown, "never a guessed Realtime");

    // A scope open on the main thread does not make another thread a process thread.
    let scope = host.enter_process();
    assert_eq!(host.get_process_level(), ProcessLevel::Realtime);
    let other = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || host.get_process_level())
            .join()
            .unwrap()
    };
    assert_eq!(other, ProcessLevel::Unknown);
    drop(scope);
    assert_eq!(host.get_process_level(), ProcessLevel::User);
}

/// The process thread is not the main thread: inside its scope it answers by the render
/// mode, and after the scope it is `Unknown`.
#[test]
fn process_level_on_a_process_thread_that_is_not_main() {
    let host = Arc::new(make_host(
        TransportWriter::new().reader(),
        SampleRate::SR_48K,
    ));
    let _main = tutti_fixtures::main_thread();
    host.set_offline(true);
    let answers = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            let inside = {
                let _scope = host.enter_process();
                host.get_process_level()
            };
            (inside, host.get_process_level())
        })
        .join()
        .unwrap()
    };
    assert_eq!(answers, (ProcessLevel::Offline, ProcessLevel::Unknown));
}

/// `audioMasterGetAutomationState` answers each mode with its `VstAutomationStates` value
/// (`aeffectx.h`: off 1, read 2, write 3, read/write 4), 1 before any mode is set.
/// Spec: vst2.audioMaster.GetAutomationState, vst2.automation.kVstAutomationOff, vst2.automation.kVstAutomationRead, vst2.automation.kVstAutomationWrite, vst2.automation.kVstAutomationReadWrite
#[test]
fn automation_state_answers_each_mode_with_its_header_value() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    assert_eq!(host.get_automation_state() as i32, 1, "off before any set");
    for (mode, value) in [
        (AutomationMode::Reading, 2),
        (AutomationMode::Writing, 3),
        (AutomationMode::ReadWriting, 4),
        (AutomationMode::Off, 1),
    ] {
        host.set_automation(mode);
        assert_eq!(host.get_automation_state() as i32, value, "{mode:?}");
        assert_eq!(host.automation(), mode, "{mode:?} round-trips");
    }
}

/// The automation queue is bounded, and overflowing it is *reported*.
///
/// The unbounded channel this replaced could not fail a `try_send`, so the
/// `let _ =` on it guarded nothing and every call allocated. Both halves
/// matter: the queue must actually refuse past its capacity, and the
/// refusal must be distinguishable from a plugin that never automated.
#[test]
fn a_full_param_queue_drops_and_counts() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);

    // Nothing dropped before anything overflows — without this the
    // assertion below would also pass on a counter stuck at "always
    // nonzero".
    for i in 0..PARAM_QUEUE_CAPACITY {
        host.automate(i as i32, 0.5);
    }
    assert_eq!(
        host.dropped_param_changes().get(),
        0,
        "a queue filled exactly to capacity must not have dropped anything"
    );

    // One past capacity: refused, and counted.
    host.automate(9_999, 1.0);
    assert_eq!(host.dropped_param_changes().get(), 1);
    host.automate(9_999, 1.0);
    assert_eq!(host.dropped_param_changes().get(), 2);

    // Draining makes room again — the queue is a queue, not a latch.
    assert_eq!(
        host.param_tx.pop(),
        Some(ParameterChange::Automate(0, Normalized::of(0.5)))
    );
    host.automate(7, 0.25);
    assert_eq!(
        host.dropped_param_changes().get(),
        2,
        "a push into freed space must not count as a drop"
    );
}

/// A plugin that repeats the `f32` narrowing of a written value, from any
/// thread and after the write returned, is echoing: nothing is queued, and the
/// `f64` the host wrote is the value held.
#[test]
fn automate_of_the_narrowing_of_a_host_write_is_an_echo_every_time() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    host.size_written(2);
    let written = Normalized::of(0.1 + 1e-12);
    let mut word = 0.0;
    assert!(host.write(1, written, |w| {
        word = w;
        true
    }));
    assert_ne!(f64::from(word), written.get(), "the write must narrow");

    host.automate(1, word);
    host.automate(1, word);

    assert_eq!(host.param_tx.pop(), None, "an echo is not an edit");
    let held = host.written.get().and_then(|cells| cells.recorded(1));
    assert_eq!(held, Some(written), "the cell keeps the f64 written");
}

/// A report that is not the narrowing of the written value is the plugin's own
/// change, reported at the width it gave.
#[test]
fn automate_off_the_narrowing_of_a_host_write_is_reported() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    host.size_written(1);
    let written = Normalized::of(0.1 + 1e-12);
    let mut word = 0.0_f32;
    host.write(0, written, |w| {
        word = w;
        true
    });
    let moved = word.next_up();

    host.automate(0, moved);

    assert_eq!(
        host.param_tx.pop(),
        Some(ParameterChange::Automate(
            0,
            Normalized::clamped(f64::from(moved))
        ))
    );
}

/// Once the plugin reported a value of its own, the narrowing of the old write is
/// no echo of anything: the report that follows is the plugin's change back to it.
#[test]
fn automate_of_the_old_narrowing_after_the_plugins_own_change_is_reported() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    host.size_written(1);
    let mut word = 0.0_f32;
    host.write(0, Normalized::of(0.1 + 1e-12), |w| {
        word = w;
        true
    });
    let own = Normalized::of(0.75);

    host.automate(0, 0.75);
    host.automate(0, word);

    assert_eq!(host.param_tx.pop(), Some(ParameterChange::Automate(0, own)));
    assert_eq!(
        host.param_tx.pop(),
        Some(ParameterChange::Automate(
            0,
            Normalized::clamped(f64::from(word))
        ))
    );
}

/// A state the plugin sets itself forgets the host's writes: its narrowing,
/// reported after, is the plugin's value.
#[test]
fn automate_of_the_narrowing_after_the_written_values_are_cleared_is_reported() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    host.size_written(1);
    let mut word = 0.0_f32;
    host.write(0, Normalized::of(0.1 + 1e-12), |w| {
        word = w;
        true
    });
    host.clear_written();

    host.automate(0, word);

    assert!(host.param_tx.pop().is_some());
}

/// A write the plugin refused is not recorded, so its narrowing is not an echo.
#[test]
fn a_refused_write_records_nothing() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    host.size_written(1);
    let mut word = 0.0_f32;
    assert!(!host.write(0, Normalized::of(0.1 + 1e-12), |w| {
        word = w;
        false
    }));

    host.automate(0, word);

    assert!(host.param_tx.pop().is_some());
}

/// The MIDI-out queue counts its drops separately from the parameter
/// queue: a stuck note and a stale knob readout are different failures and
/// must not share a tally.
#[test]
fn midi_out_drops_are_counted_apart_from_param_drops() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    let ev = a_note_on();

    for _ in 0..MIDI_OUT_QUEUE_CAPACITY {
        assert!(host.midi_out_tx.push(ev).is_ok());
    }
    assert_eq!(host.dropped_midi_out().get(), 0);

    // `process_events` is the only path that pushes; drive it through a
    // real `api::Events` so the count reflects the callback, not the queue.
    let mut api_ev = vst::from_midi(&ev).expect("note-on has a MIDI-1 form");
    let events = vst::Events {
        num_events: 1,
        _reserved: 0,
        events: [&raw mut api_ev as *mut vst::Event, std::ptr::null_mut()],
    };
    host.process_plugin_events(PluginEvents::for_test(&events));

    assert_eq!(
        host.dropped_midi_out().get(),
        1,
        "an event refused by a full queue must be counted"
    );
    assert_eq!(
        host.dropped_param_changes().get(),
        0,
        "a MIDI drop must not move the parameter tally"
    );
}

/// A `vst::SysExEvent` over `bytes`, as a plugin passes it.
fn sysex_event(delta_frames: i32, bytes: &mut [u8]) -> vst::SysExEvent {
    vst::SysExEvent {
        event_type: vst::EventType::SysEx,
        byte_size: std::mem::size_of::<vst::SysExEvent>() as i32,
        delta_frames,
        _flags: 0,
        data_size: bytes.len() as i32,
        _reserved1: 0,
        system_data: bytes.as_mut_ptr(),
        _reserved2: 0,
    }
}

fn deliver(host: &HostState, event: &mut vst::SysExEvent) {
    let events = vst::Events {
        num_events: 1,
        _reserved: 0,
        events: [
            event as *mut vst::SysExEvent as *mut vst::Event,
            std::ptr::null_mut(),
        ],
    };
    host.process_plugin_events(PluginEvents::for_test(&events));
}

/// **A SysEx the plugin emits reaches the host whole, at any length.**
/// `process_events` used to look at `Midi` events only, so every SysEx a
/// VST2 plugin emitted was ignored.
#[test]
fn a_sysex_from_the_plugin_reaches_the_host_whole() {
    for len in [1usize, 6, 7, 40, 200] {
        let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
        let body: Vec<u8> = (0..len).map(|i| (i % 0x7F) as u8).collect();
        let message: Vec<u8> = tutti_ump::frame(&body).collect();
        // The plugin's own buffer, alive for the callback only.
        let mut wire = message.clone();
        let mut event = sysex_event(13, &mut wire);
        deliver(&host, &mut event);

        let mut asm = tutti_plugin_types::sysex_assembler();
        let mut done = Vec::new();
        while let Some(e) = host.midi_out_tx.pop() {
            assert_eq!(e.offset, 13);
            if let tutti_ump::SysexStep::Complete(r) = asm.push(e.ump) {
                done.push(asm.message(r).bytes().to_vec());
            }
        }
        assert_eq!(done, vec![message], "{len} bytes");
        assert_eq!(host.dropped_midi_out().get(), 0);
    }
}

/// **An event whose `byte_size` is shorter than its struct is ignored**,
/// not read past its declared end.
#[test]
fn an_event_shorter_than_its_struct_is_ignored() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    let mut payload = vec![0xF0u8, 1, 2, 0xF7];
    let mut sysex = sysex_event(0, &mut payload);
    sysex.byte_size = 8;
    deliver(&host, &mut sysex);

    let note = a_note_on();
    let mut midi = vst::from_midi(&note).expect("MIDI-1 form");
    midi.byte_size = 8;
    let events = vst::Events {
        num_events: 1,
        _reserved: 0,
        events: [&raw mut midi as *mut vst::Event, std::ptr::null_mut()],
    };
    host.process_plugin_events(PluginEvents::for_test(&events));
    assert!(host.midi_out_tx.is_empty());

    // The same events with their true sizes are delivered.
    sysex.byte_size = std::mem::size_of::<vst::SysExEvent>() as i32;
    deliver(&host, &mut sysex);
    assert!(!host.midi_out_tx.is_empty());
}

/// **A malformed event list from the plugin is ignored, not followed.** A
/// negative `num_events` is no events (it was `as usize`, about 2^64
/// iterations over the array), a null entry is skipped, and an event whose
/// type word is no `EventType` variant is skipped without ever being read as
/// the enum (reading one is undefined behaviour, which no test can show).
#[test]
fn a_malformed_event_list_is_ignored() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);

    let negative = vst::Events {
        num_events: -1,
        _reserved: 0,
        events: [std::ptr::null_mut(); 2],
    };
    host.process_plugin_events(PluginEvents::for_test(&negative));
    let negative_max = vst::Events {
        num_events: i32::MIN,
        ..negative
    };
    host.process_plugin_events(PluginEvents::for_test(&negative_max));

    // Both entries null: skipped, not dereferenced.
    let nulls = vst::Events {
        num_events: 2,
        _reserved: 0,
        events: [std::ptr::null_mut(); 2],
    };
    host.process_plugin_events(PluginEvents::for_test(&nulls));

    // A valid note-on, but with a type word (77) that is no variant: it must not
    // be read as a `Midi` event, nor as the enum at all.
    let note = a_note_on();
    let valid = vst::from_midi(&note).expect("MIDI-1 form");
    // The note-on as plain words (the layout `MidiEvent` is `repr(C)` in), then the
    // type word set to the bogus one.
    let [d0, d1, d2] = valid.midi_data;
    let mut bogus: [i32; 8] = [
        0,
        valid.byte_size,
        valid.delta_frames,
        valid.flags,
        valid.note_length,
        valid.note_offset,
        i32::from_ne_bytes([d0, d1, d2, valid._midi_reserved]),
        i32::from_ne_bytes([
            valid.detune as u8,
            valid.note_off_velocity,
            valid._reserved1,
            valid._reserved2,
        ]),
    ];
    bogus[0] = 77;
    let events = vst::Events {
        num_events: 1,
        _reserved: 0,
        events: [bogus.as_mut_ptr().cast(), std::ptr::null_mut()],
    };
    host.process_plugin_events(PluginEvents::for_test(&events));

    assert!(host.midi_out_tx.is_empty());
    assert_eq!(host.dropped_midi_out().get(), 0);
}

/// A SysEx that does not fit the queue is dropped whole and counted once,
/// never left as a `START` without its `END`.
#[test]
fn a_sysex_past_the_queue_is_dropped_whole_and_counted() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    let mut payload = vec![1u8; MIDI_OUT_QUEUE_CAPACITY * 6 + 1];
    let mut event = sysex_event(0, &mut payload);
    deliver(&host, &mut event);
    assert!(host.midi_out_tx.is_empty());
    assert_eq!(host.dropped_midi_out().get(), 1);
}

/// **The SysEx path is allocation-free**, accepting and refusing: a
/// 200-byte message is delivered and drained, then more are delivered
/// until the queue refuses one, then it is drained again.
#[test]
fn the_sysex_path_is_allocation_free() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    let mut payload: Vec<u8> = (0..200).map(|i| (i % 0x7F) as u8).collect();
    let mut event = sysex_event(3, &mut payload);
    // 200 payload bytes, six to a packet (M2-104 section 7.7): 34 packets.
    let packets = 34;

    assert_no_alloc::assert_no_alloc(|| {
        deliver(&host, &mut event);
        let mut drained = 0;
        while host.midi_out_tx.pop().is_some() {
            drained += 1;
        }
        assert_eq!(drained, packets);

        // Past the room: the refusing branch.
        for _ in 0..MIDI_OUT_QUEUE_CAPACITY / packets + 2 {
            deliver(&host, &mut event);
        }
        while host.midi_out_tx.pop().is_some() {}
    });
    assert!(host.dropped_midi_out().get() > 0, "the refusing branch ran");
}

/// **A plugin's malformed MIDI bytes are dropped without allocating.** A
/// VST2 MIDI event is a fixed three-byte field, so a stray `0xF7`, an
/// undefined status and a data byte with its top bit set are what a plugin
/// can get wrong; none reaches the queue or counts as a drop, a valid
/// message beside them does, and nothing allocates.
#[test]
fn a_stray_status_from_a_plugin_allocates_nothing() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    let note = a_note_on();
    let mut api_ev = vst::from_midi(&note).expect("note-on has a MIDI-1 form");
    let good = api_ev.midi_data;
    let events = |ev: &mut vst::MidiEvent| vst::Events {
        num_events: 1,
        _reserved: 0,
        events: [
            std::ptr::from_mut(ev).cast::<vst::Event>(),
            std::ptr::null_mut(),
        ],
    };

    assert_no_alloc::assert_no_alloc(|| {
        for hostile in [
            [0xF7, 1, 2],
            [0xF4, 1, 2],
            [0xFD, 0, 0],
            [0xF0, 1, 2],
            [0x90, 0x80, 1],
            [0x90, 60, 0xF8],
            [1, 2, 3],
        ] {
            api_ev.midi_data = hostile;
            host.process_plugin_events(PluginEvents::for_test(&events(&mut api_ev)));
        }
        assert!(
            host.midi_out_tx.is_empty(),
            "a malformed message was queued"
        );
        assert_eq!(host.dropped_midi_out().get(), 0);

        api_ev.midi_data = good;
        host.process_plugin_events(PluginEvents::for_test(&events(&mut api_ev)));
        assert_eq!(host.midi_out_tx.len(), 1);
    });
}

/// **The RT gate.** Both callbacks are entered from inside the plugin's
/// `processReplacing`, so neither may allocate — in the accepting case or
/// in the refusing one.
///
/// This is the property the unbounded channel could not have: every
/// `try_send` on one allocated a list node, and there was no capacity at
/// which it stopped. The queue's storage is allocated once, at
/// construction, outside the gate.
///
/// `src/lib.rs` installs `AllocDisabler` for the unit-test binary, so this
/// observes something.
#[test]
fn both_audio_thread_callbacks_are_allocation_free() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    let ev = a_note_on();
    let mut api_ev = vst::from_midi(&ev).expect("note-on has a MIDI-1 form");
    let events = vst::Events {
        num_events: 1,
        _reserved: 0,
        events: [&raw mut api_ev as *mut vst::Event, std::ptr::null_mut()],
    };

    assert_no_alloc::assert_no_alloc(|| {
        // Enough iterations to fill both queues and then keep going, so
        // the accepting path and the refusing path are both inside the
        // gate. Neither may allocate.
        for i in 0..(PARAM_QUEUE_CAPACITY + MIDI_OUT_QUEUE_CAPACITY) * 2 {
            host.automate(i as i32, 0.5);
            host.process_plugin_events(PluginEvents::for_test(&events));
        }
    });

    // Both queues actually overflowed — otherwise the gate above only
    // covered the accepting half and would pass on a refusing path that
    // allocates.
    assert!(host.dropped_param_changes().get() > 0);
    assert!(host.dropped_midi_out().get() > 0);
}

/// **Every other `audioMaster` callback the audio thread can reach allocates nothing.**
/// A plugin asks for the time, the rate, the block size, the process level, the host's
/// name, its latencies, its automation state and what it can do, and raises edits and
/// notices, from inside `process`; each is answered from fixed storage or a constant.
/// (`automate` and `process_plugin_events`, which fill queues, are covered by
/// `both_audio_thread_callbacks_are_allocation_free`.)
#[test]
fn every_query_and_notice_the_audio_thread_reaches_is_allocation_free() {
    let host = make_host(TransportWriter::new().reader(), SampleRate::SR_48K);
    let _scope = host.enter_process();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..4 {
            let _ = host.get_time_info(0);
            let _ = host.get_block_size();
            let _ = host.get_sample_rate();
            let _ = host.get_process_level();
            let _ = host.get_input_latency();
            let _ = host.get_output_latency();
            let _ = host.get_automation_state();
            let _ = host.get_plugin_id();
            let _ = host.can_do("sendVstTimeInfo");
            let _ = host.can_do("a capability nobody asked about");
            let _ = host.io_changed();
            host.update_display();
            let _ = host.size_window(640, 480);
            host.begin_edit(0);
            host.end_edit(0);
            host.idle();
            host.refused_callback(1);
        }
    });
}
