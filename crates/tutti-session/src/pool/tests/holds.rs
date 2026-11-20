//! Holds have a depth: the newest snapshot holds keep their entries live,
//! older ones park a kind that releases, and recalling a parked entry loads it
//! again. The jobs run when a test says.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tutti_units::SampleRate;

use super::{Background, events, state};
use crate::pool::table::{Hold, HoldKind};
use crate::{Declared, Life, Load, PoolConfig, PoolEntry, PoolKey, Retention, Session};

/// A kind with something live behind it: it releases when parked, and loads
/// again when declared after.
struct Device {
    version: u32,
    loads: Arc<AtomicUsize>,
}

impl PoolEntry for Device {
    type Identity = u32;
    type Loaded = u32;
    type Handle = Life;

    fn identity(&self) -> u32 {
        self.version
    }

    fn load(&self) -> Load<u32> {
        let (version, loads) = (self.version, Arc::clone(&self.loads));
        Load::on_worker(move || {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(version)
        })
    }

    fn handle(_: &u32, life: &Life) -> Life {
        life.clone()
    }

    fn retain(_: u32) -> Retention<u32> {
        Retention::Release
    }
}

/// A kind that keeps its value when parked (the default), as a data kind does.
struct Shared {
    loads: Arc<AtomicUsize>,
}

impl PoolEntry for Shared {
    type Identity = u32;
    type Loaded = u32;
    type Handle = Life;

    fn identity(&self) -> u32 {
        0
    }

    fn load(&self) -> Load<u32> {
        let loads = Arc::clone(&self.loads);
        Load::on_worker(move || {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        })
    }

    fn handle(_: &u32, life: &Life) -> Life {
        life.clone()
    }
}

fn session(config: PoolConfig) -> (crate::DetachedSession, Background) {
    let (session, bg) = Session::builder()
        .pool_config(config)
        .build_detached(SampleRate::SR_48K)
        .unwrap();
    (session, bg)
}

