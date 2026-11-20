use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_platform::Instant;

use super::anchor::Anchor;
use super::bytes::{ByteIn, ByteOut, Wire};
use super::poll;
use super::ports::{
    Listed, PortInfo, Side, distinct_names, endpoint, endpoint_id, endpoints, locate, slot_of,
};
use super::schedule::{Next, Outbox, Schedule};
use super::sender::{Mailbox, run};
use crate::Devices;
use crate::capability::{OsEndpoint, legacy};
use crate::devices::ChangeSink;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{MidiHardwareError, Result};
use tutti_midi_device::{DeviceEvent, EndpointId, Identity, OsDevice, PortDirection, WinMmDevice};
use tutti_ump::{Message, MidiGroup, Sysex7Packet, SysexStatus, Ump};

fn named(name: &str, product: u16) -> WinMmDevice {
    WinMmDevice::Named {
        name: name.to_owned(),
        manufacturer: 0x41,
        product,
    }
}

fn info(name: &str) -> PortInfo {
    PortInfo {
        name: name.to_owned(),
        driver_name: name.to_owned(),
        device: named(name, 7),
    }
}

// ---- ports ----------------------------------------------------------------

#[test]
fn an_id_gives_back_its_side_and_occurrence() {
    for (side, occurrence) in [(Side::Input, 0), (Side::Output, 5), (Side::Input, u32::MAX)] {
        let id = endpoint_id(side, occurrence, &info("Keystep").device);
        let slot = slot_of(id);
        assert_eq!((slot.side, slot.occurrence), (side, occurrence));
        assert_eq!(
            slot.fingerprint,
            super::ports::fingerprint(&info("Keystep").device)
        );
    }
}

#[test]
fn an_id_does_not_find_the_port_that_took_its_position() {
    // The list shifts when a device is unplugged: position 1 is another device.
    let slot = slot_of(endpoint_id(Side::Input, 0, &info("Keystep").device));
    let listed = |names: &[&str]| -> Vec<(u32, WinMmDevice)> {
        names
            .iter()
            .zip(0..)
            .map(|(n, i)| (i, info(n).device))
            .collect()
    };
    assert_eq!(locate(slot, listed(&["Launchpad"])), None);
    let other_product = PortInfo {
        device: named("Keystep", 8),
        ..info("Keystep")
    };
    assert_eq!(locate(slot, [(0, other_product.device)]), None);
}

#[test]
fn removing_the_first_of_three_ports_keeps_the_other_two_handles() {
    let ids = |names: &[&str]| -> Vec<EndpointId> {
        endpoints(Side::Input, names.iter().map(|n| info(n)))
            .into_iter()
            .map(|e| e.id)
            .collect()
    };
    let before = ids(&["A", "B", "C"]);
    let after = ids(&["B", "C"]);
    assert_eq!(after, before[1..], "B and C never left");
}

#[test]
fn ports_that_cannot_be_told_apart_are_numbered_in_list_order() {
    let twins = endpoints(Side::Output, [info("Twin"), info("Twin"), info("Other")]);
    assert_ne!(twins[0].id, twins[1].id);
    assert_eq!(slot_of(twins[0].id).occurrence, 0);
    assert_eq!(slot_of(twins[1].id).occurrence, 1);
    assert_eq!(slot_of(twins[2].id).occurrence, 0);
}

fn names(pairs: &[(&str, &str)]) -> Vec<String> {
    let listed: Vec<Listed<'_>> = pairs
        .iter()
        .map(|&(shown, driver_name)| Listed { shown, driver_name })
        .collect();
    distinct_names(&listed)
}

#[test]
fn ports_that_share_a_friendly_name_take_their_distinct_driver_names() {
    assert_eq!(
        names(&[
            ("Pad (USB)", "Pad"),
            ("Pad (USB)", "MIDIIN2 (Pad)"),
            ("Pad (USB)", "MIDIIN3 (Pad)"),
        ]),
        ["Pad", "MIDIIN2 (Pad)", "MIDIIN3 (Pad)"]
    );
}

