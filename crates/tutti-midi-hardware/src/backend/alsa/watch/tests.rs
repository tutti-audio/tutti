use super::*;

/// Errors only end a drain when they come in a row.
#[test]
fn scattered_errors_do_not_end_a_drain_but_a_run_of_them_does() {
    let mut errors = ErrorRun::default();
    for _ in 0..100 {
        assert!(!errors.failed());
        errors.read();
    }
    for _ in 0..MAX_DRAIN_ERRORS - 1 {
        assert!(!errors.failed());
    }
    assert!(errors.failed(), "the {MAX_DRAIN_ERRORS}th in a row ends it");
}

use crate::devices::reporter::os_devices;
use crate::test_support::endpoint;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tutti_alsa_ffi::{Address, ClientId};
use tutti_midi_device::DeviceEvent;

fn heard(events: &[Announced]) -> Drained {
    Drained {
        events: events.to_vec(),
        any: true,
    }
}

const NOTHING: Drained = Drained {
    events: Vec::new(),
    any: false,
};

/// The kernel's announcements when client 5 is replaced: the old owner's port
/// and client exit, then the new owner's start and port.
const REPLUG: [Announced; 4] = [
    Announced::PortExit(Address::of(5, 0)),
    Announced::ClientExit(ClientId::of(5)),
    Announced::ClientStart(ClientId::of(5)),
    Announced::PortStart(Address::of(5, 0)),
];

/// A client 5 starting with one port.
const START: [Announced; 2] = [
    Announced::ClientStart(ClientId::of(5)),
    Announced::PortStart(Address::of(5, 0)),
];

/// A read after a look that finds events looks again, and the last look is the
/// one reported.
#[test]
fn a_read_that_finds_events_looks_again() {
    let looks = Cell::new(0);
    let reads = RefCell::new(VecDeque::from([heard(&[]), NOTHING]));
    let settled = settle(
        NOTHING,
        || reads.borrow_mut().pop_front().unwrap_or(NOTHING),
        || {
            looks.set(looks.get() + 1);
            Some(looks.get())
        },
    );
    assert_eq!(
        (settled.seen, looks.get(), settled.quiet),
        (Some(2), 2, true)
    );
}

#[test]
fn events_read_after_a_look_are_kept() {
    let reads = RefCell::new(VecDeque::from([
        heard(&[Announced::PortStart(Address::of(7, 0))]),
        NOTHING,
    ]));
    let settled = settle(
        heard(&[Announced::ClientStart(ClientId::of(6))]),
        || reads.borrow_mut().pop_front().unwrap_or(NOTHING),
        || Some(()),
    );
    assert_eq!(
        settled.events,
        [
            Announced::ClientStart(ClientId::of(6)),
            Announced::PortStart(Address::of(7, 0))
        ]
    );
}

#[test]
fn a_sequencer_that_never_quiets_ends_the_settle() {
    let looks = Cell::new(0);
    let settled = settle(
        NOTHING,
        || heard(&[]),
        || {
            looks.set(looks.get() + 1);
            Some(looks.get())
        },
    );
    assert_eq!((settled.seen, settled.quiet), (Some(MAX_SETTLE + 1), false));
}

/// A look that fails is not a look: the read is not taken for it, and the next
/// try is.
#[test]
fn a_look_that_fails_is_retried_and_is_not_a_look() {
    let failures = Cell::new(2);
    let reads = Cell::new(0);
    let settled = settle(
        heard(&[]),
        || {
            reads.set(reads.get() + 1);
            NOTHING
        },
        || {
            if failures.get() > 0 {
                failures.set(failures.get() - 1);
                None
            } else {
                Some(())
            }
        },
    );
    assert!(settled.seen.is_some() && settled.quiet);
    assert_eq!(reads.get(), 1, "one read, after the look that worked");
}

#[test]
fn a_look_that_never_succeeds_gives_none() {
    let settled: Settled<()> = settle(NOTHING, || NOTHING, || None);
    assert!(settled.seen.is_none() && !settled.quiet);
}

