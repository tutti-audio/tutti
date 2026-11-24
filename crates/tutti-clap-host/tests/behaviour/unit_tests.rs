//! Unit tests for the CLAP host's event translation.

use std::ffi::c_void;
use tutti_units::ParamId;

use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_NOTE_CHOKE, CLAP_EVENT_NOTE_END,
    CLAP_EVENT_NOTE_EXPRESSION, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
    CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE, clap_event_header, clap_event_note, clap_event_note_expression,
    clap_event_param_gesture, clap_event_param_mod, clap_event_param_value,
};
use tutti_clap_host::{
    ClapEvent, ClapHost, EventList, HostState, InputEventList, InputRooms, InputStream,
    NoteExpressionType, OutputEventList, OutputRooms, OutputStream,
};
use tutti_graph::Offset;
use tutti_ump::{ControlValue, Message, MidiChannel, NoteKeyEvent, NoteNumber, U7, Voice2};
use tutti_units::Samples;

/// A block long enough for every offset these tests use.
const BLOCK: usize = 1_000;

/// The offset `k` frames into the block.
fn at(k: usize) -> Offset {
    Offset::of(Samples::new(k), Samples::new(BLOCK))
}

use crate::support;
use support::main_thread;
#[allow(unused_imports)]
use support::raw::{HostRaw, InputListRaw, InputStreamRaw, OutputListRaw, OutputStreamRaw};
use support::{midi_events, note_expressions, param_changes};
use tutti_fixtures::midi;

/// Note events pushed through `InputEventList::add_midi` should appear in
/// the FFI event table.
#[test]
fn test_input_event_list_add_midi_counts_correctly() {
    let mut list = InputEventList::new(InputRooms::new(1024));
    list.add_midi(&(at(0), midi::note_on(MidiChannel::FIRST, 60, 0x8000)));
    list.add_midi(&(at(100), midi::note_off(MidiChannel::FIRST, 60, 0)));

    let raw = list.as_raw();
    unsafe {
        let size_fn = (*raw).size.unwrap();
        assert_eq!(size_fn(raw), 2);

        let get_fn = (*raw).get.unwrap();
        let header0 = &*get_fn(raw, 0);
        assert_eq!(header0.time, 0);

        let header1 = &*get_fn(raw, 1);
        assert_eq!(header1.time, 100);

        // Out of bounds returns null
        assert!(get_fn(raw, 2).is_null());
    }
}

/// A wildcard note-off must not be re-pointed at one phantom voice.
///
/// CLAP types `channel`/`key` as `i16` with `-1` meaning "all channels" / "all
/// keys" (`clap/events.h`). Masking them like ordinary values silently invents a
/// different event: `-1i16 as u8` is `0xFF`, so `& 0x0F` gives channel 15 and
/// `& 0x7F` gives note 127. A plugin panicking every voice off then aims that at
/// a single phantom voice and every real voice keeps sounding — an audible stuck
/// note on the normal instrument output path.
///
/// The event carries a `note_id`, so the address is recoverable and the event
/// must resolve to *that* voice rather than to (15, 127).
#[test]
fn a_wildcard_note_off_resolves_by_note_id_not_by_masking() {
    const CHANNEL: u8 = 2;
    const NOTE: u8 = 60;

    let mut list = OutputEventList::new(OutputRooms::new(256).params(64, &[]));
    let raw = list.as_raw_mut();

    let note = clap_event_note {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_note>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_NOTE_OFF,
            flags: 0,
        },
        // The spec's id: `channel * 128 + note` (`tutti_ump::NoteId`).
        note_id: tutti_ump::NoteId::from_channel_note(
            MidiChannel::of(CHANNEL),
            NoteNumber::of(NOTE),
        )
        .raw() as i32,
        port_index: 0,
        // The wildcard under test.
        channel: -1,
        key: -1,
        velocity: 0.0,
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        assert!(push_fn(raw as *const _, &note.header as *const _));
    }

    let midi_out = midi_events(&mut list);
    assert_eq!(midi_out.len(), 1, "the note-off must survive conversion");
    let event = &midi_out[0];
    assert!(event.ump.is_note_off());
    assert_eq!(
        event.ump.note_key_event().map(|key| match key {
            NoteKeyEvent::On(key) | NoteKeyEvent::Off(key) => key.note().get(),
            NoteKeyEvent::Sustain { .. } => unreachable!("a note-off"),
        }),
        Some(NOTE),
        "a wildcard key must resolve through note_id, not mask to 127 — \
         masking leaves every real voice ringing"
    );
    assert_eq!(
        match event.ump.decode() {
            Ok(Message::Midi1(a, _) | Message::Midi2(a, _)) => Some(a.channel),
            _ => None,
        },
        Some(MidiChannel::of(CHANNEL)),
        "a wildcard channel must resolve through note_id, not mask to 15"
    );
}

