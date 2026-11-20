//! `refresh_pool`: a file entry's stamp is taken at its load, the pool's diff
//! never reads the file, and a refresh loads again the entries whose file
//! changed. A refresh is run and applied by the test.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tutti_platform::Asset;

use super::{Spawned, events, session, state};
use crate::{
    Declared, Life, LoadError, PoolEntry, PoolKey, PoolLoad, RefreshError, RefreshReport, Sharing,
};

/// A kind that reads a whole file: the loaded value is its bytes.
struct Read {
    path: PathBuf,
    reads: Arc<AtomicUsize>,
}

impl PoolEntry for Read {
    type Identity = PathBuf;
    type Loaded = Arc<Vec<u8>>;
    type Handle = Arc<Vec<u8>>;

    fn identity(&self) -> PathBuf {
        self.path.clone()
    }

    fn load(&self) -> PoolLoad<Arc<Vec<u8>>> {
        let (path, reads) = (self.path.clone(), Arc::clone(&self.reads));
        PoolLoad::on_worker(move || {
            reads.fetch_add(1, Ordering::SeqCst);
            std::fs::read(path)
                .map(Arc::new)
                .map_err(|_| LoadError::Job(crate::WorkerError::Dropped))
        })
    }

    fn asset(&self) -> Option<Asset> {
        Some(Asset::from(self.path.as_path()))
    }

    fn handle(loaded: &Arc<Vec<u8>>, _: &Life) -> Arc<Vec<u8>> {
        Arc::clone(loaded)
    }
}

fn read(
    key: &'static str,
    path: &Path,
    reads: &Arc<AtomicUsize>,
) -> Declared<&'static str, Read, Sharing> {
    Declared::shared(
        key,
        Read {
            path: path.to_path_buf(),
            reads: Arc::clone(reads),
        },
    )
}

fn bytes(session: &crate::DetachedSession, key: &'static str) -> Vec<u8> {
    session
        .pool
        .entries
        .handle::<_, Read>(&key)
        .unwrap()
        .to_vec()
}

/// Runs a refresh and applies it.
fn refreshed(s: &mut crate::DetachedSession, _: &Spawned) -> RefreshReport {
    let done = s.refresh_pool().run();
    s.apply(done).expect("the refresh is current")
}

fn loaded(dir: &Path, text: &[u8]) -> (crate::DetachedSession, Spawned, PathBuf, Arc<AtomicUsize>) {
    let path = dir.join("a.bin");
    std::fs::write(&path, text).unwrap();
    let (mut s, spawned) = session();
    let reads = Arc::new(AtomicUsize::new(0));
    s.pool(read("a", &path, &reads)).unwrap();
    spawned.run_until_stalled();
    let _ = events(&mut s);
    (s, spawned, path, reads)
}

/// A kind that reads an asset of bytes: its identity never changes.
struct Held {
    asset: Asset,
}

impl PoolEntry for Held {
    type Identity = u8;
    type Loaded = u8;
    type Handle = u8;

    fn identity(&self) -> u8 {
        0
    }

    fn load(&self) -> PoolLoad<u8> {
        let asset = self.asset.clone();
        PoolLoad::on_worker(move || {
            asset
                .open()
                .map(|_| 0)
                .map_err(|_| LoadError::Job(crate::WorkerError::Dropped))
        })
    }

    fn asset(&self) -> Option<Asset> {
        Some(self.asset.clone())
    }

    fn handle(loaded: &u8, _: &Life) -> u8 {
        *loaded
    }
}

#[test]
fn an_asset_of_bytes_never_refreshes() {
    let (mut s, spawned) = session();
    s.pool(Declared::new(
        "a",
        Held {
            asset: Asset::from(b"one".to_vec()),
        },
    ))
    .unwrap();
    spawned.run_until_stalled();
    let _ = events(&mut s);
    for _ in 0..2 {
        let found = refreshed(&mut s, &spawned);
        assert!(found.reloaded().is_empty() && found.unreadable().is_empty());
        assert!(spawned.run_until_stalled() == 0, "nothing started");
    }
}