/// A failed read is not a quiet one.
#[test]
fn a_failed_read_is_not_quiet() {
    let mut found = Drained::default();
    found.failed();
    assert!(found.any);
    let looks = Cell::new(0);
    let reads = RefCell::new(VecDeque::from([{
        let mut d = Drained::default();
        d.failed();
        d
    }]));
    settle(
        NOTHING,
        || reads.borrow_mut().pop_front().unwrap_or(NOTHING),
        || {
            looks.set(looks.get() + 1);
            Some(())
        },
    );
    assert_eq!(looks.get(), 2);
}

/// A backend that only hands out the sink its watch is given.
struct Capture(Arc<Mutex<Option<ChangeSink>>>);

impl crate::MidiEndpoints for Capture {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(Box::new(crate::endpoints::NoWatch))
    }
    fn inputs(&self) -> Result<Vec<crate::OsEndpoint>> {
        Ok(Vec::new())
    }
    fn outputs(&self) -> Result<Vec<crate::OsEndpoint>> {
        Ok(Vec::new())
    }
    fn open_input(
        &self,
        _started: crate::Started,
        _: EndpointId,
        _: crate::InputProducerHandle,
    ) -> Result<Box<dyn crate::InputConnection>> {
        Err(MidiHardwareError::Unsupported("none"))
    }
    fn open_output(
        &self,
        _started: crate::Started,
        _: EndpointId,
    ) -> Result<Box<dyn crate::DeviceOut>> {
        Err(MidiHardwareError::Unsupported("none"))
    }
}

/// The sequencer: one ordered announce port, and the clients.
struct Model {
    stream: RefCell<Vec<Announced>>,
    present: RefCell<Vec<OsDevice>>,
    /// Looks that fail, as an enumeration cut short by a query error does.
    failing: Cell<usize>,
    /// Whether every read finds something, as a sequencer that never quiets.
    storm: Cell<bool>,
}

impl Model {
    /// A read of the announce port.
    fn drain(&self) -> Drained {
        let events = self.stream.take();
        Drained {
            any: !events.is_empty() || self.storm.get(),
            events,
        }
    }

    fn look(&self) -> Option<Vec<OsDevice>> {
        if self.failing.get() > 0 {
            self.failing.set(self.failing.get() - 1);
            return None;
        }
        Some(self.present.borrow().clone())
    }
}

/// A `Tracker` over a `Devices` whose events the test reads, and the model.
struct World {
    devices: crate::Devices,
    events: crate::DeviceEvents,
    tracker: Tracker,
    model: Model,
}

impl World {
    /// Client 5 is the device `name`, and `queued` is in the announce port
    /// before the first look.
    fn with(name: &str, queued: &[Announced]) -> Self {
        Self::build(name, queued, 0)
    }

    /// As [`with`](Self::with), the first `failing` looks of which fail.
    fn build(name: &str, queued: &[Announced], failing: usize) -> Self {
        let slot = Arc::new(Mutex::new(None));
        let devices = crate::Devices::open_over(
            &tutti_platform::Executor::new(),
            Box::new(Capture(Arc::clone(&slot))),
        )
        .unwrap();
        let events = devices.subscribe();
        let sink = slot.lock().unwrap().take().unwrap();
        let model = Model {
            stream: RefCell::new(queued.to_vec()),
            present: RefCell::new(Self::device(name)),
            failing: Cell::new(failing),
            storm: Cell::new(false),
        };
        let mut tracker = Tracker::new(Reporter::new(sink));
        tracker.seed(|| model.drain(), || model.look()).unwrap();
        Self {
            devices,
            events,
            tracker,
            model,
        }
    }

    /// The device `name` on client 5.
    fn device(name: &str) -> Vec<OsDevice> {
        Self::device_on(5, name)
    }

    /// The device `name` on `client`.
    fn device_on(client: u64, name: &str) -> Vec<OsDevice> {
        os_devices(&[endpoint(client << 32, name)], &[])
    }

    /// One wake. `first` is what the wake read; `race` runs inside the first
    /// look, as clients starting and exiting while the watch enumerates.
    fn wake(&mut self, first: Drained, race: impl FnOnce(&Model)) {
        let race = RefCell::new(Some(race));
        let model = &self.model;
        let look = || {
            if let Some(race) = race.borrow_mut().take() {
                race(model);
            }
            model.look()
        };
        self.tracker.step(first, || model.drain(), look);
    }