/// **Each core event a plugin pushes is held as its own variant, at its time,
/// and reaches only its own sink**: notes and MIDI become UMP messages
/// (`NoteChoke` and `NoteEnd` are notices with no message; a tuning is a
/// per-note pitch message), a value becomes a parameter queue, a tuning also a
/// note expression, and a gesture or modulation neither. Taking the events empties the list.
#[test]
fn each_core_event_a_plugin_pushes_is_held_as_its_own_variant() {
    use clap_sys::events::{CLAP_NOTE_EXPRESSION_TUNING, clap_event_midi};

    let mut list = OutputEventList::new(OutputRooms::new(256).params(64, &[]));
    let raw = list.as_raw_mut();
    let header = |size: usize, time: u32, type_: u16| clap_event_header {
        size: size as u32,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_,
        flags: 0,
    };
    // Each field of each row has a value of its own, so a crosswise read shows.
    let note = |time: u32, type_: u16, note_id: i32, channel: i16, key: i16| clap_event_note {
        header: header(std::mem::size_of::<clap_event_note>(), time, type_),
        note_id,
        port_index: 0,
        channel,
        key,
        velocity: 0.5,
    };
    let midi = clap_event_midi {
        header: header(std::mem::size_of::<clap_event_midi>(), 5, CLAP_EVENT_MIDI),
        port_index: 0,
        data: [0xB2, 74, 100],
    };
    let expression = clap_event_note_expression {
        header: header(
            std::mem::size_of::<clap_event_note_expression>(),
            6,
            CLAP_EVENT_NOTE_EXPRESSION,
        ),
        expression_id: CLAP_NOTE_EXPRESSION_TUNING,
        note_id: 5,
        port_index: 0,
        channel: 0,
        key: 60,
        value: 0.25,
    };
    let value = clap_event_param_value {
        header: header(
            std::mem::size_of::<clap_event_param_value>(),
            7,
            CLAP_EVENT_PARAM_VALUE,
        ),
        param_id: 7,
        cookie: std::ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value: 0.42,
    };
    let modulation = clap_event_param_mod {
        header: header(
            std::mem::size_of::<clap_event_param_mod>(),
            8,
            CLAP_EVENT_PARAM_MOD,
        ),
        param_id: 9,
        cookie: std::ptr::null_mut(),
        note_id: 11,
        port_index: 2,
        channel: 3,
        key: 77,
        amount: 0.25,
    };
    let gesture = |time: u32, type_: u16| clap_event_param_gesture {
        header: header(std::mem::size_of::<clap_event_param_gesture>(), time, type_),
        param_id: 15,
    };
    let (on, off, choke, end) = (
        note(1, CLAP_EVENT_NOTE_ON, 40, 5, 63),
        note(2, CLAP_EVENT_NOTE_OFF, 44, 6, 64),
        note(3, CLAP_EVENT_NOTE_CHOKE, 41, 7, 61),
        note(4, CLAP_EVENT_NOTE_END, 43, 8, 62),
    );
    let (begin, finish) = (
        gesture(9, CLAP_EVENT_PARAM_GESTURE_BEGIN),
        gesture(10, CLAP_EVENT_PARAM_GESTURE_END),
    );
    let headers = [
        &on.header,
        &off.header,
        &choke.header,
        &end.header,
        &midi.header,
        &expression.header,
        &value.header,
        &modulation.header,
        &begin.header,
        &finish.header,
    ];
    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        for header in headers {
            assert!(push_fn(raw as *const _, header as *const _));
        }
    }

    let held: Vec<(&str, u32)> = list
        .events()
        .iter()
        .map(|event| {
            let name = match event {
                ClapEvent::NoteOn(_) => "NoteOn",
                ClapEvent::NoteOff(_) => "NoteOff",
                ClapEvent::NoteChoke(_) => "NoteChoke",
                ClapEvent::NoteEnd(_) => "NoteEnd",
                ClapEvent::Midi(_) => "Midi",
                ClapEvent::NoteExpression(_) => "NoteExpression",
                ClapEvent::ParamValue(_) => "ParamValue",
                ClapEvent::ParamMod(_) => "ParamMod",
                ClapEvent::ParamGestureBegin(_) => "ParamGestureBegin",
                ClapEvent::ParamGestureEnd(_) => "ParamGestureEnd",
                other => panic!("an event the test did not push: {other:?}"),
            };
            (name, event.header().time)
        })
        .collect();
    assert_eq!(
        held,
        [
            ("NoteOn", 1),
            ("NoteOff", 2),
            ("NoteChoke", 3),
            ("NoteEnd", 4),
            ("Midi", 5),
            ("NoteExpression", 6),
            ("ParamValue", 7),
            ("ParamMod", 8),
            ("ParamGestureBegin", 9),
            ("ParamGestureEnd", 10),
        ]
    );

    let events = list.events();
    let fields = |e: &clap_event_note| (e.note_id, e.channel, e.key);
    let (ClapEvent::NoteOn(on), ClapEvent::NoteOff(off)) = (&events[0], &events[1]) else {
        unreachable!("checked above");
    };
    assert_eq!((fields(on), fields(off)), ((40, 5, 63), (44, 6, 64)));
    let (ClapEvent::NoteChoke(choke), ClapEvent::NoteEnd(end)) = (&events[2], &events[3]) else {
        unreachable!("checked above");
    };
    assert_eq!((fields(choke), fields(end)), ((41, 7, 61), (43, 8, 62)));
    let ClapEvent::Midi(cc) = &events[4] else {
        unreachable!("checked above");
    };
    assert_eq!(cc.data, [0xB2, 74, 100]);
    let ClapEvent::ParamMod(m) = &events[7] else {
        unreachable!("checked above");
    };
    assert_eq!(
        (
            m.param_id,
            m.note_id,
            m.port_index,
            m.channel,
            m.key,
            m.amount
        ),
        (9, 11, 2, 3, 77, 0.25)
    );
    let (ClapEvent::ParamGestureBegin(b), ClapEvent::ParamGestureEnd(e)) = (&events[8], &events[9])
    else {
        unreachable!("checked above");
    };
    assert_eq!((b.param_id, e.param_id), (15, 15));

    let changes = param_changes(&list);
    assert_eq!(
        changes.queues.len(),
        1,
        "only the value is a parameter queue"
    );
    assert_eq!(changes.queues[0].param_id, ParamId::new(7u32));
    assert!((changes.queues[0].points[0].value.get() - 0.42).abs() < 0.001);

    let expressions = note_expressions(&list);
    assert_eq!(expressions.len(), 1);
    assert_eq!(expressions[0].expression_type, NoteExpressionType::Tuning);
    assert_eq!(expressions[0].note_id, 5);
    assert!((expressions[0].value - 0.25).abs() < 0.001);

    let messages = midi_events(&mut list);
    let offsets: Vec<u32> = messages.iter().map(|e| e.offset).collect();
    assert_eq!(
        offsets,
        [1, 2, 5, 6],
        "a note on, a note off, the CC and the tuning's per-note pitch"
    );
    let decoded: Vec<Message> = messages
        .iter()
        .map(|e| e.ump.decode().expect("a message"))
        .collect();
    assert!(matches!(
        decoded[0],
        Message::Midi2(a, Voice2::NoteOn { note, .. })
            if (a.channel.get(), note.get()) == (5, 63)
    ));
    assert!(matches!(
        decoded[1],
        Message::Midi2(a, Voice2::NoteOff { note, .. })
            if (a.channel.get(), note.get()) == (6, 64)
    ));
    assert!(matches!(
        decoded[2],
        Message::Midi2(a, Voice2::ControlChange { controller, value })
            if a.channel.get() == 2
                && controller.get() == 74
                && value == ControlValue::from(U7::of(100))
    ));

    assert_eq!(list.take_events().len(), 10);
    assert_eq!(list.len(), 0);
}