fn device(key: &'static str, loads: &Arc<AtomicUsize>) -> Declared<&'static str, Device> {
    Declared::new(
        key,
        Device {
            version: 1,
            loads: Arc::clone(loads),
        },
    )
}

fn life(session: &crate::DetachedSession, key: &'static str) -> Life {
    session.pool.entries.handle::<_, Device>(&key).unwrap()
}

/// Declares `keys`, loads them, and takes a snapshot hold on each in order,
/// then undeclares them: the pool holds only what the holds keep.
fn held(
    s: &mut crate::DetachedSession,
    bg: &mut Background,
    loads: &Arc<AtomicUsize>,
    keys: &[&'static str],
) -> (Vec<Life>, Vec<Hold>) {
    s.pool(
        keys.iter()
            .map(|key| device(key, loads))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    // The parallel cap starts the rest as running loads land.
    while bg.run_until_stalled() > 0 {
        let _ = events(s);
    }
    let lives = keys.iter().map(|key| life(s, key)).collect();
    let holds = keys
        .iter()
        .map(|key| {
            s.pool
                .entries
                .hold(&PoolKey::new(key), HoldKind::Snapshot)
                .unwrap()
        })
        .collect();
    s.pool::<&'static str>(()).unwrap();
    (lives, holds)
}

#[test]
fn a_snapshot_hold_past_the_depth_parks_a_kind_that_releases_and_the_newest_stay_live() {
    let (mut s, mut bg) = session(PoolConfig::new().with_live_holds(2));
    let loads = Arc::new(AtomicUsize::new(0));
    let (lives, _holds) = held(&mut s, &mut bg, &loads, &["a", "b", "c"]);
    let _ = events(&mut s);
    assert!(lives[0].is_closed(), "the oldest hold's entry parked");
    assert!(
        !lives[1].is_closed() && !lives[2].is_closed(),
        "the newest two live"
    );
    for key in ["a", "b", "c"] {
        assert_eq!(
            state(&s, key).as_deref(),
            Some("held"),
            "{key} is held, not released"
        );
    }
    assert!(events(&mut s).is_empty(), "parking is not a release");
}

#[test]
fn the_default_depth_is_eight_holds() {
    let (mut s, mut bg) = session(PoolConfig::new());
    let loads = Arc::new(AtomicUsize::new(0));
    let keys = ["k0", "k1", "k2", "k3", "k4", "k5", "k6", "k7", "k8"];
    let (lives, _holds) = held(&mut s, &mut bg, &loads, &keys);
    let _ = events(&mut s);
    let closed: Vec<bool> = lives.iter().map(Life::is_closed).collect();
    assert_eq!(
        closed,
        [true, false, false, false, false, false, false, false, false],
        "only the ninth newest parks"
    );
}

#[test]
fn recalling_a_parked_entry_loads_it_again_and_a_live_one_adopts_with_no_load() {
    let (mut s, mut bg) = session(PoolConfig::new().with_live_holds(1));
    let loads = Arc::new(AtomicUsize::new(0));
    let (lives, _holds) = held(&mut s, &mut bg, &loads, &["old", "new"]);
    let _ = events(&mut s);
    assert!(lives[0].is_closed() && !lives[1].is_closed());
    assert_eq!(loads.load(Ordering::SeqCst), 2);

    s.pool((device("old", &loads), device("new", &loads)))
        .unwrap();
    assert_eq!(state(&s, "new").as_deref(), Some("loaded"), "adopted");
    assert_eq!(
        state(&s, "old").as_deref(),
        Some("loading"),
        "reloading from the park"
    );
    bg.run_until_stalled();
    assert_eq!(events(&mut s), [("old", "loaded".to_owned())]);
    assert_eq!(
        loads.load(Ordering::SeqCst),
        3,
        "only the parked one loaded again"
    );
    assert!(!life(&s, "old").is_closed(), "the recalled entry is live");
    assert!(
        lives[0].is_closed(),
        "the handle of the parked value stays closed"
    );
    assert!(!lives[1].is_closed());
}

#[test]
fn a_node_hold_never_parks_however_old() {
    let (mut s, mut bg) = session(PoolConfig::new().with_live_holds(0));
    let loads = Arc::new(AtomicUsize::new(0));
    s.pool(device("a", &loads)).unwrap();
    bg.run_until_stalled();
    let _ = events(&mut s);
    let handle = life(&s, "a");
    let _hold = s
        .pool
        .entries
        .hold(&PoolKey::new(&"a"), HoldKind::Node)
        .unwrap();
    s.pool::<&'static str>(()).unwrap();
    let _ = events(&mut s);
    assert!(!handle.is_closed());
}

#[test]
fn a_kind_that_keeps_its_value_is_not_parked_past_the_depth() {
    let (mut s, mut bg) = session(PoolConfig::new().with_live_holds(0));
    let loads = Arc::new(AtomicUsize::new(0));
    s.pool(Declared::new(
        "a",
        Shared {
            loads: Arc::clone(&loads),
        },
    ))
    .unwrap();
    bg.run_until_stalled();
    let _ = events(&mut s);
    let handle = s.pool.entries.handle::<_, Shared>(&"a").unwrap();
    let _hold = s
        .pool
        .entries
        .hold(&PoolKey::new(&"a"), HoldKind::Snapshot)
        .unwrap();
    s.pool::<&'static str>(()).unwrap();
    let _ = events(&mut s);
    assert!(!handle.is_closed());
    s.pool(Declared::new(
        "a",
        Shared {
            loads: Arc::clone(&loads),
        },
    ))
    .unwrap();
    assert!(bg.run_until_stalled() == 0, "adopted with no load");
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

#[test]
fn the_last_hold_of_a_parked_entry_releases_it() {
    let (mut s, mut bg) = session(PoolConfig::new().with_live_holds(0));
    let loads = Arc::new(AtomicUsize::new(0));
    let (lives, holds) = held(&mut s, &mut bg, &loads, &["a"]);
    let _ = events(&mut s);
    assert!(lives[0].is_closed(), "parked");
    drop(holds);
    assert_eq!(events(&mut s), [("a", "released".to_owned())]);
    assert_eq!(state(&s, "a"), None);
}