#[test]
fn ports_that_share_a_name_and_a_driver_name_get_numbers_in_list_order() {
    assert_eq!(
        names(&[("Twin", "Twin"), ("Twin", "Twin"), ("Twin", "Twin")]),
        ["Twin", "Twin 2", "Twin 3"]
    );
}

#[test]
fn only_the_ports_that_still_clash_are_numbered() {
    // X twice clashes; Y is alone in the group.
    assert_eq!(
        names(&[("G", "X"), ("G", "X"), ("G", "Y")]),
        ["G", "G 2", "Y"]
    );
}

#[test]
fn a_port_with_a_name_of_its_own_keeps_it_whatever_its_neighbours_are() {
    assert_eq!(
        names(&[("Solo (USB)", "Solo"), ("Twin", "Twin"), ("Twin", "Twin")]),
        ["Solo (USB)", "Twin", "Twin 2"]
    );
    assert_eq!(names(&[("Solo (USB)", "Solo")]), ["Solo (USB)"]);
}

#[test]
fn a_unique_name_is_kept_before_or_after_a_group_that_would_take_it() {
    let group = [("USB MIDI", "USB MIDI"), ("USB MIDI", "X")];
    let unique = ("X", "X");
    assert_eq!(
        names(&[group[0], group[1], unique]),
        ["USB MIDI", "USB MIDI 2", "X"]
    );
    assert_eq!(
        names(&[unique, group[0], group[1]]),
        ["X", "USB MIDI", "USB MIDI 2"]
    );
}

#[test]
fn a_number_never_lands_on_a_name_another_port_has() {
    for order in [[0, 1, 2], [2, 0, 1], [1, 2, 0]] {
        let all = [("A", "A"), ("A", "A"), ("A 2", "A 2")];
        let listed: Vec<_> = order.iter().map(|&i| all[i]).collect();
        let got = names(&listed);
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "{got:?}");
        let at = order.iter().position(|&i| i == 2).unwrap();
        assert_eq!(got[at], "A 2", "the unique name is kept: {got:?}");
    }
}

#[test]
fn two_groups_never_take_the_same_driver_name() {
    let all = [("G1", "P"), ("G1", "Q"), ("G2", "P"), ("G2", "R")];
    for turn in 0..all.len() {
        let mut rotated = all;
        rotated.rotate_left(turn);
        let got = names(&rotated);
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "{got:?}");
    }
}

#[test]
fn a_blank_driver_name_is_absent_and_the_shown_name_is_numbered() {
    assert_eq!(
        names(&[("Pad", ""), ("Pad", "  "), ("Pad", "\t")]),
        ["Pad", "Pad 2", "Pad 3"]
    );
    assert_eq!(names(&[("Pad", ""), ("Pad", "Pad B")]), ["Pad", "Pad B"]);
}

#[test]
fn naming_ports_apart_leaves_every_id_as_it_was() {
    let twins = |driver_name: &str| PortInfo {
        driver_name: driver_name.to_owned(),
        ..info("Twin")
    };
    let apart = endpoints(Side::Input, [twins("A"), twins("B")]);
    let same = endpoints(Side::Input, [info("Twin"), info("Twin")]);
    assert_ne!(apart[0].name, apart[1].name);
    assert_eq!(
        apart.iter().map(|e| e.id).collect::<Vec<_>>(),
        same.iter().map(|e| e.id).collect::<Vec<_>>()
    );
    assert_eq!(same[1].name, "Twin 2");
}

#[test]
fn an_interface_path_names_the_device_whatever_its_position_or_name() {
    let path = |name: &str| PortInfo {
        name: name.to_owned(),
        driver_name: name.to_owned(),
        device: WinMmDevice::Interface("\\\\?\\usb#vid_0001".to_owned()),
    };
    let slot = slot_of(endpoint_id(Side::Input, 0, &path("Keystep").device));
    // The same device, listed elsewhere with a different cut of its name.
    assert_eq!(
        locate(slot, [(3, path("Keystep Pro MIDI").device)]),
        Some(3)
    );
    let other = PortInfo {
        device: WinMmDevice::Interface("\\\\?\\usb#vid_0002".to_owned()),
        ..path("Keystep")
    };
    assert_eq!(locate(slot, [(0, other.device)]), None);
}

