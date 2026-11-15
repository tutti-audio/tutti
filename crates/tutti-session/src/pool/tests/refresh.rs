//! `refresh_pool`: a file entry's stamp is taken at its load, the pool's diff
//! never reads the file, and a refresh loads again the entries whose file
//! changed. The jobs run when a test says.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{Spawned, events, session, state};
use crate::{Declared, Life, Load, LoadError, PoolEntry, PoolKey, RefreshPool, Session};

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

    fn load(&self) -> Load<Arc<Vec<u8>>> {
        let (path, reads) = (self.path.clone(), Arc::clone(&self.reads));
        Load::on_worker(move || {
            reads.fetch_add(1, Ordering::SeqCst);
            std::fs::read(path)
                .map(Arc::new)
                .map_err(|_| LoadError::Job(crate::jobs::JobFailed::Dropped))
        })
    }

    fn file(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn handle(loaded: &Arc<Vec<u8>>, _: &Life) -> Arc<Vec<u8>> {
        Arc::clone(loaded)
    }
}

fn read(key: &'static str, path: &Path, reads: &Arc<AtomicUsize>) -> Declared<&'static str, Read> {
    Declared::shared(
        key,
        Read {
            path: path.to_path_buf(),
            reads: Arc::clone(reads),
        },
    )
}

fn bytes(session: &Session, key: &'static str) -> Vec<u8> {
    session.pool.handle::<_, Read>(&key).unwrap().to_vec()
}

/// Runs a refresh to its landing: the stat job, then the update that lands it.
fn refreshed(s: &mut Session, spawned: &Spawned) -> RefreshPool {
    let wait = s.refresh_pool();
    spawned.run_all();
    let _ = s.update();
    wait.take().expect("landed").expect("the job finished")
}

fn loaded(dir: &Path, text: &[u8]) -> (Session, Spawned, PathBuf, Arc<AtomicUsize>) {
    let path = dir.join("a.bin");
    std::fs::write(&path, text).unwrap();
    let (mut s, spawned) = session();
    let reads = Arc::new(AtomicUsize::new(0));
    s.pool(read("a", &path, &reads)).unwrap();
    spawned.run_all();
    let _ = events(&mut s);
    (s, spawned, path, reads)
}

#[test]
fn a_refresh_of_unchanged_files_loads_nothing_again() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, spawned, _, reads) = loaded(dir.path(), b"one");
    let found = refreshed(&mut s, &spawned);
    assert!(found.reloaded().is_empty() && found.unreadable().is_empty());
    assert!(spawned.take().is_empty(), "nothing started");
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
    spawned.run_all();
    assert_eq!(events(&mut s), [("a", "reloaded".to_owned())]);
    assert_eq!(bytes(&s, "a"), b"longer");
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    let again = refreshed(&mut s, &spawned);
    assert!(again.reloaded().is_empty(), "the new stamp is the file's");
}

#[test]
fn the_pool_diff_never_reads_the_file_so_a_declared_key_shares_the_stale_entry_until_a_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, spawned, path, reads) = loaded(dir.path(), b"one");
    std::fs::write(&path, b"changed on disk").unwrap();
    s.pool((read("a", &path, &reads), read("b", &path, &reads)))
        .unwrap();
    spawned.run_all();
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
    spawned.run_all();
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
    spawned.run_all();
    let _ = events(&mut s);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    std::fs::write(&path, b"changed").unwrap();
    let found = refreshed(&mut s, &spawned);
    assert_eq!(found.reloaded().len(), 2);
    spawned.run_all();
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
fn an_entry_that_reloaded_meanwhile_is_not_reloaded_by_the_refresh_that_began_before() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, spawned, path, reads) = loaded(dir.path(), b"one");
    std::fs::write(&path, b"changed").unwrap();
    let wait = s.refresh_pool();
    // The app declares another path under the key while the check runs.
    let other = dir.path().join("b.bin");
    std::fs::write(&other, b"other").unwrap();
    s.pool(read("a", &other, &reads)).unwrap();
    spawned.run_all();
    let _ = s.update();
    spawned.run_all();
    let _ = events(&mut s);
    let found = wait.take().expect("landed").expect("finished");
    assert!(found.reloaded().is_empty(), "the stamp it checked is gone");
    assert_eq!(bytes(&s, "a"), b"other");
}

#[test]
fn a_kind_with_no_file_is_not_refreshed() {
    let (mut s, spawned) = session();
    let runs = super::runs();
    s.pool(super::entry("a", super::probe(1, &runs))).unwrap();
    spawned.run_all();
    let _ = events(&mut s);
    let found = refreshed(&mut s, &spawned);
    assert!(found.reloaded().is_empty() && found.unreadable().is_empty());
}