// ── Stream I/O ──

#[test]
fn test_output_stream_write_and_read_back() {
    let mut stream = OutputStream::new();
    let raw = stream.as_raw();

    let data = b"CLAP state data here";
    unsafe {
        let write_fn = (*raw).write.unwrap();
        let written = write_fn(raw, data.as_ptr() as *const c_void, data.len() as u64);
        assert_eq!(written, data.len() as i64);
    }

    assert_eq!(stream.data(), data);
}

#[test]
fn test_input_stream_partial_reads() {
    let data = b"ABCDEFGHIJ";
    let mut stream = InputStream::new(data);
    let raw = stream.as_raw();

    let mut buf = [0u8; 4];
    unsafe {
        let read_fn = (*raw).read.unwrap();

        let n = read_fn(raw, buf.as_mut_ptr() as *mut c_void, 4);
        assert_eq!(n, 4);
        assert_eq!(&buf, b"ABCD");

        let n = read_fn(raw, buf.as_mut_ptr() as *mut c_void, 4);
        assert_eq!(n, 4);
        assert_eq!(&buf, b"EFGH");

        let n = read_fn(raw, buf.as_mut_ptr() as *mut c_void, 4);
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], b"IJ");

        assert_eq!(stream.position(), 10);
        assert_eq!(stream.remaining(), 0);

        let n = read_fn(raw, buf.as_mut_ptr() as *mut c_void, 4);
        assert_eq!(n, 0);
    }
}