#[test]
fn a_device_that_moved_in_the_list_is_found_where_it_went_and_one_gone_is_not() {
    let slot = slot_of(endpoint_id(Side::Input, 0, &info("B").device));
    let list = |names: &[&str]| -> Vec<(u32, WinMmDevice)> {
        names
            .iter()
            .zip(0..)
            .map(|(n, i)| (i, info(n).device))
            .collect()
    };
    assert_eq!(locate(slot, list(&["A", "C", "B"])), Some(2));
    assert_eq!(
        locate(slot, list(&["B", "C"])),
        Some(0),
        "an earlier device left"
    );
    assert_eq!(locate(slot, list(&["A", "C"])), None, "unplugged");
}

#[test]
fn the_input_and_the_output_of_one_unit_have_different_ids_and_identities() {
    let input = endpoint(Side::Input, 0, info("USB MIDI"));
    let output = endpoint(Side::Output, 0, info("USB MIDI"));
    assert_ne!(input.id, output.id);
    assert_ne!(input.identity, output.identity);
    assert_eq!(input.identity, Identity::winmm_input(named("USB MIDI", 7)));
    assert_eq!(
        output.identity,
        Identity::winmm_output(named("USB MIDI", 7))
    );
}

#[test]
fn a_byte_port_is_listed_as_midi1_with_no_blocks_in_list_order() {
    let listed = endpoints(Side::Output, vec![info("A"), info("B")]);
    assert_eq!(
        listed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        ["A", "B"]
    );
    assert_ne!(listed[0].id, listed[1].id);
    assert!(listed.iter().all(|e| e.capability == legacy()));
}

// ---- bytes in -------------------------------------------------------------

#[test]
fn a_sysex_split_across_two_buffers_is_one_sysex7_packet() {
    let mut input = ByteIn::new();
    assert_eq!(input.long(&[0xF0, 0x7E, 0x7F]).count(), 0, "not whole yet");
    let packets: Vec<Ump> = input.long(&[0x06, 0x01, 0xF7]).collect();
    // Complete (status 0), four data bytes (M2-104 7.7).
    assert_eq!(packets, [Ump([0x3004_7E7F, 0x0601_0000, 0, 0])]);
}

// ---- bytes out ------------------------------------------------------------

fn sent(out: &mut ByteOut, ump: Ump) -> Vec<Vec<u8>> {
    let mut wire: Vec<Vec<u8>> = Vec::new();
    out.encode::<()>(ump, |w| {
        wire.push(match w {
            Wire::Short(ump) => ump.words().iter().flat_map(|w| w.to_be_bytes()).collect(),
            Wire::Long(message) => message.bytes().to_vec(),
        });
        Ok(())
    })
    .unwrap();
    wire
}

fn sysex(status: SysexStatus, data: &[u8]) -> Ump {
    Ump::from(Message::Sysex7(
        MidiGroup::FIRST,
        Sysex7Packet::new(status, data).unwrap(),
    ))
}

#[test]
fn a_midi1_note_on_is_one_short_message() {
    let note_on = Ump([0x2090_3C64, 0, 0, 0]);
    assert_eq!(
        sent(&mut ByteOut::new(), note_on),
        [vec![0x20, 0x90, 0x3C, 0x64]]
    );
}

#[test]
fn a_midi1_program_change_is_one_short_message() {
    let program = Ump([0x20C3_0A00, 0, 0, 0]);
    assert_eq!(
        sent(&mut ByteOut::new(), program),
        [vec![0x20, 0xC3, 0x0A, 0]]
    );
}

/// M2-104 Appendix D.3: a MIDI 2.0 velocity downscales to its top 7 bits.
#[test]
fn a_midi2_note_on_is_one_midi1_note_on_with_the_top_seven_velocity_bits() {
    // Type 4, status 9 channel 0, note 0x3C; velocity 0xC000 (top 7 bits 0x60).
    let note_on = Ump([0x4090_3C00, 0xC000_0000, 0, 0]);
    assert_eq!(
        sent(&mut ByteOut::new(), note_on),
        [vec![0x20, 0x90, 0x3C, 0x60]]
    );
}

