//! Which thread plays the CLAP `[main-thread]` and `[audio-thread]` roles.
//!
//! A plugin asks the host (`clap.thread-check`) which thread a call is on,
//! and the audio-side entries of one plugin must never overlap. Both are one
//! object's job: the claim on the `[audio-thread]` role is a lock, and holding
//! it publishes the claiming thread's identity.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::thread::ThreadId;
use tutti_rt::LockExt;

use tutti_plugin_types::MainThreadMarker;

/// The thread roles of one plugin instance.
#[derive(Debug)]
pub struct ThreadRoles {
    main: ThreadId,
    // An `ArcSwapOption<ThreadId>` would not do: `store` waits for readers and
    // can run the retired `Arc`'s deallocation on the audio thread, and a
    // one-slot `Arc` cache is evicted whenever a GUI thread claims the role
    // between two blocks. A hash in an atomic allocates nothing.
    audio: AtomicU64,
    lock: Mutex<()>,
}

const NO_AUDIO_THREAD: u64 = 0;

fn thread_id_hash(id: ThreadId) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    // Fold the sentinel away so a real thread can never be read as "unclaimed".
    match hasher.finish() {
        NO_AUDIO_THREAD => 1,
        h => h,
    }
}

/// RAII claim on the `[audio-thread]` role for one plugin instance.
///
/// While alive it holds the instance's role lock and has published the
/// claiming thread, so [`ThreadRoles::is_audio_thread`] answers `true` on
/// this thread and `false` everywhere else, and
/// [`ThreadRoles::is_main_thread`] answers `false` on this thread even when it
/// is the OS main thread (the spec lets a host drive `start_processing` from
/// there). Not re-entrant: claiming twice on one thread deadlocks.
#[derive(Debug)]
pub(crate) struct AudioClaim<'a> {
    roles: &'a ThreadRoles,
    // Held for the claim's lifetime; declared last so it releases only after
    // `Drop` has cleared the published identity.
    _guard: MutexGuard<'a, ()>,
}

impl Drop for AudioClaim<'_> {
    fn drop(&mut self) {
        // One store of a `u64`: nothing to free, so this cannot deallocate on
        // the audio thread. `Release` pairs with the `Acquire` in
        // `is_audio_thread`.
        self.roles.audio.store(NO_AUDIO_THREAD, Ordering::Release);
    }
}

/// The roles lock held for `deactivate` and `destroy`, which are
/// `[main-thread]` entries: no audio-side entry can run beside them, but the
/// holder stays the main thread, so the callbacks a plugin makes from them
/// (`unregister_timer`, `latency.changed`, `params.rescan`, ...) are legal and
/// answered as such. The one audio-side entry in the sequence, `stop_processing`,
/// runs inside [`audio`](Self::audio).
#[derive(Debug)]
pub(crate) struct LifecycleClaim<'a> {
    roles: &'a ThreadRoles,
    _guard: MutexGuard<'a, ()>,
}

impl LifecycleClaim<'_> {
    /// Runs `f` as the audio thread: the claim's thread answers
    /// `is_audio_thread` (and not `is_main_thread`) for its span.
    pub(crate) fn audio<R>(&self, f: impl FnOnce() -> R) -> R {
        struct Clear<'r>(&'r ThreadRoles);
        impl Drop for Clear<'_> {
            fn drop(&mut self) {
                self.0.audio.store(NO_AUDIO_THREAD, Ordering::Release);
            }
        }
        self.roles.audio.store(
            thread_id_hash(std::thread::current().id()),
            Ordering::Release,
        );
        let _clear = Clear(self.roles);
        f()
    }
}