    /// A wake for want of an announcement, as the retry timer makes.
    fn timeout_wake(&mut self) {
        self.wake(Drained::default(), |_| {});
    }

    /// A wake that reads what the port holds.
    fn wake_on_queue(&mut self) {
        let first = self.model.drain();
        self.wake(first, |_| {});
    }

    fn shape(&self) -> Vec<(&'static str, String)> {
        self.events
            .pending()
            .map(|e| match e {
                DeviceEvent::Added(i) => ("added", i.name),
                DeviceEvent::Removed(i) => ("removed", i.name),
                DeviceEvent::Changed(i) => ("changed", i.name),
            })
            .collect()
    }
}

fn pair(gone: &str, came: &str) -> Vec<(&'static str, String)> {
    vec![("removed", gone.into()), ("added", came.into())]
}

fn twice(
    a: Vec<(&'static str, String)>,
    b: Vec<(&'static str, String)>,
) -> Vec<(&'static str, String)> {
    a.into_iter().chain(b).collect()
}

/// Client 5 was device "One"; it exits and client 5 is taken by "Two" while the
/// watch is looking, so the look shows "Two" and the announcements are still
/// unread. The device is removed once and added once, and the announcements read
/// in the next round do not take "Two" away again.
#[test]
fn an_exit_read_after_the_look_that_shows_its_successor_is_not_held_against_it() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[]), |m| {
        m.stream.borrow_mut().extend(REPLUG);
        *m.present.borrow_mut() = World::device("Two");
    });
    world.wake_on_queue();
    assert_eq!(world.shape(), pair("One", "Two"));
    assert_eq!(world.devices.list().len(), 1);
}

/// The old owner's `CLIENT_EXIT` is posted after the lock drops, so it can be
/// read after the new owner's start. Nor does the old owner's late change
/// matter. Neither is a start.
#[test]
fn late_exits_and_changes_of_an_old_owner_are_nothing() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[]), |m| {
        m.stream.borrow_mut().extend(START);
        *m.present.borrow_mut() = World::device("Two");
    });
    for late in [
        Announced::ClientExit(ClientId::of(5)),
        Announced::ClientChange(ClientId::of(5)),
        Announced::PortExit(Address::of(5, 0)),
        Announced::PortChange(Address::of(5, 0)),
    ] {
        world.wake(heard(&[late]), |_| {});
    }
    assert_eq!(world.shape(), pair("One", "Two"));
}

/// An identical device that takes the number is a new device each time.
#[test]
fn an_identical_replug_is_removed_then_added_each_time() {
    let mut world = World::with("One", &[]);
    for _ in 0..2 {
        world.wake(heard(&REPLUG), |_| {});
    }
    assert_eq!(world.shape(), twice(pair("One", "One"), pair("One", "One")));
}

/// A rename whose change announcement was lost in an overrun is only an
/// identity change, and nothing is left behind by it: a later replug of an
/// identical device is reported.
#[test]
fn a_rename_lost_in_an_overrun_does_not_hide_a_later_replug() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[]), |m| {
        *m.present.borrow_mut() = World::device("Renamed")
    });
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(
        world.shape(),
        twice(pair("One", "Renamed"), pair("Renamed", "Renamed"))
    );
}

/// Renamed twice, with its changes read at their own time: no state outlives
/// the renames, and a replug after them is reported.
#[test]
fn renamed_twice_then_replugged() {
    let mut world = World::with("One", &[]);
    for name in ["Two", "Three"] {
        world.wake(heard(&[Announced::ClientChange(ClientId::of(5))]), |m| {
            *m.present.borrow_mut() = World::device(name)
        });
    }
    world.wake(heard(&REPLUG), |_| {});
    let mut want = pair("One", "Two");
    want.extend(pair("Two", "Three"));
    want.extend(pair("Three", "Three"));
    assert_eq!(world.shape(), want);
}

/// One to Two to Three between two looks: the look shows Three, and One is
/// removed and Three added once.
#[test]
fn a_number_taken_twice_between_looks_is_one_replacement() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&REPLUG), |m| {
        m.stream.borrow_mut().extend(REPLUG);
        *m.present.borrow_mut() = World::device("Three");
    });
    assert_eq!(world.shape(), pair("One", "Three"));
}