/// A Registered Controller is CC 101, 100, 6, 38 (M2-104 Appendix D).
#[test]
fn a_midi2_pitch_bend_range_is_four_control_changes() {
    // Type 4, status 2 (Registered), RPN 0/0, value 12 << 25.
    let rpn = Ump([0x4020_0000, 12 << 25, 0, 0]);
    assert_eq!(
        sent(&mut ByteOut::new(), rpn),
        [
            vec![0x20, 0xB0, 101, 0],
            vec![0x20, 0xB0, 100, 0],
            vec![0x20, 0xB0, 6, 12],
            vec![0x20, 0xB0, 38, 0],
        ]
    );
}

#[test]
fn sysex7_packets_are_sent_as_one_framed_message_at_the_end_packet() {
    let mut out = ByteOut::new();
    assert!(sent(&mut out, sysex(SysexStatus::Start, &[1, 2, 3, 4, 5, 6])).is_empty());
    assert!(
        sent(
            &mut out,
            sysex(SysexStatus::Continue, &[7, 8, 9, 10, 11, 12])
        )
        .is_empty()
    );
    let message = sent(&mut out, sysex(SysexStatus::End, &[13]));
    let mut want = vec![0xF0];
    want.extend(1..=13);
    want.push(0xF7);
    assert_eq!(message, [want]);
}

#[test]
fn a_complete_sysex7_packet_is_a_whole_message() {
    let message = sent(
        &mut ByteOut::new(),
        sysex(SysexStatus::Complete, &[0x7E, 0x7F]),
    );
    assert_eq!(message, [vec![0xF0, 0x7E, 0x7F, 0xF7]]);
}

#[test]
fn a_message_with_no_midi1_form_sends_nothing() {
    // Utility: a NOOP (type 0).
    assert!(sent(&mut ByteOut::new(), Ump::default()).is_empty());
}

#[test]
fn a_failed_send_stops_the_message_and_is_returned() {
    let mut out = ByteOut::new();
    let failed = out.encode(Ump([0x2090_3C64, 0, 0, 0]), |_| Err("no"));
    assert_eq!(failed, Err("no"));
}

// ---- schedule -------------------------------------------------------------

#[test]
fn a_schedule_releases_the_earliest_first_and_ties_in_push_order() {
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);
    let mut held = Schedule::new();
    held.push_batch(8, [(at(30), "late")]);
    held.push_batch(8, [(at(10), "first")]);
    held.push_batch(8, [(at(10), "second")]);
    held.push_batch(8, [(at(20), "middle")]);
    let now = at(25);
    let due: Vec<_> = std::iter::from_fn(|| held.pop_due(now)).collect();
    assert_eq!(due, ["first", "second", "middle"]);
    assert_eq!(held.pop_due(now), None, "the last is still to wait");
}

#[test]
fn a_batch_event_stamped_before_the_one_ahead_of_it_goes_after_it() {
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);
    let mut held = Schedule::new();
    // In batch order: a note at +50, then one "now" (+0), then one at +10.
    let taken = held.push_batch(8, [(at(50), "a"), (at(0), "b"), (at(10), "c")]);
    assert_eq!(taken, 3);
    let due: Vec<_> = std::iter::from_fn(|| held.pop_due(at(100))).collect();
    assert_eq!(due, ["a", "b", "c"]);
}

#[test]
fn a_batch_is_merged_among_held_items_by_time_with_held_first_on_a_tie() {
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);
    let mut held = Schedule::new();
    held.push_batch(8, [(at(10), "h10")]);
    held.push_batch(8, [(at(30), "h30")]);
    held.push_batch(8, [(at(10), "b10"), (at(20), "b20"), (at(40), "b40")]);
    let all: Vec<_> = std::iter::from_fn(|| held.pop_due(at(100))).collect();
    assert_eq!(all, ["h10", "b10", "b20", "h30", "b40"]);
}

#[test]
fn a_batch_no_earlier_than_the_last_held_item_is_appended_in_its_own_order() {
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);
    let mut held = Schedule::new();
    held.push_batch(8, [(at(10), "h10"), (at(30), "h30")]);
    held.push_batch(
        8,
        [
            (at(30), "b30"),
            (at(20), "b20 clamped to 30"),
            (at(50), "b50"),
        ],
    );
    let all: Vec<_> = std::iter::from_fn(|| held.pop_due(at(100))).collect();
    assert_eq!(all, ["h10", "h30", "b30", "b20 clamped to 30", "b50"]);
}