// ── Host state + extensions ──

/// The main thread is the one thread that proved it with a token, and no
/// other. The answer is `ThreadRoles`' alone (`HostState` keeps no second copy
/// of the thread to drift from it).
#[test]
fn test_host_state_main_thread_is_the_one_that_proved_it() {
    let state = std::sync::Arc::new(HostState::new(main_thread()));
    assert!(state.is_main_thread());
    let elsewhere = {
        let state = std::sync::Arc::clone(&state);
        std::thread::spawn(move || state.is_main_thread())
            .join()
            .unwrap()
    };
    assert!(!elsewhere, "another thread is not the main thread");
}

/// The host does not route a plugin's `request_show` / `request_hide` to its
/// editor window, so it says so: `true` would tell a floating GUI its window
/// state changed when it did not.
#[test]
fn host_gui_show_and_hide_requests_are_declined() {
    use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui};

    let host = ClapHost::new(std::sync::Arc::new(HostState::new(main_thread())));
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let gui_ext = unsafe { &*(get_ext(raw, CLAP_EXT_GUI.as_ptr()) as *const clap_host_gui) };
    assert!(!unsafe { gui_ext.request_show.unwrap()(raw) });
    assert!(!unsafe { gui_ext.request_hide.unwrap()(raw) });
}

#[test]
fn host_gui_closed_was_destroyed_latches_window_destroyed() {
    // gui.closed(was_destroyed = true) records that the plugin's *window* is
    // gone, which is what makes close_editor skip gui.hide. It does not record
    // that the gui object was released — the host still owes it a gui.destroy.
    // With was_destroyed = false the latch stays clear.
    use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui};
    use std::sync::atomic::Ordering;

    let host = ClapHost::new(std::sync::Arc::new(HostState::new(main_thread())));
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let gui_ext = unsafe { get_ext(raw, CLAP_EXT_GUI.as_ptr()) as *const clap_host_gui };
    assert!(!gui_ext.is_null());
    let closed = unsafe { (*gui_ext).closed.unwrap() };

    // was_destroyed = false: closed flag set, but no window_destroyed latch.
    unsafe { closed(raw, false) };
    assert!(host.state().gui.closed.load(Ordering::Acquire));
    assert!(
        !host.state().gui.window_destroyed.load(Ordering::Acquire),
        "was_destroyed=false must not latch window_destroyed"
    );

    // was_destroyed = true: latch set — close_editor will skip hide.
    unsafe { closed(raw, true) };
    assert!(
        host.state().gui.window_destroyed.load(Ordering::Acquire),
        "was_destroyed=true must latch window_destroyed"
    );
}