/// A port made again on a client that lives is a new occupant of its handle:
/// removed and added, which closes a port held on the old one.
#[test]
fn a_port_made_again_on_a_live_client_is_a_new_occupant() {
    let mut world = World::with("One", &[]);
    world.wake(
        heard(&[
            Announced::PortExit(Address::of(5, 0)),
            Announced::PortStart(Address::of(5, 0)),
        ]),
        |_| {},
    );
    assert_eq!(world.shape(), pair("One", "One"));
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

/// The look shows a new, identical client whose start is queued but not yet
/// read when the look is taken: the read that follows the look finds it, and the
/// replacement is reported by that wake.
#[test]
fn a_look_that_sees_a_new_client_before_its_start_is_read_reports_it() {
    let mut world = World::with("One", &[]);
    world.wake(
        heard(&[
            Announced::PortExit(Address::of(5, 0)),
            Announced::ClientExit(ClientId::of(5)),
        ]),
        |m| {
            m.stream.borrow_mut().extend(START);
        },
    );
    assert_eq!(world.shape(), pair("One", "One"));
}

/// A device whose start is still in the port when the first look is taken is
/// the device that look lists: its start is not a new occupant.
#[test]
fn a_seeded_devices_queued_start_is_its_own() {
    let mut world = World::with("One", &START);
    world.wake_on_queue();
    assert_eq!(world.shape(), Vec::new());
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

/// When a settle does not quiet, the own start of a handle it lists first may
/// not be read yet. The handle is unconfirmed: the first start read for it is its
/// own, and the port start that follows its client's start is the same owner's.
#[test]
fn a_start_read_late_for_an_unconfirmed_handle_is_its_own() {
    let mut world = World::with("One", &[]);
    world.model.storm.set(true);
    world.wake(heard(&[]), |m| {
        m.present.borrow_mut().extend(World::device_on(6, "Two"));
    });
    world.model.storm.set(false);
    assert_eq!(world.shape(), vec![("added", "Two".to_string())]);
    world.wake(
        heard(&[
            Announced::ClientStart(ClientId::of(6)),
            Announced::PortStart(Address::of(6, 0)),
        ]),
        |_| {},
    );
    assert_eq!(world.shape(), Vec::new());
    world.wake(
        heard(&[
            Announced::PortExit(Address::of(6, 0)),
            Announced::ClientExit(ClientId::of(6)),
            Announced::ClientStart(ClientId::of(6)),
            Announced::PortStart(Address::of(6, 0)),
        ]),
        |_| {},
    );
    assert_eq!(world.shape(), pair("Two", "Two"));
}

/// A settle that does not quiet (other clients keep announcing) says nothing
/// about a handle already reported: its start was read, so a start read next is
/// a new occupant and is reported.
#[test]
fn a_settle_that_does_not_quiet_does_not_hide_a_replug() {
    let mut world = World::with("One", &[]);
    world.model.storm.set(true);
    world.wake(heard(&[]), |_| {});
    world.model.storm.set(false);
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

/// A start lost in an overrun cannot be known, so an identical replug inside the
/// overrun window is the one thing missed; the replug after it is reported.
#[test]
fn after_an_overrun_the_next_replug_is_reported() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[]), |_| {});
    assert_eq!(world.shape(), Vec::new(), "the lost start is the one limit");
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

/// A look cut short is not reported: the devices it omits are not removed. The
/// announcements read meanwhile are kept for the next look.
#[test]
fn a_look_that_never_succeeds_reports_nothing_and_keeps_the_starts() {
    let mut world = World::with("One", &[]);
    world.model.failing.set(100);
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), Vec::new());
    world.model.failing.set(0);
    world.wake(heard(&[]), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

/// A look that fails once is retried within the wake, and reports as if it had
/// not.
#[test]
fn a_look_that_fails_once_costs_nothing() {
    let mut world = World::with("One", &[]);
    world.model.failing.set(1);
    world.wake(heard(&[Announced::PortChange(Address::of(5, 0))]), |_| {});
    assert_eq!(world.shape(), Vec::new());
}

/// A new port is visible (`snd_seq_insert_port`) before its `PORT_START` is
/// posted, so a look lists a device whose own port start is read by a later
/// wake. That start is the port's own, not a port made again.
#[test]
fn a_new_port_listed_before_its_start_is_posted_is_not_made_again() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[Announced::ClientStart(ClientId::of(6))]), |m| {
        m.present.borrow_mut().extend(World::device_on(6, "Two"));
    });
    assert_eq!(world.shape(), vec![("added", "Two".to_string())]);
    world.wake(heard(&[Announced::PortStart(Address::of(6, 0))]), |_| {});
    assert_eq!(world.shape(), Vec::new());
    // The start was its own; the next one is a port made again.
    world.wake(
        heard(&[
            Announced::PortExit(Address::of(6, 0)),
            Announced::PortStart(Address::of(6, 0)),
        ]),
        |_| {},
    );
    assert_eq!(world.shape(), pair("Two", "Two"));
}