#[test]
fn a_batch_past_the_room_takes_a_prefix() {
    let t0 = Instant::now();
    let mut held = Schedule::new();
    assert_eq!(held.push_batch(2, [(t0, 1), (t0, 2), (t0, 3)]), 2);
    assert_eq!(std::iter::from_fn(|| held.pop_due(t0)).count(), 2);
}

#[test]
fn a_schedule_says_how_long_until_the_earliest_and_zero_when_late() {
    let t0 = Instant::now();
    let mut held = Schedule::new();
    assert_eq!(held.wait(t0), None);
    held.push_batch(8, [(t0 + Duration::from_millis(40), ())]);
    assert_eq!(
        held.wait(t0 + Duration::from_millis(15)),
        Some(Duration::from_millis(25))
    );
    assert_eq!(
        held.wait(t0 + Duration::from_millis(90)),
        Some(Duration::ZERO)
    );
}

#[test]
fn an_outbox_counts_what_is_taken_until_each_is_handled() {
    let t0 = Instant::now();
    let mut outbox = Outbox::new();
    assert_eq!(
        outbox.take(8, [(t0, 1), (t0, 2), (t0 + Duration::from_secs(60), 3)]),
        3
    );
    assert_eq!(outbox.pending(), 3);
    assert_eq!(outbox.next(t0), Next::Send(1));
    assert_eq!(outbox.pending(), 3, "in hand is still pending");
    outbox.done();
    assert_eq!(outbox.next(t0), Next::Send(2));
    outbox.done();
    assert_eq!(outbox.pending(), 1);
    assert_eq!(outbox.next(t0), Next::Wait(Some(Duration::from_secs(60))));
}

#[test]
fn a_dead_outbox_takes_nothing_and_drops_what_it_held() {
    let t0 = Instant::now();
    let mut outbox = Outbox::new();
    assert_eq!(
        outbox.take(
            8,
            [
                (t0 + Duration::from_secs(1), 1),
                (t0 + Duration::from_secs(2), 2)
            ]
        ),
        2
    );
    assert_eq!(outbox.end(), 2, "both were held");
    assert!(!outbox.alive());
    assert_eq!(
        outbox.pending(),
        0,
        "nothing is left waiting for a sender that is gone"
    );
    assert_eq!(outbox.take(8, [(t0, 3)]), 0);
    assert_eq!(outbox.pending(), 0);
}

#[test]
fn ending_an_outbox_returns_the_held_events_and_the_one_in_hand() {
    let t0 = Instant::now();
    let mut outbox = Outbox::new();
    outbox.take(8, [(t0, 1), (t0 + Duration::from_secs(9), 2)]);
    assert_eq!(outbox.next(t0), Next::Send(1), "one is now in hand");
    assert_eq!(outbox.end(), 2);
}

#[test]
fn a_sender_that_panics_leaves_a_dead_mailbox_that_counts_what_it_dropped() {
    let (mailbox, writer) = Mailbox::new();
    let now = Instant::now();
    assert_eq!(mailbox.state().take(8, [(now, 1), (now, 2), (now, 3)]), 3);
    let sent = std::cell::RefCell::new(Vec::new());
    let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mailbox, writer, || {
            (
                |event| -> std::result::Result<(), String> {
                    assert!(event != 2, "the sender fails on the second");
                    sent.borrow_mut().push(event);
                    Ok(())
                },
                |_| -> std::result::Result<(), String> { unreachable!("events are due") },
            )
        });
    }));
    assert!(ended.is_err());
    assert_eq!(*sent.borrow(), [1]);
    let state = mailbox.state();
    assert!(!state.alive());
    assert_eq!(state.pending(), 0);
    drop(state);
    // The second was in hand and the third held: both are refused.
    assert_eq!(mailbox.losses(), tutti_graph::Losses::refused(2));
    assert_eq!(mailbox.state().take(8, [(now, 4)]), 0);
}

