//! LRU disk cache for audio files.
//!
//! Bounded cache with least-recently-used eviction.
//!
//! The *read* half (`get`, `pin`) is unconditional, but the sole writer is the
//! decode arm of `decode::load_wave`, which needs `read_wave` and so is
//! gated on the codec features. A build with none of them on therefore reaches
//! `insert` from nowhere, and `over_budget` / `evict_lru` / the budget fields
//! die with it — a feature-matrix artifact, not rot. CI builds that config, so
//! the allow is scoped to it rather than blanket, keeping real dead-code
//! detection live everywhere a codec is on.
#![cfg_attr(not(any(codec, test)), allow(dead_code))]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, MutexGuard};
use tutti_buffers::{Float, Wave};
use tutti_platform::Identity;
use tutti_rt::Mutex;

/// Bounded LRU cache with entry count and byte limits.
///
/// Two kinds of thread touch it: the thread that adopts a stream (a pin while
/// the stream lives) and the thread that reads a file for an open (a lookup,
/// then an insert of the wave it decoded). Neither is the audio thread, and neither holds the
/// lock for more than a map operation: a wave an insert evicts is dropped after
/// the lock is released. The control side never reads it (a stream's file is
/// its own [`StreamFile`](super::control::StreamFile)), so a plain map behind
/// one lock is all it takes.
///
/// Entries are keyed by [`Identity`], taken from an asset's
/// [`identity`](tutti_platform::Asset::identity): a file's carries its stamp, so a
/// file rewritten since is another key and misses. An asset whose identity is
/// `None` has no key: it is never cached, and is decoded each time it is opened.
pub struct LruCache<F: Float> {
    inner: Mutex<Inner<F>>,
    max_entries: usize,
    max_bytes: u64,
}

struct Inner<F: Float> {
    entries: HashMap<Identity, CacheEntry<F>>,
    current_bytes: u64,
    /// Monotonic access counter — the source of every `last_access` stamp.
    ///
    /// "Least recently used" is a statement about access *order*, so the stamp
    /// is a sequence number rather than a reading of a clock. A tick increments
    /// per touch, so no two accesses can tie.
    ///
    /// `u64` cannot realistically wrap: at one touch per nanosecond it takes
    /// ~584 years.
    access_tick: u64,
}

impl<F: Float> Inner<F> {
    /// The next access stamp, consuming a tick.
    fn next_tick(&mut self) -> u64 {
        let tick = self.access_tick;
        self.access_tick += 1;
        tick
    }
}

struct CacheEntry<F: Float> {
    wave: Arc<Wave<F>>,
    /// The value of [`Inner::access_tick`] at this entry's last touch.
    /// Ordering-only — the number has no meaning beyond comparing against
    /// another entry's.
    last_access: u64,
    size_bytes: u64,
    /// Active stream count. Non-zero while a stream is reading this wave; the
    /// LRU refuses to evict a pinned entry (a fully-buffered stream would go
    /// cold and get evicted out from under its reader otherwise).
    pins: Arc<AtomicU32>,
}

/// RAII guard that keeps a cache entry pinned (unevictable) for the lifetime of
/// a stream. Acquired by [`LruCache::pin`], released on `Drop`. Held inside the
/// streamer's stream link so it lives exactly as long as the stream.
pub struct StreamPin {
    /// The counter of the entry this pin incremented, not the entry that
    /// happens to sit under the key when it drops. `None` when there was no
    /// entry to pin.
    pins: Option<Arc<AtomicU32>>,
}