/// Every extension `clap_host_get_extension` dispatches must hand back a
/// non-null vtable. The list mirrors the `dispatch_extension!` arms in
/// `src/host/mod.rs` one for one: an arm added there without a row here is an
/// extension nothing checks, and a row here without an arm fails immediately.
///
/// A plugin reads a null as "the host does not support this" and takes its
/// degraded path silently, so a dropped arm is invisible at runtime.
/// Spec: clap.host.get_extension
#[test]
fn test_host_get_extension_returns_non_null_for_supported() {
    use clap_sys::ext::ambisonic::CLAP_EXT_AMBISONIC;
    use clap_sys::ext::audio_ports::CLAP_EXT_AUDIO_PORTS;
    use clap_sys::ext::audio_ports_config::CLAP_EXT_AUDIO_PORTS_CONFIG;
    use clap_sys::ext::draft::transport_control::CLAP_EXT_TRANSPORT_CONTROL;
    use clap_sys::ext::event_registry::CLAP_EXT_EVENT_REGISTRY;
    use clap_sys::ext::gui::CLAP_EXT_GUI;
    use clap_sys::ext::latency::CLAP_EXT_LATENCY;
    use clap_sys::ext::log::CLAP_EXT_LOG;
    use clap_sys::ext::note_name::CLAP_EXT_NOTE_NAME;
    use clap_sys::ext::note_ports::CLAP_EXT_NOTE_PORTS;
    use clap_sys::ext::params::CLAP_EXT_PARAMS;
    use clap_sys::ext::preset_load::CLAP_EXT_PRESET_LOAD;
    use clap_sys::ext::state::CLAP_EXT_STATE;
    use clap_sys::ext::surround::CLAP_EXT_SURROUND;
    use clap_sys::ext::tail::CLAP_EXT_TAIL;
    use clap_sys::ext::thread_check::CLAP_EXT_THREAD_CHECK;
    use clap_sys::ext::thread_pool::CLAP_EXT_THREAD_POOL;
    use clap_sys::ext::timer_support::CLAP_EXT_TIMER_SUPPORT;
    use clap_sys::ext::track_info::CLAP_EXT_TRACK_INFO;
    use clap_sys::ext::voice_info::CLAP_EXT_VOICE_INFO;

    let host = ClapHost::new(std::sync::Arc::new(HostState::new(main_thread())));
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };

    let supported = [
        CLAP_EXT_TRACK_INFO,
        CLAP_EXT_TRANSPORT_CONTROL,
        CLAP_EXT_THREAD_CHECK,
        CLAP_EXT_LOG,
        CLAP_EXT_PARAMS,
        CLAP_EXT_STATE,
        CLAP_EXT_LATENCY,
        CLAP_EXT_TAIL,
        CLAP_EXT_GUI,
        CLAP_EXT_AUDIO_PORTS,
        CLAP_EXT_NOTE_PORTS,
        CLAP_EXT_TIMER_SUPPORT,
        CLAP_EXT_NOTE_NAME,
        CLAP_EXT_VOICE_INFO,
        CLAP_EXT_PRESET_LOAD,
        CLAP_EXT_AUDIO_PORTS_CONFIG,
        CLAP_EXT_EVENT_REGISTRY,
        CLAP_EXT_THREAD_POOL,
        CLAP_EXT_AMBISONIC,
        CLAP_EXT_SURROUND,
    ];

    for ext_id in &supported {
        let ptr = unsafe { get_ext(raw, ext_id.as_ptr()) };
        assert!(!ptr.is_null(), "Extension {:?} returned null", ext_id);
    }

    // `posix_fd_support` is the one arm behind a `cfg`, so it is checked
    // behind the same one rather than folded into the table above.
    #[cfg(unix)]
    {
        use clap_sys::ext::posix_fd_support::CLAP_EXT_POSIX_FD_SUPPORT;
        let ptr = unsafe { get_ext(raw, CLAP_EXT_POSIX_FD_SUPPORT.as_ptr()) };
        assert!(
            !ptr.is_null(),
            "Extension {CLAP_EXT_POSIX_FD_SUPPORT:?} returned null"
        );
    }
}

/// The headers keep a `*_COMPAT` id for each draft that became stable ("The
/// latest draft is 100% compatible. This compat ID may be removed in 2026"), so
/// a plugin built against the older header asks the host for that id. The host
/// answers it with the stable extension's own vtable, as it does for the plugin
/// side.
/// Spec: clap.host.get_extension
#[test]
fn test_host_get_extension_answers_the_compat_ids_with_the_stable_vtable() {
    use clap_sys::ext::ambisonic::{CLAP_EXT_AMBISONIC, CLAP_EXT_AMBISONIC_COMPAT};
    use clap_sys::ext::preset_load::{CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT};
    use clap_sys::ext::surround::{CLAP_EXT_SURROUND, CLAP_EXT_SURROUND_COMPAT};

    let host = ClapHost::new(std::sync::Arc::new(HostState::new(main_thread())));
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };

    for (stable, compat) in [
        (CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT),
        (CLAP_EXT_AMBISONIC, CLAP_EXT_AMBISONIC_COMPAT),
        (CLAP_EXT_SURROUND, CLAP_EXT_SURROUND_COMPAT),
    ] {
        let stable_ptr = unsafe { get_ext(raw, stable.as_ptr()) };
        let compat_ptr = unsafe { get_ext(raw, compat.as_ptr()) };
        assert!(!compat_ptr.is_null(), "{compat:?} returned null");
        assert_eq!(compat_ptr, stable_ptr, "{compat:?} is {stable:?}'s vtable");
    }
}

/// An extension the host serves nothing for is not advertised: the plugin
/// then does not call it (`host.h`: `get_extension` returns null for one the
/// host does not implement).
#[test]
fn test_host_does_not_advertise_extensions_it_does_not_serve() {
    let host = ClapHost::new(std::sync::Arc::new(HostState::new(main_thread())));
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };

    for id in [
        c"clap.remote-controls/2",
        c"clap.context-menu/1",
        c"clap.triggers/1",
        c"clap.tuning/2",
        c"clap.resource-directory/1",
        c"clap.undo/4",
    ] {
        let ptr = unsafe { get_ext(raw, id.as_ptr()) };
        assert!(ptr.is_null(), "{id:?} must not be advertised");
    }
}

#[test]
fn test_host_get_extension_returns_null_for_null_id() {
    let host = ClapHost::new(std::sync::Arc::new(HostState::new(main_thread())));
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };

    let ptr = unsafe { get_ext(raw, std::ptr::null()) };
    assert!(ptr.is_null());
}