#[test]
fn a_sender_whose_setup_panics_is_a_dead_mailbox_too() {
    let (mailbox, writer) = Mailbox::new();
    mailbox.state().take(8, [(Instant::now(), 1)]);
    let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mailbox, writer, || -> (
            fn(i32) -> std::result::Result<(), String>,
            fn(Option<Duration>) -> std::result::Result<(), String>,
        ) {
            panic!("the setup fails")
        });
    }));
    assert!(ended.is_err());
    assert!(!mailbox.state().alive());
    assert_eq!(mailbox.losses(), tutti_graph::Losses::refused(1));
}

#[test]
fn a_send_the_device_refuses_is_lost_and_the_sender_goes_on() {
    let (mailbox, writer) = Mailbox::new();
    let now = Instant::now();
    mailbox.state().take(8, [(now, 1), (now, 2)]);
    let mut waits = 0;
    run(&mailbox, writer, || {
        (
            |event| if event == 1 { Err("refused") } else { Ok(()) },
            |_| -> std::result::Result<(), &str> {
                waits += 1;
                mailbox.state().stop();
                Ok(())
            },
        )
    });
    assert_eq!(waits, 1);
    assert_eq!(mailbox.losses(), tutti_graph::Losses::lost(1));
    assert_eq!(mailbox.state().pending(), 0);
}

#[test]
fn an_outbox_takes_no_more_than_its_room_counting_what_is_pending() {
    let t0 = Instant::now();
    let mut outbox = Outbox::new();
    assert_eq!(outbox.take(3, [(t0, 1), (t0, 2)]), 2);
    assert_eq!(outbox.take(3, [(t0, 3), (t0, 4)]), 1);
}

#[test]
fn a_stopped_outbox_tells_the_sender_to_stop_before_anything_due() {
    let t0 = Instant::now();
    let mut outbox = Outbox::new();
    outbox.take(8, [(t0, 1)]);
    outbox.stop();
    assert_eq!(outbox.next(t0), Next::Stop);
}

// ---- anchor ---------------------------------------------------------------

#[test]
fn a_stamp_is_that_long_after_the_start() {
    let start = Instant::now();
    let mut anchor = Anchor::new(start);
    let at = anchor.arrival(
        Duration::from_millis(250),
        start + Duration::from_millis(260),
    );
    assert_eq!(at, start + Duration::from_millis(250));
}

#[test]
fn a_stamp_seen_before_it_is_due_arrives_when_it_was_seen_and_moves_the_start() {
    let start = Instant::now();
    let mut anchor = Anchor::new(start);
    let now = start + Duration::from_millis(100);
    assert_eq!(anchor.arrival(Duration::from_millis(500), now), now);
    // The next event, 100 ms after that one by the stamps, is placed from the
    // moved start: seen 100 ms later, it arrived when it was seen.
    let later = now + Duration::from_millis(100);
    assert_eq!(anchor.arrival(Duration::from_millis(600), later), later);
}

#[test]
fn a_stamp_more_than_a_second_old_re_anchors_to_now() {
    let start = Instant::now();
    let mut anchor = Anchor::new(start);
    let now = start + Duration::from_secs(5);
    assert_eq!(anchor.arrival(Duration::from_millis(100), now), now);
    let next = now + Duration::from_millis(40);
    assert_eq!(anchor.arrival(Duration::from_millis(140), next), next);
}

// ---- the poll watch -------------------------------------------------------

static LISTED: Mutex<Vec<OsDevice>> = Mutex::new(Vec::new());

fn device(n: u64, name: &str) -> OsDevice {
    OsDevice {
        handle: EndpointId::from_raw(n),
        identity: Identity::winmm_input(named(name, 1)),
        name: name.to_owned(),
        direction: PortDirection::Input,
    }
}

static LOOKS: AtomicUsize = AtomicUsize::new(0);

fn listed() -> Vec<OsDevice> {
    LOOKS.fetch_add(1, Ordering::SeqCst);
    LISTED.lock().unwrap().clone()
}

struct Polled(tutti_platform::Executor);

