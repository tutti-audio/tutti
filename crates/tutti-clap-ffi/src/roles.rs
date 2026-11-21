//! Which thread plays the CLAP `[main-thread]` and `[audio-thread]` roles.
//!
//! A plugin asks the host (`clap.thread-check`) which thread a call is on,
//! and the audio-side entries of one plugin must never overlap. Both are one
//! object's job: the claim on the `[audio-thread]` role is a [`Claim`], which
//! the audio thread takes without locking, waiting or allocating, and holding
//! it publishes the claiming thread's identity.

use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::ThreadId;
use tutti_rt::{Claim, ClaimGuard};

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
    claim: Claim,
}

const NO_AUDIO_THREAD: u64 = 0;

/// Why [`ThreadRoles::try_claim`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Another claim is held; a later try can succeed.
    Held,
    /// A holder panicked, which kills the claim: no audio-side entry runs on
    /// this instance again.
    Dead,
}

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
/// While alive it holds the instance's role claim and has published the
/// claiming thread, so [`ThreadRoles::is_audio_thread`] answers `true` on
/// this thread and `false` everywhere else, and
/// [`ThreadRoles::is_main_thread`] answers `false` on this thread even when it
/// is the OS main thread (the spec lets a host drive `start_processing` from
/// there). Not re-entrant: claiming twice on one thread never returns.
///
/// It holds no guard once the claim is dead (a holder panicked): a dead
/// claim admits no audio-side entry again, so the only entries left are the
/// main-thread ones, which run on the one main thread and so one at a time.
#[derive(Debug)]
pub(crate) struct AudioClaim<'a> {
    roles: &'a ThreadRoles,
    // Held for the claim's lifetime; declared last so it releases only after
    // `Drop` has cleared the published identity.
    _guard: Option<ClaimGuard<'a>>,
}

impl Drop for AudioClaim<'_> {
    fn drop(&mut self) {
        // One store of a `u64`: nothing to free, so this cannot deallocate on
        // the audio thread. `Release` pairs with the `Acquire` in
        // `is_audio_thread`.
        self.roles.audio.store(NO_AUDIO_THREAD, Ordering::Release);
    }
}

/// The roles claim held for `deactivate` and `destroy`, which are
/// `[main-thread]` entries: no audio-side entry can run beside them, but the
/// holder stays the main thread, so the callbacks a plugin makes from them
/// (`unregister_timer`, `latency.changed`, `params.rescan`, ...) are legal and
/// answered as such. The one audio-side entry in the sequence, `stop_processing`,
/// runs inside [`audio`](Self::audio). Like an [`AudioClaim`], it holds no
/// guard once the claim is dead.
#[derive(Debug)]
pub(crate) struct LifecycleClaim<'a> {
    roles: &'a ThreadRoles,
    _guard: Option<ClaimGuard<'a>>,
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
    /// Takes the roles claim without becoming the audio thread, waiting for
    /// any other holder: for the `[main-thread]` entries that must not overlap
    /// audio-side ones.
    pub(crate) fn claim_lifecycle(&self) -> LifecycleClaim<'_> {
        LifecycleClaim {
            roles: self,
            _guard: self.claim.claim(),
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
            claim: Claim::new(),
        }
    }

    /// The thread that built these roles.
    #[must_use]
    pub(crate) fn main_thread_id(&self) -> ThreadId {
        self.main
    }

    /// Claim the `[audio-thread]` role, waiting until any other claim has
    /// been released. Control side.
    pub(crate) fn claim(&self) -> AudioClaim<'_> {
        let guard = self.claim.claim();
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
    /// waits: [`Refused::Held`] while a claim is held, by the main thread's
    /// lifecycle and parameter calls or by another audio entry, and
    /// [`Refused::Dead`] for good once the claim is dead. What the audio
    /// thread takes: one compare-exchange, so a main-thread call never stalls
    /// a block and a block never locks or allocates.
    pub(crate) fn try_claim(&self) -> Result<AudioClaim<'_>, Refused> {
        let guard = self.claim.try_claim().ok_or_else(|| {
            if self.claim.is_dead() {
                Refused::Dead
            } else {
                Refused::Held
            }
        })?;
        // `Release` pairs with the `Acquire` in `is_audio_thread`.
        self.audio.store(
            thread_id_hash(std::thread::current().id()),
            Ordering::Release,
        );
        Ok(AudioClaim {
            roles: self,
            _guard: Some(guard),
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
    /// Spec: clap.ext.thread_check.host.is_main_thread, clap.ext.thread_check.host.is_audio_thread
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
    /// Spec: clap.ext.thread_check.host.is_main_thread, clap.ext.thread_check.host.is_audio_thread
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
    /// Spec: clap.ext.params.flush, clap.plugin.process
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
            std::thread::spawn(move || other.try_claim().err() == Some(Refused::Held))
                .join()
                .unwrap()
        );
        drop(held);
        assert!(roles.try_claim().is_ok());
    }

    /// A panic while the claim is held ends the instance for the audio side:
    /// every later audio claim is refused as dead, never as held, so a block
    /// can name the instance as gone rather than momentarily busy.
    #[test]
    fn a_panic_under_a_claim_refuses_every_later_audio_claim_as_dead() {
        let roles = roles();
        assert_eq!(roles.try_claim().err(), None);
        let panicked = {
            let roles = Arc::clone(&roles);
            std::thread::spawn(move || {
                let _claim = roles.claim();
                panic!("host code panics under the claim");
            })
            .join()
        };
        assert!(panicked.is_err(), "the holder panicked");
        for _ in 0..3 {
            assert_eq!(roles.try_claim().err(), Some(Refused::Dead));
        }
    }

    /// Claiming allocates nothing from the first claim of fresh roles on: an
    /// audio claim is one compare-exchange, with no lock to set up on first
    /// use.
    #[test]
    fn claims_allocate_nothing_from_the_first() {
        let roles = roles();
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1024 {
                let claim = roles.try_claim().expect("nothing else holds it");
                assert!(roles.is_audio_thread());
                drop(claim);
                drop(roles.claim());
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