/// `request_process` raises the bound wake once per call, and a request made
/// before any binding has nobody to wake and changes nothing.
#[test]
fn test_host_request_process_raises_the_bound_wake() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tutti_plugin_types::ProcessWake;

    let state = Arc::new(HostState::new(main_thread()));
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let request_process = unsafe { (*raw).request_process.unwrap() };

    unsafe { request_process(raw) };

    let raised = Arc::new(AtomicU32::new(0));
    let count = raised.clone();
    assert!(
        state
            .process_wake
            .set(ProcessWake::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            }))
            .is_ok()
    );
    assert_eq!(
        raised.load(Ordering::SeqCst),
        0,
        "the early request is gone"
    );

    unsafe { request_process(raw) };
    unsafe { request_process(raw) };
    assert_eq!(raised.load(Ordering::SeqCst), 2, "one raise per request");
}

// ── Miscellaneous ──

#[test]
fn a_flush_list_is_sent_in_time_order() {
    let events = vec![
        ClapEvent::param_value(10, 1, 0.5),
        ClapEvent::param_value(5, 2, 0.3),
    ];

    let mut list = tutti_clap_host::FlushEvents::new(events);
    list.sort_by_time();
    let source: *const dyn tutti_clap_ffi::EventSource = &list;
    // SAFETY: `list` outlives the raw list, which is read here only.
    let raw = unsafe { tutti_clap_ffi::input_raw_for_test(source) };
    unsafe {
        let size_fn = (*raw).size.unwrap();
        assert_eq!(size_fn(raw), 2);

        let get_fn = (*raw).get.unwrap();
        let first = get_fn(raw, 0);
        assert_eq!((*first).time, 5);
        let second = get_fn(raw, 1);
        assert_eq!((*second).time, 10);
    }
}

/// `dialect_to_send` must never answer with a dialect this host does not
/// send. `host_note_ports_supported_dialects` advertises CLAP and MIDI 1.0
/// only, so MIDI 2.0 and MPE are never valid answers however the port is
/// described.
#[test]
fn test_note_port_dialect_to_send() {
    use tutti_clap_host::{NoteDialect, NoteDialects, NotePortInfo, NoteSend};

    let port = |supported: NoteDialects, preferred: Option<NoteDialect>| NotePortInfo {
        id: 0,
        name: String::new(),
        supported_dialects: supported,
        preferred_dialect: preferred,
    };

    // A preference this host can speak is honoured.
    assert_eq!(
        port(
            NoteDialects::CLAP | NoteDialects::MIDI,
            Some(NoteDialect::Midi)
        )
        .dialect_to_send(),
        NoteSend::Midi,
    );

    // No preference stated: fall back to a supported dialect rather than
    // inventing one (for example, reporting Midi2).
    assert_eq!(
        port(NoteDialects::CLAP | NoteDialects::MIDI, None).dialect_to_send(),
        NoteSend::CLAP_AND_MIDI,
    );
    assert_eq!(
        port(NoteDialects::MIDI, None).dialect_to_send(),
        NoteSend::Midi,
    );

    // A preference this host cannot send falls back to what the port also
    // supports, rather than being echoed back at the caller.
    assert_eq!(
        port(
            NoteDialects::MIDI | NoteDialects::MIDI2,
            Some(NoteDialect::Midi2)
        )
        .dialect_to_send(),
        NoteSend::Midi,
    );

    // Nothing in common: the caller must see that, not a fabricated dialect.
    assert_eq!(
        port(NoteDialects::MIDI2, Some(NoteDialect::Midi2)).dialect_to_send(),
        NoteSend::Nothing,
    );
    assert_eq!(
        port(NoteDialects::MIDI_MPE, None).dialect_to_send(),
        NoteSend::Nothing,
    );

    // A preference the port does not list is not sent: a supported dialect is.
    assert_eq!(
        port(NoteDialects::CLAP, Some(NoteDialect::Midi)).dialect_to_send(),
        NoteSend::CLAP_ONLY,
    );
    assert_eq!(
        port(NoteDialects::MIDI, Some(NoteDialect::Clap)).dialect_to_send(),
        NoteSend::Midi,
    );
    assert_eq!(
        port(NoteDialects::MIDI_MPE, Some(NoteDialect::Midi)).dialect_to_send(),
        NoteSend::Nothing,
    );
}

// ── Remaining plan items: voice info, note name, sysex, timer ──