impl MidiEndpoints for Polled {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        Ok(Box::new(poll::start(
            &self.0,
            sink,
            listed,
            Duration::from_millis(5),
        )?))
    }
    fn inputs(&self) -> Vec<OsEndpoint> {
        Vec::new()
    }
    fn outputs(&self) -> Vec<OsEndpoint> {
        Vec::new()
    }
    fn open_input(
        &self,
        _: EndpointId,
        _: crate::InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        Err(MidiHardwareError::Unsupported("test"))
    }
    fn open_output(&self, _: EndpointId) -> Result<Box<dyn DeviceOut>> {
        Err(MidiHardwareError::Unsupported("test"))
    }
}

/// One test owns `LISTED`, so the process-wide list is not raced.
#[test]
fn the_poll_seeds_then_reports_a_device_that_appears_and_one_that_goes() {
    *LISTED.lock().unwrap() = vec![device(1, "A")];
    let executor = tutti_platform::Executor::new();
    let threads = executor.spawn_threads(std::num::NonZeroUsize::MIN).unwrap();
    let devices = Devices::open_over(&executor, Box::new(Polled(executor.clone()))).unwrap();
    assert_eq!(devices.list().len(), 1, "seeded before open returns");
    let events = devices.subscribe();

    LISTED.lock().unwrap().push(device(2, "B"));
    let Ok(DeviceEvent::Added(added)) = events.recv_timeout(Duration::from_secs(30)) else {
        panic!("B was not reported");
    };
    assert_eq!(added.name, "B");

    LISTED.lock().unwrap().remove(0);
    let Ok(DeviceEvent::Removed(removed)) = events.recv_timeout(Duration::from_secs(30)) else {
        panic!("A was not reported gone");
    };
    assert_eq!(removed.name, "A");

    // Once the last holder drops, the task is cancelled: with the drivers
    // stopped (so no look is under way) and started again, no look follows. The
    // window gives a surviving task, looking every 5 ms, the chance to show; a
    // longer one only makes the check stricter.
    drop(events);
    drop(devices);
    drop(threads);
    let looks = LOOKS.load(Ordering::SeqCst);
    let _threads = executor.spawn_threads(std::num::NonZeroUsize::MIN).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(LOOKS.load(Ordering::SeqCst), looks);
}

/// A backend that hands the test its `ChangeSink` and watches nothing.
struct Captured(Arc<Mutex<Option<ChangeSink>>>);

impl MidiEndpoints for Captured {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(Box::new(crate::endpoints::NoWatch))
    }
    fn inputs(&self) -> Vec<OsEndpoint> {
        Vec::new()
    }
    fn outputs(&self) -> Vec<OsEndpoint> {
        Vec::new()
    }
    fn open_input(
        &self,
        _: EndpointId,
        _: crate::InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        Err(MidiHardwareError::Unsupported("test"))
    }
    fn open_output(&self, _: EndpointId) -> Result<Box<dyn DeviceOut>> {
        Err(MidiHardwareError::Unsupported("test"))
    }
}

/// A poll whose executor has shut down ends without a look: a `Timer`'s sleep
/// fails from then on, and a poll that looked again would spin. The future is polled once by hand on a thread, under a hang
/// guard: one that loops never returns.
#[test]
fn a_poll_ends_when_its_executor_has_shut_down() {
    static SHUT_LOOKS: AtomicUsize = AtomicUsize::new(0);
    fn counted() -> Vec<OsDevice> {
        SHUT_LOOKS.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }
    let slot = Arc::new(Mutex::new(None));
    let executor = tutti_platform::Executor::new();
    let _devices = Devices::open_over(&executor, Box::new(Captured(Arc::clone(&slot)))).unwrap();
    let sink = slot.lock().unwrap().take().unwrap();
    let timer = tutti_platform::Executor::new().timer();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut ended = std::pin::pin!(poll::run(
            timer,
            crate::devices::reporter::Reporter::new(sink.clone()),
            sink,
            counted,
            Duration::from_secs(3600),
        ));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let _ = done.send(ended.as_mut().poll(&mut cx).is_ready());
    });
    assert_eq!(
        finished.recv_timeout(Duration::from_secs(30)),
        Ok(true),
        "the poll ends with its executor"
    );
    assert_eq!(SHUT_LOOKS.load(Ordering::SeqCst), 0, "and does not look");
}