/// The own start can come several wakes after the port is listed.
#[test]
fn a_port_start_still_to_come_survives_other_wakes() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[Announced::ClientStart(ClientId::of(6))]), |m| {
        m.present.borrow_mut().extend(World::device_on(6, "Two"));
    });
    world.shape();
    for other in [
        Announced::PortChange(Address::of(6, 0)),
        Announced::ClientChange(ClientId::of(6)),
    ] {
        world.wake(heard(&[other]), |_| {});
    }
    world.wake(heard(&[Announced::PortStart(Address::of(6, 0))]), |_| {});
    assert_eq!(world.shape(), Vec::new());
}

/// An identical client takes the number and its port is listed before its
/// `PORT_START` is posted: the client's start already replaced the handle, and
/// the port start after it is that owner's own.
#[test]
fn a_replug_listed_before_its_port_start_is_not_made_again() {
    let mut world = World::with("One", &[]);
    world.wake(
        heard(&[
            Announced::PortExit(Address::of(5, 0)),
            Announced::ClientExit(ClientId::of(5)),
            Announced::ClientStart(ClientId::of(5)),
        ]),
        |_| {},
    );
    assert_eq!(world.shape(), pair("One", "One"));
    world.wake(heard(&[Announced::PortStart(Address::of(5, 0))]), |_| {});
    assert_eq!(world.shape(), Vec::new());
}

/// The announcements of an unplug are read, then every look fails: the watch
/// has nothing to wait on, so it asks to be woken again, and the removal is
/// reported by that wake and not by some other announcement.
#[test]
fn a_removal_read_while_every_look_fails_is_reported_by_the_retry() {
    let mut world = World::with("One", &[]);
    world.model.failing.set(1000);
    world.wake(
        heard(&[
            Announced::PortExit(Address::of(5, 0)),
            Announced::ClientExit(ClientId::of(5)),
        ]),
        |m| {
            m.present.borrow_mut().clear();
        },
    );
    assert_eq!(world.shape(), Vec::new());
    assert!(world.tracker.retry_after().is_some());
    world.model.failing.set(0);
    world.timeout_wake();
    assert_eq!(world.shape(), vec![("removed", "One".to_string())]);
    assert!(
        world.tracker.retry_after().is_none(),
        "a look ends the retries"
    );
}

#[test]
fn the_retry_backs_off_to_a_second() {
    use std::time::Duration;
    let ms = |n| backoff(n).as_millis();
    assert_eq!((ms(1), ms(2), ms(3)), (10, 20, 40));
    assert_eq!((ms(8), ms(9), ms(u32::MAX)), (1000, 1000, 1000));
    assert!(backoff(1) < Duration::from_secs(1));
}

/// A first look that fails is retried; until one is whole nothing is listed.
#[test]
fn a_first_look_that_fails_is_retried() {
    let mut world = World::build("One", &[], 1000);
    assert!(world.devices.list().is_empty());
    assert!(world.tracker.retry_after().is_some());
    world.model.failing.set(0);
    world.timeout_wake();
    assert_eq!(world.shape(), vec![("added", "One".to_string())]);
    assert!(world.tracker.retry_after().is_none());
}