#[test]
fn a_refresh_of_unchanged_files_loads_nothing_again() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, spawned, _, reads) = loaded(dir.path(), b"one");
    let found = refreshed(&mut s, &spawned);
    assert!(found.reloaded().is_empty() && found.unreadable().is_empty());
    assert!(spawned.run_until_stalled() == 0, "nothing started");
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn a_refresh_loads_again_the_entry_whose_file_changed_and_keeps_the_old_value_until_it_lands() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, spawned, path, reads) = loaded(dir.path(), b"one");
    std::fs::write(&path, b"longer").unwrap();
    let found = refreshed(&mut s, &spawned);
    assert_eq!(found.reloaded(), [PoolKey::new(&"a")]);
    assert_eq!(state(&s, "a").as_deref(), Some("reloading"));
    assert_eq!(bytes(&s, "a"), b"one", "the old value stays");
    spawned.run_until_stalled();
    assert_eq!(events(&mut s), [("a", "reloaded".to_owned())]);
    assert_eq!(bytes(&s, "a"), b"longer");
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    let again = refreshed(&mut s, &spawned);
    assert!(again.reloaded().is_empty(), "the new stamp is the file's");
}

#[test]
fn a_refresh_that_is_never_applied_starts_no_reload() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, spawned, path, reads) = loaded(dir.path(), b"one");
    std::fs::write(&path, b"longer").unwrap();
    drop(s.refresh_pool().run());
    spawned.run_until_stalled();
    let _ = s.update();
    assert!(spawned.run_until_stalled() == 0, "no reload started");
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn the_pool_diff_never_reads_the_file_so_a_declared_key_shares_the_stale_entry_until_a_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, spawned, path, reads) = loaded(dir.path(), b"one");
    std::fs::write(&path, b"changed on disk").unwrap();
    s.pool((read("a", &path, &reads), read("b", &path, &reads)))
        .unwrap();
    spawned.run_until_stalled();
    let _ = events(&mut s);
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "no load for an unchanged identity"
    );
    assert_eq!(bytes(&s, "a"), b"one");
    assert_eq!(
        bytes(&s, "b"),
        b"one",
        "the new key shares the loaded entry"
    );
    refreshed(&mut s, &spawned);
    spawned.run_until_stalled();
    let _ = events(&mut s);
    assert_eq!(bytes(&s, "a"), b"changed on disk");
}

#[test]
fn two_keys_of_one_changed_file_reload_with_one_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.bin");
    std::fs::write(&path, b"one").unwrap();
    let (mut s, spawned) = session();
    let reads = Arc::new(AtomicUsize::new(0));
    s.pool((read("a", &path, &reads), read("b", &path, &reads)))
        .unwrap();
    spawned.run_until_stalled();
    let _ = events(&mut s);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    std::fs::write(&path, b"changed").unwrap();
    let found = refreshed(&mut s, &spawned);
    assert_eq!(found.reloaded().len(), 2);
    spawned.run_until_stalled();
    assert_eq!(
        events(&mut s),
        [("a", "reloaded".to_owned()), ("b", "reloaded".to_owned())]
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2, "one read for both keys");
    assert_eq!(bytes(&s, "b"), b"changed");
}

#[test]
fn a_file_that_cannot_be_read_is_named_and_its_entry_keeps_its_value() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, spawned, path, _) = loaded(dir.path(), b"one");
    std::fs::remove_file(&path).unwrap();
    let found = refreshed(&mut s, &spawned);
    assert!(found.reloaded().is_empty());
    assert_eq!(
        found.unreadable(),
        [(PoolKey::new(&"a"), std::io::ErrorKind::NotFound)]
    );
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
    assert_eq!(bytes(&s, "a"), b"one");
}

#[test]
fn a_refresh_whose_entry_reloaded_since_is_refused_as_stale() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, spawned, path, reads) = loaded(dir.path(), b"one");
    std::fs::write(&path, b"changed").unwrap();
    let done = s.refresh_pool().run();
    // The app declares another path under the key after the refresh was cut.
    let other = dir.path().join("b.bin");
    std::fs::write(&other, b"other").unwrap();
    s.pool(read("a", &other, &reads)).unwrap();
    spawned.run_until_stalled();
    let _ = events(&mut s);
    assert_eq!(s.apply(done).unwrap_err(), RefreshError::Stale);
    assert!(spawned.run_until_stalled() == 0, "nothing reloaded");
    assert_eq!(bytes(&s, "a"), b"other");
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}