impl ThreadRoles {
    /// Takes the roles lock without becoming the audio thread: for the
    /// `[main-thread]` entries that must not overlap audio-side ones.
    pub(crate) fn claim_lifecycle(&self) -> LifecycleClaim<'_> {
        let guard = self.lock.lock_recover();
        LifecycleClaim {
            roles: self,
            _guard: guard,
        }
    }

    /// Roles with the calling thread as the main thread and no audio claim.
    /// The token proves the caller is on the process's main thread, so the
    /// thread recorded here is the one every `MainThreadMarker` entry runs on: this
    /// is the one place the main thread is named, and the `thread-check`
    /// answer, the host shims and `Plugin`'s drop all read it.
    #[must_use]
    pub fn new(_main: MainThreadMarker) -> Self {
        Self {
            main: std::thread::current().id(),
            audio: AtomicU64::new(NO_AUDIO_THREAD),
            lock: Mutex::new(()),
        }
    }

    /// The thread that built these roles.
    #[must_use]
    pub(crate) fn main_thread_id(&self) -> ThreadId {
        self.main
    }

    /// Claim the `[audio-thread]` role, waiting until any other claim has
    /// been released. A poisoned lock is taken as is: the guarded value is
    /// `()`, so a panicking claimant broke no invariant.
    pub(crate) fn claim(&self) -> AudioClaim<'_> {
        let guard = self.lock.lock_recover();
        // `Release` pairs with the `Acquire` in `is_audio_thread`.
        self.audio.store(
            thread_id_hash(std::thread::current().id()),
            Ordering::Release,
        );
        AudioClaim {
            roles: self,
            _guard: guard,
        }
    }

    /// Claims the `[audio-thread]` role if no other claim is held, and never
    /// waits: `None` while a claim is held, by the main thread's lifecycle
    /// and parameter calls or by another audio entry. What the audio thread
    /// takes, so a main-thread call never stalls a block. A poisoned lock is
    /// taken as is, as [`claim`](Self::claim) takes it.
    pub(crate) fn try_claim(&self) -> Option<AudioClaim<'_>> {
        let guard = match self.lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        // `Release` pairs with the `Acquire` in `is_audio_thread`.
        self.audio.store(
            thread_id_hash(std::thread::current().id()),
            Ordering::Release,
        );
        Some(AudioClaim {
            roles: self,
            _guard: guard,
        })
    }

    /// Whether the calling thread holds the audio claim.
    #[must_use]
    pub fn is_audio_thread(&self) -> bool {
        let claimed = self.audio.load(Ordering::Acquire);
        claimed != NO_AUDIO_THREAD && claimed == thread_id_hash(std::thread::current().id())
    }

    /// Whether the calling thread is the main thread and holds no audio
    /// claim. Exclusive with [`is_audio_thread`](Self::is_audio_thread), per
    /// thread: a claim held by another thread does not demote this one.
    #[must_use]
    pub fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main && !self.is_audio_thread()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    fn roles() -> Arc<ThreadRoles> {
        Arc::new(ThreadRoles::new(tutti_fixtures::main_thread()))
    }

    /// A thread that holds no claim is neither role but the one that built
    /// the roles is the main thread.
    /// Spec: clap.thread-check
    #[test]
    fn an_unclaimed_thread_is_not_the_audio_thread() {
        let roles = roles();
        assert!(roles.is_main_thread());
        assert!(!roles.is_audio_thread());
        let other = Arc::clone(&roles);
        let (main, audio) =
            std::thread::spawn(move || (other.is_main_thread(), other.is_audio_thread()))
                .join()
                .unwrap();
        assert!(!main && !audio);
    }

    /// `is_main_thread` and `is_audio_thread` are mutually exclusive: the main
    /// thread inside an audio claim is the audio thread alone, so a plugin's
    /// `!is_main_thread()` assertion there can fail.
    /// Spec: clap.thread-check
    #[test]
    fn a_claim_makes_its_thread_the_audio_thread_and_not_the_main_thread() {
        let roles = roles();
        {
            let _claim = roles.claim();
            assert!(roles.is_audio_thread());
            assert!(!roles.is_main_thread());
        }
        assert!(roles.is_main_thread());
        assert!(!roles.is_audio_thread());
    }

    /// The audio claim is mutual exclusion: no two threads are inside it at
    /// once, and each sees itself as the audio thread throughout.
    /// Spec: clap.params (`flush` is never concurrent with `process`)
    #[test]
    fn audio_claims_serialize_across_threads() {
        let roles = roles();
        let inside = Arc::new(AtomicU32::new(0));
        let overlapped = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (roles, inside, overlapped) = (
                    Arc::clone(&roles),
                    Arc::clone(&inside),
                    Arc::clone(&overlapped),
                );
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let _claim = roles.claim();
                        if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                            overlapped.store(true, Ordering::SeqCst);
                        }
                        assert!(roles.is_audio_thread());
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(!overlapped.load(Ordering::SeqCst));
        assert!(!roles.is_audio_thread());
    }

    /// `try_claim` never waits: it is `None` while another claim is held.
    #[test]
    fn a_try_claim_over_a_held_claim_is_refused() {
        let roles = roles();
        let held = roles.claim();
        let other = Arc::clone(&roles);
        assert!(
            std::thread::spawn(move || other.try_claim().is_none())
                .join()
                .unwrap()
        );
        drop(held);
        assert!(roles.try_claim().is_some());
    }

    /// Claiming repeatedly from one thread does not allocate.
    #[test]
    fn repeated_claims_from_one_thread_do_not_allocate() {
        let roles = roles();
        for _ in 0..8 {
            drop(roles.claim());
        }
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1024 {
                let claim = roles.claim();
                assert!(roles.is_audio_thread());
                drop(claim);
            }
        });
    }

    /// An audio thread claiming every block while a second thread claims in
    /// between does not allocate: a one-slot cache of the claimant would miss
    /// on every block, a published hash has nothing to evict.
    #[test]
    fn claims_interleaved_with_another_thread_do_not_allocate() {
        use std::sync::Barrier;
        const WARMUP: usize = 8;
        const ROUNDS: usize = 256;
        let roles = roles();
        // Two barriers make the threads strictly alternate; the counts match by
        // construction, so neither side ever parks alone.
        let before = Arc::new(Barrier::new(2));
        let after = Arc::new(Barrier::new(2));
        let interloper = {
            let (roles, before, after) =
                (Arc::clone(&roles), Arc::clone(&before), Arc::clone(&after));
            std::thread::spawn(move || {
                for _ in 0..WARMUP + ROUNDS {
                    drop(roles.claim());
                    before.wait();
                    after.wait();
                }
            })
        };
        for _ in 0..WARMUP {
            before.wait();
            drop(roles.claim());
            after.wait();
        }
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..ROUNDS {
                before.wait();
                let claim = roles.claim();
                assert!(roles.is_audio_thread());
                drop(claim);
                after.wait();
            }
        });
        interloper.join().unwrap();
    }
}