/// Announcements held while no look succeeds are bounded; past the bound they are
/// dropped as an overrun drops them, and the next look is the truth.
#[test]
fn pending_announcements_are_bounded() {
    let mut world = World::with("One", &[]);
    world.model.failing.set(10_000);
    world.wake(
        heard(&[Announced::PortChange(Address::of(5, 0)); 3]),
        |_| {},
    );
    assert_eq!(world.tracker.pending.len(), 3);
    world.wake(
        heard(&vec![
            Announced::PortChange(Address::of(5, 0));
            MAX_PENDING + 1
        ]),
        |_| {},
    );
    assert!(world.tracker.pending.is_empty());
    world.model.failing.set(0);
    world.timeout_wake();
    assert_eq!(world.shape(), Vec::new());
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

#[test]
fn a_read_stops_at_its_cap() {
    let at = |n| Drained {
        events: vec![Announced::PortChange(Address::of(5, 0)); n],
        any: true,
    };
    assert!(!at(MAX_DRAIN_EVENTS - 1).full());
    assert!(at(MAX_DRAIN_EVENTS).full());
}

/// A look that did not quiet can list a new occupant on a reported handle whose
/// client start is still unread; the wake that reads it is not a second
/// replacement.
#[test]
fn a_new_occupant_listed_by_a_look_that_did_not_quiet_has_its_start_still_to_come() {
    let mut world = World::with("One", &[]);
    world.model.storm.set(true);
    world.wake(heard(&[]), |m| {
        *m.present.borrow_mut() = World::device("Two")
    });
    world.model.storm.set(false);
    assert_eq!(world.shape(), pair("One", "Two"));
    world.wake(heard(&START), |_| {});
    assert_eq!(world.shape(), Vec::new());
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("Two", "Two"));
}

/// A rename read in a look that did not quiet is a change of the occupant whose
/// start was read: the old client exits and an identical one takes the number,
/// and the new start is a new occupant, reported removed and added, whether the
/// rename's change is read in the same wake as the replug or before it.
#[test]
fn a_rename_listed_by_a_look_that_did_not_quiet_does_not_hide_a_replug() {
    for changes_first in [false, true] {
        let mut world = World::with("One", &[]);
        world.model.storm.set(true);
        world.wake(heard(&[]), |m| {
            *m.present.borrow_mut() = World::device("Renamed")
        });
        world.model.storm.set(false);
        assert_eq!(world.shape(), pair("One", "Renamed"));
        if changes_first {
            world.wake(heard(&[Announced::ClientChange(ClientId::of(5))]), |_| {});
            world.wake(heard(&REPLUG), |_| {});
        } else {
            let mut announced = vec![Announced::ClientChange(ClientId::of(5))];
            announced.extend(REPLUG);
            world.wake(heard(&announced), |_| {});
        }
        assert_eq!(world.shape(), pair("Renamed", "Renamed"));
    }
}

/// Wakes `world` as a non-quiet look that lists client 5 as `name` and reads
/// `first`, then reads `rest`; returns after the quiet wake.
fn non_quiet_rename(world: &mut World, name: &str, first: &[Announced], rest: &[Announced]) {
    world.model.storm.set(true);
    world.wake(heard(first), |m| {
        *m.present.borrow_mut() = World::device(name)
    });
    world.model.storm.set(false);
    world.wake(heard(rest), |_| {});
}

/// The rename's change read in the same wake as the look that lists it is
/// already the verdict: the next start is a replug.
#[test]
fn a_change_read_with_the_look_that_lists_the_rename_is_a_rename() {
    let mut world = World::with("One", &[]);
    non_quiet_rename(
        &mut world,
        "Renamed",
        &[Announced::ClientChange(ClientId::of(5))],
        &REPLUG,
    );
    assert_eq!(
        world.shape(),
        twice(pair("One", "Renamed"), pair("Renamed", "Renamed"))
    );
}

/// A port's change is the rename of that port's device.
#[test]
fn a_port_change_is_a_rename() {
    let mut world = World::with("One", &[]);
    non_quiet_rename(
        &mut world,
        "Renamed",
        &[Announced::PortChange(Address::of(5, 0))],
        &REPLUG,
    );
    assert_eq!(
        world.shape(),
        twice(pair("One", "Renamed"), pair("Renamed", "Renamed"))
    );
}