#[test]
fn a_refresh_whose_entry_was_released_is_refused_as_stale() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, spawned, path, reads) = loaded(dir.path(), b"one");
    std::fs::write(&path, b"changed").unwrap();
    let done = s.refresh_pool().run();
    s.pool(read("z", &path, &reads)).unwrap();
    let _ = events(&mut s);
    assert!(state(&s, "a").is_none(), "released");
    assert_eq!(s.apply(done).unwrap_err(), RefreshError::Stale);
    assert!(spawned.run_until_stalled() == 0, "nothing reloaded");
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn an_output_of_another_session_is_refused_as_stale() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _spawned_a, path, reads_a) = loaded(dir.path(), b"one");
    let (mut b, spawned_b) = session();
    let reads_b = Arc::new(AtomicUsize::new(0));
    b.pool(read("a", &path, &reads_b)).unwrap();
    spawned_b.run_until_stalled();
    let _ = events(&mut b);
    let generation = |s: &crate::DetachedSession| s.pool.entries.stamped_files()[0].generation;
    assert_eq!(generation(&a), generation(&b), "the generations match");
    std::fs::write(&path, b"changed").unwrap();
    let done = a.refresh_pool().run();
    assert_eq!(b.apply(done).unwrap_err(), RefreshError::Stale);
    assert!(spawned_b.run_until_stalled() == 0, "nothing reloaded");
    assert_eq!(state(&b, "a").as_deref(), Some("loaded"));
    assert_eq!(reads_b.load(Ordering::SeqCst), 1);
    assert_eq!(reads_a.load(Ordering::SeqCst), 1);
}

#[test]
fn a_kind_with_no_file_is_not_refreshed() {
    let (mut s, spawned) = session();
    let runs = super::runs();
    s.pool(super::entry("a", super::probe(1, &runs))).unwrap();
    spawned.run_until_stalled();
    let _ = events(&mut s);
    let found = refreshed(&mut s, &spawned);
    assert!(found.reloaded().is_empty() && found.unreadable().is_empty());
}

/// A kind whose value lands with no worker stage, so no stamp was taken.
struct Unstamped {
    path: PathBuf,
}

impl PoolEntry for Unstamped {
    type Identity = PathBuf;
    type Loaded = u8;
    type Handle = u8;

    fn identity(&self) -> PathBuf {
        self.path.clone()
    }

    fn load(&self) -> PoolLoad<u8> {
        PoolLoad::ready(0)
    }

    fn asset(&self) -> Option<Asset> {
        Some(Asset::from(self.path.as_path()))
    }

    fn handle(loaded: &u8, _: &Life) -> u8 {
        *loaded
    }
}

#[test]
fn an_entry_with_no_stamp_is_stamped_by_the_refresh_and_loads_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.bin");
    std::fs::write(&path, b"one").unwrap();
    let (mut s, spawned) = session();
    s.pool(Declared::new("a", Unstamped { path })).unwrap();
    let _ = events(&mut s);
    assert!(
        s.pool
            .entries
            .stamped_files()
            .iter()
            .all(|stamped| stamped.was.is_none())
    );
    let found = refreshed(&mut s, &spawned);
    assert_eq!(
        found.reloaded(),
        [PoolKey::new(&"a")],
        "now readable, so stamped by loading"
    );
}

#[test]
fn a_reload_never_copies_the_stale_value_of_a_held_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.bin");
    std::fs::write(&path, b"one").unwrap();
    let (mut s, spawned) = session();
    let reads = Arc::new(AtomicUsize::new(0));
    s.pool((read("a", &path, &reads), read("b", &path, &reads)))
        .unwrap();
    spawned.run_until_stalled();
    let _ = events(&mut s);
    let _hold = s
        .pool
        .entries
        .hold(&PoolKey::new(&"b"), crate::pool::table::HoldKind::Node)
        .unwrap();
    s.pool(read("a", &path, &reads)).unwrap();
    assert_eq!(state(&s, "b").as_deref(), Some("held"));
    std::fs::write(&path, b"changed").unwrap();
    refreshed(&mut s, &spawned);
    assert_eq!(
        state(&s, "a").as_deref(),
        Some("reloading"),
        "not the held sibling's old value"
    );
    spawned.run_until_stalled();
    let _ = events(&mut s);
    assert_eq!(bytes(&s, "a"), b"changed");
}