#[test]
fn test_host_timer_register_unregister() {
    use clap_sys::ext::timer_support::{CLAP_EXT_TIMER_SUPPORT, clap_host_timer_support};
    use std::sync::Arc;

    let state = Arc::new(HostState::new(main_thread()));
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ts_ptr = unsafe { get_ext(raw, CLAP_EXT_TIMER_SUPPORT.as_ptr()) };
    let ts = unsafe { &*(ts_ptr as *const clap_host_timer_support) };

    let mut timer_id: u32 = 0;
    let ok = unsafe { ts.register_timer.unwrap()(raw, 100, &mut timer_id) };
    assert!(ok);
    assert!(timer_id > 0);

    // Register a second timer — should get a different ID
    let mut timer_id2: u32 = 0;
    let ok = unsafe { ts.register_timer.unwrap()(raw, 200, &mut timer_id2) };
    assert!(ok);
    assert_ne!(timer_id, timer_id2);

    // Unregister first
    let ok = unsafe { ts.unregister_timer.unwrap()(raw, timer_id) };
    assert!(ok);

    // Unregister second
    let ok = unsafe { ts.unregister_timer.unwrap()(raw, timer_id2) };
    assert!(ok);

    // Unregister again should fail (already removed)
    let ok = unsafe { ts.unregister_timer.unwrap()(raw, timer_id) };
    assert!(!ok);
}

/// The host sends and takes core events only, so it claims no other event
/// space: every name is unknown (`false`, `UINT16_MAX`).
#[test]
fn test_host_event_registry_claims_no_space() {
    use clap_sys::ext::event_registry::{CLAP_EXT_EVENT_REGISTRY, clap_host_event_registry};
    use std::sync::Arc;

    let state = Arc::new(HostState::new(main_thread()));
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let er_ptr = unsafe { get_ext(raw, CLAP_EXT_EVENT_REGISTRY.as_ptr()) };
    let er = unsafe { &*(er_ptr as *const clap_host_event_registry) };

    let mut space_id: u16 = 0;
    let ok = unsafe { er.query.unwrap()(raw, c"com.example.my-events".as_ptr(), &mut space_id) };
    assert!(!ok);
    assert_eq!(space_id, u16::MAX);
}

// ── thread_pool ──

/// `ext/thread-pool.h`: `true` means the host *did execute* all the tasks,
/// and a call from outside the plugin's `process` must be rejected. No
/// process call is in flight here (and no pool), so every request must be
/// rejected — a `true` tells the plugin its tasks completed when nothing ran
/// them. The served case is `tests/behaviour/clap_thread_pool.rs`.
#[test]
fn test_host_thread_pool_request_exec_rejects() {
    use clap_sys::ext::thread_pool::{CLAP_EXT_THREAD_POOL, clap_host_thread_pool};
    use std::sync::Arc;

    let host = ClapHost::new(Arc::new(HostState::new(main_thread())));
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let tp_ptr = unsafe { get_ext(raw, CLAP_EXT_THREAD_POOL.as_ptr()) };
    let tp = unsafe { &*(tp_ptr as *const clap_host_thread_pool) };

    for num_tasks in [0, 1, 16, u32::MAX] {
        assert!(
            !unsafe { tp.request_exec.unwrap()(raw, num_tasks) },
            "request_exec({num_tasks}) must reject: accepting claims {num_tasks} \
             tasks ran, and this host has no pool to run them"
        );
    }
}

// ── ambisonic, surround ──

/// Every id CLAP defines maps to a named position, and anything else is
/// carried rather than discarded.
///
/// 18 is `CLAP_SURROUND_TSL` (`surround.h:55`) — a position CLAP does define —
/// so it must decode to a named position, not `None`.
#[test]
fn test_surround_channel_from_position() {
    use tutti_clap_host::SurroundChannel;
    assert_eq!(
        SurroundChannel::from_position(0),
        SurroundChannel::FrontLeft
    );
    assert_eq!(
        SurroundChannel::from_position(3),
        SurroundChannel::LowFrequency
    );
    assert_eq!(
        SurroundChannel::from_position(17),
        SurroundChannel::TopBackRight
    );
    // The two that were missing.
    assert_eq!(
        SurroundChannel::from_position(18),
        SurroundChannel::TopSideLeft
    );
    assert_eq!(
        SurroundChannel::from_position(19),
        SurroundChannel::TopSideRight
    );
    // Past the last CLAP position: named as unknown, not discarded.
    assert_eq!(
        SurroundChannel::from_position(255),
        SurroundChannel::Unknown(255)
    );
}

/// `position` inverts `from_position` for every id in the byte range.
///
/// The two tables are written out by hand, so nothing but a round trip stops
/// one from drifting — a transposed pair would still compile and still decode
/// "successfully", just to the wrong speaker.
#[test]
fn a_surround_position_round_trips_through_the_enum() {
    use tutti_clap_host::SurroundChannel;
    for raw in 0u8..=255 {
        assert_eq!(
            SurroundChannel::from_position(raw).position(),
            raw,
            "position {raw} did not survive the round trip"
        );
    }
}