/// A new occupant listed by a look that did not quiet: the exit of the old
/// owner's port tells it, so a change read after it (the new owner's, or the old
/// one's late) is no rename, and the start that follows is the occupant's own.
#[test]
fn a_port_exit_read_for_an_undecided_handle_is_a_new_occupant() {
    let mut world = World::with("One", &[]);
    world.model.storm.set(true);
    world.wake(heard(&[]), |m| {
        *m.present.borrow_mut() = World::device("Two")
    });
    world.wake(
        heard(&[
            Announced::PortExit(Address::of(5, 0)),
            Announced::ClientChange(ClientId::of(5)),
        ]),
        |_| {},
    );
    world.model.storm.set(false);
    world.wake(heard(&START), |_| {});
    assert_eq!(world.shape(), pair("One", "Two"));
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("Two", "Two"));
}

/// A quiet look says the start of every listed client was read: a handle left
/// undecided by an earlier look is decided, and a replug is reported.
#[test]
fn a_quiet_look_decides_an_undecided_handle() {
    let mut world = World::with("One", &[]);
    world.model.storm.set(true);
    world.wake(heard(&[]), |m| {
        *m.present.borrow_mut() = World::device("Renamed")
    });
    world.model.storm.set(false);
    world.wake(heard(&[]), |_| {});
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(
        world.shape(),
        twice(pair("One", "Renamed"), pair("Renamed", "Renamed"))
    );
}

/// A change read for an earlier rename cannot be told from the rename of the
/// identity the look lists: a rename and a replug to "Three" that happen before
/// the look, with only the first change read, is taken for a rename, and the
/// start read next is reported again. The error is a second pair, never a miss.
#[test]
fn a_replug_listed_after_a_read_change_is_reported_again_by_its_start() {
    let mut world = World::with("One", &[]);
    non_quiet_rename(
        &mut world,
        "Three",
        &[Announced::ClientChange(ClientId::of(5))],
        &REPLUG,
    );
    assert_eq!(
        world.shape(),
        twice(pair("One", "Three"), pair("Three", "Three"))
    );
}

/// A port's change read in a wake after the look that lists the rename decides
/// it, as a client's does.
#[test]
fn a_port_change_read_after_the_look_decides_the_rename() {
    let mut world = World::with("One", &[]);
    world.model.storm.set(true);
    world.wake(heard(&[]), |m| {
        *m.present.borrow_mut() = World::device("Renamed")
    });
    world.wake(heard(&[Announced::PortChange(Address::of(5, 0))]), |_| {});
    world.model.storm.set(false);
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(
        world.shape(),
        twice(pair("One", "Renamed"), pair("Renamed", "Renamed"))
    );
}

/// The exit of the old owner's port read before a change in the wake that lists
/// the new occupant tells a replacement, not a rename: the start read next is
/// the occupant's own.
#[test]
fn a_port_exit_read_before_a_change_with_the_look_is_a_replacement() {
    let mut world = World::with("One", &[]);
    non_quiet_rename(
        &mut world,
        "Two",
        &[
            Announced::PortExit(Address::of(5, 0)),
            Announced::ClientChange(ClientId::of(5)),
        ],
        &START,
    );
    assert_eq!(world.shape(), pair("One", "Two"));
}

/// A replacement a look lists after a rename, whose port exit was read, is not
/// made a rename by a change read after it.
#[test]
fn a_replacement_listed_after_a_rename_is_not_taken_for_one() {
    let mut world = World::with("One", &[]);
    non_quiet_rename(&mut world, "Renamed", &[], &[]);
    world.model.storm.set(true);
    world.wake(heard(&[Announced::PortExit(Address::of(5, 0))]), |m| {
        *m.present.borrow_mut() = World::device("Two")
    });
    world.wake(heard(&[Announced::ClientChange(ClientId::of(5))]), |_| {});
    world.model.storm.set(false);
    world.wake(heard(&START), |_| {});
    assert_eq!(
        world.shape(),
        twice(pair("One", "Renamed"), pair("Renamed", "Two"))
    );
}