impl Drop for StreamPin {
    fn drop(&mut self) {
        if let Some(pins) = &self.pins {
            pins.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// The bytes a resident `wave` holds: what the cache's budget and the read
/// meter count.
pub(super) fn wave_bytes<F: Float>(wave: &Wave<F>) -> u64 {
    (wave.len().get() * wave.channels() * std::mem::size_of::<F>()) as u64
}

impl<F: Float> LruCache<F> {
    /// An empty cache bounded by `max_entries` resident waves and `max_bytes`
    /// total.
    ///
    /// Both bounds are targets, not guarantees: an insert that can find no
    /// unpinned victim is admitted over-budget rather than evicting a wave a
    /// live stream is still reading.
    pub fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                current_bytes: 0,
                access_tick: 0,
            }),
            max_entries,
            max_bytes,
        }
    }

    /// The map, locked. Poisoning is ignored: nothing runs under the lock that
    /// can leave the map half-changed.
    fn lock(&self) -> MutexGuard<'_, Inner<F>> {
        self.inner.lock()
    }

    /// The resident wave for `key`, if any, marking it most-recently-used.
    ///
    /// `None` is a plain miss — every caller treats it as "skip this region",
    /// never as an error.
    pub fn get(&self, key: &Identity) -> Option<Arc<Wave<F>>> {
        let mut inner = self.lock();
        let tick = inner.next_tick();
        inner.entries.get_mut(key).map(|entry| {
            entry.last_access = tick;
            Arc::clone(&entry.wave)
        })
    }

    /// Admit `wave` under `key`, evicting least-recently-used unpinned
    /// entries until it fits.
    ///
    /// Re-inserting a resident key only refreshes its access time — the bytes
    /// are not counted twice. When nothing evictable remains the wave is
    /// admitted over budget, which is the deliberate trade: exceeding a byte
    /// target is recoverable, pulling a wave out from under a playing stream is
    /// not.
    ///
    /// A wave larger than the whole byte budget is refused, and evicts
    /// nothing: admitting it would empty the cache for one file and still
    /// leave it over budget. Its holder keeps it (a stream that cannot seek
    /// holds its file on its writer, as its `Source::Whole`).
    pub fn insert(&self, key: Identity, wave: Arc<Wave<F>>) {
        let size = wave_bytes(&wave);
        if size > self.max_bytes {
            return;
        }

        // Dropped after the lock: a wave can be the last holder of a large
        // buffer.
        let mut evicted = Vec::new();
        {
            let mut inner = self.lock();
            let tick = inner.next_tick();
            if let Some(existing) = inner.entries.get_mut(&key) {
                existing.last_access = tick;
                return;
            }

            while self.over_budget(&inner, size) {
                match Self::evict_lru(&mut inner) {
                    Some(entry) => evicted.push(entry),
                    None => break,
                }
            }

            inner.entries.insert(
                key,
                CacheEntry {
                    wave,
                    last_access: tick,
                    size_bytes: size,
                    pins: Arc::new(AtomicU32::new(0)),
                },
            );
            inner.current_bytes += size;
        }
        drop(evicted);
    }

    /// Pin the entry at `key` so the LRU will not evict it while the returned
    /// [`StreamPin`] is alive. Held by an active stream (in its `Record`)
    /// so a fully-buffered — hence cold — stream is never evicted mid-read.
    ///
    /// If the entry is absent (never admitted: an incremental stream) the
    /// guard is inert. The guard holds the entry's own counter, so its drop
    /// releases exactly what it took, whatever is under the key by then. No key (an asset that is never cached) pins nothing.
    pub fn pin(&self, key: Option<&Identity>) -> StreamPin {
        let pins = key.and_then(|key| {
            let inner = self.lock();
            let entry = inner.entries.get(key)?;
            entry.pins.fetch_add(1, Ordering::Relaxed);
            Some(Arc::clone(&entry.pins))
        });
        StreamPin { pins }
    }

    /// Whether inserting `incoming` bytes would exceed the entry-count or
    /// byte budget (byte check ignores an empty cache so a single oversized
    /// wave can still be admitted).
    fn over_budget(&self, inner: &Inner<F>, incoming: u64) -> bool {
        inner.entries.len() >= self.max_entries
            || (inner.current_bytes + incoming > self.max_bytes && !inner.entries.is_empty())
    }

    /// Removes the least recently used unpinned entry and hands it back.
    fn evict_lru(inner: &mut Inner<F>) -> Option<CacheEntry<F>> {
        // Only unpinned entries are eviction candidates; a pinned entry backs an
        // active stream and must stay resident. If every remaining entry is
        // pinned this returns `None`, `insert`'s loop breaks, and the wave is
        // admitted over-budget — the same escape the empty-cache case uses.
        let oldest = inner
            .entries
            .iter()
            .filter(|(_, entry)| entry.pins.load(Ordering::Relaxed) == 0)
            .min_by_key(|(_, entry)| entry.last_access)
            .map(|(key, _)| key.clone())?;
        let entry = inner.entries.remove(&oldest)?;
        inner.current_bytes -= entry.size_bytes;
        Some(entry)
    }

    /// Number of cached entries. Test observability for eviction behavior.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Total cached bytes. Test observability for byte-budget eviction.
    #[cfg(test)]
    pub fn byte_len(&self) -> u64 {
        self.lock().current_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::Mono;
    use tutti_units::SampleRate;

    fn make_wave(samples: usize) -> Arc<Wave> {
        let data = vec![0.0f32; samples];
        Arc::new(Wave::from_mono(SampleRate::SR_44K1, Mono::new(&data)))
    }

    /// A wave larger than the whole budget is refused, and evicts nothing.
    #[test]
    fn a_wave_larger_than_the_budget_is_refused() {
        // Two-channel waves of 100 frames: 800 bytes each.
        let cache = LruCache::new(10, 1_000);
        let (small, huge) = (Identity::minted(), Identity::minted());
        cache.insert(small.clone(), make_wave(100));
        cache.insert(huge.clone(), make_wave(1_000));
        assert!(cache.get(&huge).is_none(), "admitted");
        assert!(cache.get(&small).is_some(), "evicted for it");
    }

    /// A pin released releases exactly what it took: a pin on an absent path
    /// (an incremental stream) holds nothing, so dropping it must not unpin an
    /// entry that was inserted for the same path afterwards.
    #[test]
    fn a_pin_releases_only_what_it_took() {
        // One entry fits: any insert of another path must evict an unpinned one.
        let cache = Arc::new(LruCache::new(1, 1024 * 1024));
        let path = Identity::minted();
        let absent = cache.pin(Some(&path));
        cache.insert(path.clone(), make_wave(100));
        let live = cache.pin(Some(&path));
        drop(absent);
        cache.insert(Identity::minted(), make_wave(100));
        assert!(
            cache.get(&path).is_some(),
            "a pinned entry was evicted: another pin's drop unpinned it"
        );
        drop(live);
    }

    #[test]
    fn test_cache_insert_and_get() {
        let cache = LruCache::new(10, 1024 * 1024);
        let path = Identity::minted();
        let wave = make_wave(100);

        assert!(cache.get(&path).is_none());
        cache.insert(path.clone(), wave);

        let retrieved = cache.get(&path).expect("just inserted");
        assert_eq!(retrieved.len().get(), 100);
    }

    /// Two accesses with no delay between them still order — the property a
    /// wall clock could not give.
    #[test]
    fn same_instant_accesses_still_order_deterministically() {
        // Budget for two of three waves, forcing one eviction on the third.
        let cache = LruCache::new(2, u64::MAX);
        let a = Identity::minted();
        let b = Identity::minted();
        let c = Identity::minted();

        cache.insert(a.clone(), make_wave(10));
        cache.insert(b.clone(), make_wave(10));
        cache.insert(c.clone(), make_wave(10));

        // `a` was touched first, so `a` is the victim — every time.
        assert!(cache.get(&a).is_none(), "first-touched entry is evicted");
        assert!(cache.get(&b).is_some());
        assert!(cache.get(&c).is_some());
        assert_eq!(cache.len(), 2);

        // And it is not luck: the same sequence gives the same verdict on
        // every repetition. A tie broken by hash order would vary across
        // these, since each cache is a fresh map.
        for _ in 0..64 {
            let cache = LruCache::new(2, u64::MAX);
            cache.insert(a.clone(), make_wave(10));
            cache.insert(b.clone(), make_wave(10));
            cache.insert(c.clone(), make_wave(10));
            assert!(
                cache.get(&a).is_none() && cache.get(&b).is_some() && cache.get(&c).is_some(),
                "the victim must be a on every run, not an arbitrary entry"
            );
        }
    }

    /// Eviction follows *access* order, not insertion order: a `get` on the
    /// oldest entry promotes it, and the next-oldest becomes the victim.
    #[test]
    fn eviction_follows_access_order_not_insertion_order() {
        let cache = LruCache::new(2, u64::MAX);
        let a = Identity::minted();
        let b = Identity::minted();
        let c = Identity::minted();

        cache.insert(a.clone(), make_wave(10));
        cache.insert(b.clone(), make_wave(10));

        // Touch `a`. It was inserted first, but it is now the most recently
        // *used*, so `b` inherits the victim slot.
        assert!(cache.get(&a).is_some(), "touch promotes a");

        cache.insert(c.clone(), make_wave(10));

        assert!(
            cache.get(&a).is_some(),
            "promoted by the get, so it survives"
        );
        assert!(cache.get(&b).is_none(), "least recently used is evicted");
        assert!(cache.get(&c).is_some());
    }

    /// Re-inserting a resident path makes it the most recently used.
    #[test]
    fn reinsert_of_a_resident_path_promotes_it() {
        let cache = LruCache::new(2, u64::MAX);
        let a = Identity::minted();
        let b = Identity::minted();
        let c = Identity::minted();

        cache.insert(a.clone(), make_wave(10));
        cache.insert(b.clone(), make_wave(10));
        cache.insert(a.clone(), make_wave(10));

        cache.insert(c.clone(), make_wave(10));

        assert!(
            cache.get(&a).is_some(),
            "re-insert made a the most recently used"
        );
        assert!(cache.get(&b).is_none(), "b is now least recently used");
    }

    #[test]
    fn test_cache_max_entries_eviction() {
        let cache = LruCache::new(3, u64::MAX);

        for _ in 0..3 {
            cache.insert(Identity::minted(), make_wave(10));
        }
        assert_eq!(cache.len(), 3);

        let path4 = Identity::minted();
        cache.insert(path4.clone(), make_wave(10));

        assert_eq!(cache.len(), 3);
        assert!(cache.get(&path4).is_some());
    }

    #[test]
    fn test_cache_max_bytes_eviction() {
        let cache = LruCache::new(100, 1000);

        cache.insert(Identity::minted(), make_wave(100));
        cache.insert(Identity::minted(), make_wave(100));
        assert_eq!(cache.len(), 2);

        cache.insert(Identity::minted(), make_wave(100));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_cache_byte_accounting() {
        let cache = LruCache::new(10, 10000);
        cache.insert(Identity::minted(), make_wave(100));
        cache.insert(Identity::minted(), make_wave(200));

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.byte_len(), 1200);
    }

    #[test]
    fn test_pin_prevents_eviction_of_lru_victim() {
        // Byte budget fits exactly two waves of 100 samples (400 bytes each).
        let cache = Arc::new(LruCache::new(100, 900));
        let a = Identity::minted();
        let b = Identity::minted();
        let c = Identity::minted();

        // `a` is inserted first, so it is the LRU victim. Pin it: an active
        // stream is reading it.
        cache.insert(a.clone(), make_wave(100));
        let _pin = cache.pin(Some(&a));
        cache.insert(b.clone(), make_wave(100));

        // Inserting `c` would evict the LRU (`a`), but `a` is pinned. The evictor
        // must skip it and take `b` instead (next-oldest, unpinned).
        cache.insert(c.clone(), make_wave(100));

        assert!(cache.get(&a).is_some(), "pinned LRU victim must survive");
        assert!(cache.get(&c).is_some(), "new entry admitted");
        assert!(
            cache.get(&b).is_none(),
            "unpinned next-oldest evicted instead"
        );
    }

    #[test]
    fn test_all_pinned_admits_over_budget() {
        let cache = Arc::new(LruCache::new(100, 900));
        let a = Identity::minted();
        let b = Identity::minted();
        let c = Identity::minted();

        cache.insert(a.clone(), make_wave(100));
        let _pa = cache.pin(Some(&a));
        cache.insert(b.clone(), make_wave(100));
        let _pb = cache.pin(Some(&b));

        // Both resident entries are pinned; the newcomer can't evict either, so
        // it is admitted over-budget rather than dropping an active stream.
        cache.insert(c.clone(), make_wave(100));

        assert!(cache.get(&a).is_some());
        assert!(cache.get(&b).is_some());
        assert!(cache.get(&c).is_some(), "over-budget admit when all pinned");
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn test_unpin_on_drop_restores_evictability() {
        // Budget holds exactly one wave (400 bytes); the second insert must evict
        // the first — unless it's pinned. This makes the eviction target
        // unambiguous (no LRU tie between same-millisecond entries).
        let cache = Arc::new(LruCache::new(100, 400));
        let a = Identity::minted();
        let b = Identity::minted();

        cache.insert(a.clone(), make_wave(100));

        // While pinned, inserting `b` cannot evict `a`; it is admitted
        // over-budget instead (the all-pinned escape). Keep `b` pinned too so
        // that once `a`'s pin drops, `a` is the *only* eviction candidate — no
        // LRU tie between same-millisecond entries.
        let pin_a = cache.pin(Some(&a));
        cache.insert(b.clone(), make_wave(100));
        let _pin_b = cache.pin(Some(&b));
        assert!(
            cache.get(&a).is_some(),
            "pinned `a` survives the over-budget insert"
        );
        drop(pin_a);

        // With `a`'s pin gone it is the sole unpinned entry: the next over-budget
        // insert reclaims it (`b` stays put, pinned).
        let c = Identity::minted();
        cache.insert(c.clone(), make_wave(100));
        assert!(cache.get(&a).is_none(), "dropped pin → LRU can evict `a`");
        assert!(cache.get(&b).is_some(), "still-pinned `b` untouched");
    }

    #[test]
    fn test_duplicate_insert_no_double_count() {
        let cache = LruCache::new(10, 10000);
        let path = Identity::minted();

        cache.insert(path.clone(), make_wave(100));
        let bytes_after_first = cache.byte_len();

        cache.insert(path, make_wave(100));
        let bytes_after_second = cache.byte_len();

        assert_eq!(bytes_after_first, bytes_after_second);
        assert_eq!(cache.len(), 1);
    }
}