// ── POSIX FD support tests (unix only) ──

#[cfg(unix)]
#[test]
fn test_host_posix_fd_register_and_unregister() {
    use clap_sys::ext::posix_fd_support::{
        CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_READ, clap_host_posix_fd_support,
    };
    use std::sync::Arc;

    let state = Arc::new(HostState::new(main_thread()));
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let fd_ptr = unsafe { get_ext(raw, CLAP_EXT_POSIX_FD_SUPPORT.as_ptr()) };
    let fd_ext = unsafe { &*(fd_ptr as *const clap_host_posix_fd_support) };

    // Register a file descriptor
    assert!(unsafe { fd_ext.register_fd.unwrap()(raw, 42, CLAP_POSIX_FD_READ) });

    // Duplicate registration should fail
    assert!(!unsafe { fd_ext.register_fd.unwrap()(raw, 42, CLAP_POSIX_FD_READ) });

    // Verify it was stored
    {
        let fds = state.posix_fds.entries.lock();
        assert_eq!(fds.len(), 1);
        assert_eq!(fds[0].fd, 42);
        assert_eq!(fds[0].flags, CLAP_POSIX_FD_READ);
    }

    // Unregister
    assert!(unsafe { fd_ext.unregister_fd.unwrap()(raw, 42) });

    // Verify removed
    {
        let fds = state.posix_fds.entries.lock();
        assert_eq!(fds.len(), 0);
    }

    // Unregister again should fail
    assert!(!unsafe { fd_ext.unregister_fd.unwrap()(raw, 42) });

    // A negative number (`-1` is "no descriptor") names none: refused, and
    // nothing stored for a later wait to borrow.
    for fd in [-1, i32::MIN] {
        assert!(!unsafe { fd_ext.register_fd.unwrap()(raw, fd, CLAP_POSIX_FD_READ) });
    }
    assert!(state.posix_fds.entries.lock().is_empty());
}

#[cfg(unix)]
#[test]
fn test_host_posix_fd_modify() {
    use clap_sys::ext::posix_fd_support::{
        CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE,
        clap_host_posix_fd_support,
    };
    use std::sync::Arc;

    let state = Arc::new(HostState::new(main_thread()));
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let fd_ptr = unsafe { get_ext(raw, CLAP_EXT_POSIX_FD_SUPPORT.as_ptr()) };
    let fd_ext = unsafe { &*(fd_ptr as *const clap_host_posix_fd_support) };

    // Modify non-existent fd should fail
    assert!(!unsafe { fd_ext.modify_fd.unwrap()(raw, 99, CLAP_POSIX_FD_WRITE) });

    // Register then modify
    assert!(unsafe { fd_ext.register_fd.unwrap()(raw, 99, CLAP_POSIX_FD_READ) });
    assert!(unsafe {
        fd_ext.modify_fd.unwrap()(raw, 99, CLAP_POSIX_FD_READ | CLAP_POSIX_FD_WRITE)
    });

    {
        let fds = state.posix_fds.entries.lock();
        assert_eq!(fds[0].flags, CLAP_POSIX_FD_READ | CLAP_POSIX_FD_WRITE);
    }
}

/// Each CLAP severity is emitted at its level on target `tutti::plugin`, tagged
/// with the plugin's id; one CLAP does not name is a warning that carries the
/// raw value.
#[test]
fn each_log_severity_maps_to_its_tracing_level() {
    use tracing::Level;
    use tutti_clap_ffi::{HostCallbacks, LogSeverity};

    let state = HostState::new(main_thread());
    state.plugin_id.set("org.example.synth".to_owned()).unwrap();
    let severities = [
        (LogSeverity::Debug, Level::DEBUG, None),
        (LogSeverity::Info, Level::INFO, None),
        (LogSeverity::Warning, Level::WARN, None),
        (LogSeverity::Error, Level::ERROR, None),
        (LogSeverity::Fatal, Level::ERROR, None),
        (LogSeverity::HostMisbehaving, Level::ERROR, None),
        (LogSeverity::PluginMisbehaving, Level::ERROR, None),
        (LogSeverity::Unknown(99), Level::WARN, Some(99)),
    ];
    let got = support::logs::plugin_events(|| {
        for (severity, _, _) in severities {
            state.log(severity, "line");
        }
    });
    let want: Vec<_> = severities
        .iter()
        .map(|(_, level, raw)| (*level, *raw))
        .collect();
    let got_pairs: Vec<_> = got.iter().map(|e| (e.level, e.severity)).collect();
    assert_eq!(got_pairs, want);
    assert!(
        got.iter()
            .all(|e| e.plugin.as_deref() == Some("org.example.synth") && e.message == "line")
    );
}
