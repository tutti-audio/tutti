//! [`Watcher`]: the changes of a set of targets, polled.
//!
//! Every `set` that moves a base tells one feed what landed, under the value
//! lock. A watcher holds one slot the feed fills: for each target it watches,
//! the newest change since the watcher last read. The feed holds its slots
//! weakly, so dropping a watcher ends the watching.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Weak};

use tutti_rt::Mutex;

use super::batch::Failure;
use super::error::{ValueError, ValueErrorKind};
use super::state::{Params, ValueState};
use super::target::{
    Target, TargetId,
    sealed::{Sealed, Store},
};
use crate::AppKey;

/// What one gesture is: a batch that moved two or more targets, which an app
/// undoes as one step. Two changes with equal ids are of one batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GestureId(u64);

/// Who made a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChangedBy {
    /// The app: a [`Session::set`](crate::Session::set) or a
    /// [`Values::set`](crate::Values::set).
    App,
}

/// A target a [`Watcher`] can follow: every target except a plugin's
/// parameter, which a plugin reports through its own handle. Sealed.
///
/// ```
/// # fn watchable<T: tutti_session::Watchable>() {}
/// watchable::<tutti_session::TempoTarget>();
/// ```
#[cfg_attr(
    feature = "plugin",
    doc = "
A plugin's parameter is not watchable:

```compile_fail
# fn watchable<T: tutti_session::Watchable>() {}
watchable::<tutti_session::PluginParamTarget>();
```"
)]
pub trait Watchable: Target {}

/// The newest change of one target since the last read.
pub struct Change<T: Target> {
    /// The base after the change.
    pub value: T::Value,
    /// Who made it.
    pub by: ChangedBy,
    /// The gesture the change belongs to: `None` for a call of one write.
    pub gesture: Option<GestureState<T>>,
}

/// The gesture a [`Change`] belongs to.
pub struct GestureState<T: Target> {
    /// The gesture, shared by every change of its batch.
    pub id: GestureId,
    /// The base before the first change since the last read: what undoing restores.
    pub base: T::Value,
    /// Whether the gesture is over: a batch ends with its call.
    pub ended: bool,
}

impl<T: Target> Clone for GestureState<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            base: self.base.clone(),
            ended: self.ended,
        }
    }
}

impl<T: Target> fmt::Debug for GestureState<T>
where
    T::Value: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GestureState")
            .field("id", &self.id)
            .field("base", &self.base)
            .field("ended", &self.ended)
            .finish()
    }
}

impl<T: Target> PartialEq for GestureState<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.base == other.base && self.ended == other.ended
    }
}

impl<T: Target> PartialEq for Change<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value && self.by == other.by && self.gesture == other.gesture
    }
}

impl<T: Target> Clone for Change<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            by: self.by,
            gesture: self.gesture.clone(),
        }
    }
}

impl<T: Target> fmt::Debug for Change<T>
where
    T::Value: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Change")
            .field("value", &self.value)
            .field("by", &self.by)
            .field("gesture", &self.gesture)
            .finish()
    }
}

type Erased = Arc<dyn Any + Send + Sync>;

/// One target's base moved: before and after, erased.
pub(crate) struct StoredChange {
    id: TargetId,
    base: Erased,
    value: Erased,
}

impl StoredChange {
    pub(super) fn new<V: Send + Sync + 'static>(id: TargetId, base: V, value: V) -> Self {
        Self {
            id,
            base: Arc::new(base),
            value: Arc::new(value),
        }
    }
}

/// What a batch changed: the targets whose base moved.
pub(crate) struct Stored {
    landings: Vec<StoredChange>,
}

impl Stored {
    pub(super) fn new(landings: Vec<StoredChange>) -> Self {
        Self { landings }
    }
}

/// What a watcher keeps of a target since it last read: the newest value, and
/// the base the first change since then started from.
pub struct Record {
    by: ChangedBy,
    value: Erased,
    base: Erased,
    gesture: Option<Gestured>,
}

/// The gesture of the newest change.
struct Gestured {
    id: GestureId,
    ended: bool,
}

type Drained = BTreeMap<TargetId, Record>;

macro_rules! opaque_debug {
    ($($name:ident),+) => {$(
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        }
    )+};
}
opaque_debug!(StoredChange, Stored, Record);

/// A watcher's side of the feed: what it watches, and the changes the feed has
/// recorded for it since it last read.
pub(crate) struct WatchSlot {
    ids: Vec<TargetId>,
    pending: Mutex<BTreeMap<TargetId, Record>>,
}

impl WatchSlot {
    fn take(&self) -> Drained {
        std::mem::take(&mut *self.pending.lock())
    }
}

/// The one internal feed of changes: every landing, to every live watcher.
/// Behind the value lock: a call lands under it, and the feed's own lock is
/// taken after it, never before.
#[derive(Default)]
pub(crate) struct Feed {
    inner: Mutex<FeedInner>,
}

#[derive(Default)]
struct FeedInner {
    /// The next gesture id.
    next: u64,
    watchers: Vec<Weak<WatchSlot>>,
}

impl fmt::Debug for Feed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Feed").finish_non_exhaustive()
    }
}

impl Feed {
    /// A slot that records the changes of `ids`.
    fn subscribe(&self, ids: Vec<TargetId>) -> Arc<WatchSlot> {
        let slot = Arc::new(WatchSlot {
            ids,
            pending: Mutex::new(BTreeMap::new()),
        });
        self.inner.lock().watchers.push(Arc::downgrade(&slot));
        slot
    }

    fn unsubscribe(&self, slot: &Arc<WatchSlot>) {
        let weak = Arc::downgrade(slot);
        self.inner.lock().watchers.retain(|w| !w.ptr_eq(&weak));
    }

    /// Tells every watcher what `landed`. A batch that moved two or more
    /// targets is one gesture, which ends with the call. A watcher that has not
    /// read since an earlier change keeps that change's base.
    pub(super) fn land(&self, landed: Stored) {
        if landed.landings.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        let gesture = (landed.landings.len() >= 2).then(|| {
            let id = GestureId(inner.next);
            inner.next += 1;
            id
        });
        inner.watchers.retain(|w| w.strong_count() > 0);
        for slot in inner.watchers.iter().filter_map(Weak::upgrade) {
            let mut pending = slot.pending.lock();
            for landing in &landed.landings {
                if slot.ids.contains(&landing.id) {
                    let base = pending
                        .get(&landing.id)
                        .map_or(&landing.base, |earlier| &earlier.base);
                    let record = Record {
                        by: ChangedBy::App,
                        value: Arc::clone(&landing.value),
                        base: Arc::clone(base),
                        gesture: gesture.map(|id| Gestured { id, ended: true }),
                    };
                    pending.insert(landing.id, record);
                }
            }
        }
    }

    /// How many watchers are listening.
    #[cfg(test)]
    pub(super) fn watching(&self) -> usize {
        self.inner
            .lock()
            .watchers
            .iter()
            .filter(|w| w.strong_count() > 0)
            .count()
    }

    /// The id the next gesture takes.
    #[cfg(test)]
    pub(super) fn next_gesture(&self) -> GestureId {
        GestureId(self.inner.lock().next)
    }
}

/// The sealed half of [`IntoTargets`].
mod seal {
    use super::*;

    pub trait Sealed {
        /// The changes of the targets, in the argument's shape.
        type Changes;

        /// Checks every target is there and lists its id; `at` counts items.
        fn register(
            &self,
            params: &Params,
            at: &mut usize,
            ids: &mut Vec<TargetId>,
        ) -> Result<(), Failure>;

        /// Whether any target is still there.
        fn any_live(&self, params: &Params) -> bool;

        /// Each target's change in `drained`.
        fn changes(&self, drained: &Drained) -> Self::Changes;
    }
}
use seal::Sealed as Targets;

/// What [`Session::watch`](crate::Session::watch) takes: a target (every one
/// but a plugin's parameter), or tuples of up to 12 of them (nested too),
/// arrays, `Vec`s and `Option`s (`None` watches nothing but keeps its
/// position). It answers in the argument's shape. Sealed.
pub trait IntoTargets: Targets + Send {}

impl<T: Watchable + Store> Targets for T {
    type Changes = Option<Change<T>>;

    fn register(
        &self,
        params: &Params,
        at: &mut usize,
        ids: &mut Vec<TargetId>,
    ) -> Result<(), Failure> {
        let id = Sealed::id(*self);
        if (*self).resolve(params).is_none() {
            let kind = (*self)
                .unresolved(params)
                .map_or(ValueErrorKind::NoSuchTarget, ValueErrorKind::Refused);
            return Err(Failure {
                at: *at,
                target: Some(id),
                kind,
            });
        }
        ids.push(id);
        *at += 1;
        Ok(())
    }

    fn any_live(&self, params: &Params) -> bool {
        (*self).resolve(params).is_some()
    }

    fn changes(&self, drained: &Drained) -> Option<Change<T>> {
        const ONE_TYPE: &str = "one target id names one value type";
        let record = drained.get(&Sealed::id(*self))?;
        Some(Change {
            value: record
                .value
                .downcast_ref::<T::Value>()
                .expect(ONE_TYPE)
                .clone(),
            by: record.by,
            gesture: record.gesture.as_ref().map(|g| GestureState {
                id: g.id,
                base: record
                    .base
                    .downcast_ref::<T::Value>()
                    .expect(ONE_TYPE)
                    .clone(),
                ended: g.ended,
            }),
        })
    }
}
impl<T: Watchable + Store> IntoTargets for T {}

impl<A: Targets> Targets for Option<A> {
    type Changes = Option<A::Changes>;

    fn register(
        &self,
        params: &Params,
        at: &mut usize,
        ids: &mut Vec<TargetId>,
    ) -> Result<(), Failure> {
        match self {
            Some(item) => item.register(params, at, ids),
            None => {
                *at += 1;
                Ok(())
            }
        }
    }

    fn any_live(&self, params: &Params) -> bool {
        self.as_ref().is_some_and(|item| item.any_live(params))
    }

    fn changes(&self, drained: &Drained) -> Self::Changes {
        self.as_ref().map(|item| item.changes(drained))
    }
}
impl<A: IntoTargets> IntoTargets for Option<A> {}

impl<A: Targets> Targets for Vec<A> {
    type Changes = Vec<A::Changes>;

    fn register(
        &self,
        params: &Params,
        at: &mut usize,
        ids: &mut Vec<TargetId>,
    ) -> Result<(), Failure> {
        self.iter()
            .try_for_each(|item| item.register(params, at, ids))
    }

    fn any_live(&self, params: &Params) -> bool {
        self.iter().any(|item| item.any_live(params))
    }

    fn changes(&self, drained: &Drained) -> Self::Changes {
        self.iter().map(|item| item.changes(drained)).collect()
    }
}
impl<A: IntoTargets> IntoTargets for Vec<A> {}

impl<A: Targets, const N: usize> Targets for [A; N] {
    type Changes = [A::Changes; N];

    fn register(
        &self,
        params: &Params,
        at: &mut usize,
        ids: &mut Vec<TargetId>,
    ) -> Result<(), Failure> {
        self.iter()
            .try_for_each(|item| item.register(params, at, ids))
    }

    fn any_live(&self, params: &Params) -> bool {
        self.iter().any(|item| item.any_live(params))
    }

    fn changes(&self, drained: &Drained) -> Self::Changes {
        self.each_ref().map(|item| item.changes(drained))
    }
}
impl<A: IntoTargets, const N: usize> IntoTargets for [A; N] {}

macro_rules! tuples {
    ($(($($name:ident $at:tt),+))+) => {$(
        impl<$($name: Targets),+> Targets for ($($name,)+) {
            type Changes = ($($name::Changes,)+);

            fn register(
                &self,
                params: &Params,
                at: &mut usize,
                ids: &mut Vec<TargetId>,
            ) -> Result<(), Failure> {
                $(self.$at.register(params, at, ids)?;)+
                Ok(())
            }

            fn any_live(&self, params: &Params) -> bool {
                false $(|| self.$at.any_live(params))+
            }

            fn changes(&self, drained: &Drained) -> Self::Changes {
                ($(self.$at.changes(drained),)+)
            }
        }
        impl<$($name: IntoTargets),+> IntoTargets for ($($name,)+) {}
    )+};
}

tuples! {
    (A 0)
    (A 0, B 1)
    (A 0, B 1, C 2)
    (A 0, B 1, C 2, D 3)
    (A 0, B 1, C 2, D 3, E 4)
    (A 0, B 1, C 2, D 3, E 4, F 5)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K10 10)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K10 10, L 11)
}

/// The changes of a set of targets, since the last read. Owned and `Send`, not
/// `Clone`; dropping it stops the watching.
///
/// A watcher is polled: nothing wakes it. It reads the value lock, so a call
/// from inside a `modify` is [`ValueErrorKind::Reentrant`].
///
/// A read reports the newest value of each target since the last read: writes
/// in between are coalesced. The change keeps the earliest base of those
/// writes, which is what undoing them restores, and the gesture of the newest
/// one (`None` when that was a single move).
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::Watcher<&'static str, tutti_session::TempoTarget>>();
/// ```
///
/// ```compile_fail
/// fn clone<T: Clone>() {}
/// clone::<tutti_session::Watcher<&'static str, tutti_session::TempoTarget>>();
/// ```
pub struct Watcher<K: AppKey, W: IntoTargets> {
    state: Weak<ValueState<K>>,
    slot: Arc<WatchSlot>,
    targets: W,
}

impl<K: AppKey, W: IntoTargets> Watcher<K, W> {
    pub(super) fn new(state: &Arc<ValueState<K>>, ids: Vec<TargetId>, targets: W) -> Self {
        Self {
            slot: state.feed().subscribe(ids),
            state: Arc::downgrade(state),
            targets,
        }
    }

    /// Each target's newest change since the last call, in the argument's
    /// shape: `None` for a target that did not change. A target the graph no
    /// longer has reads `None`; once every target is gone and its last change
    /// read, the watcher ends.
    ///
    /// # Errors
    ///
    /// [`ValueErrorKind::Closed`] once the session is gone or every target
    /// has been removed and drained, and [`ValueErrorKind::Reentrant`] from
    /// inside a `modify`.
    pub fn changes(&mut self) -> Result<W::Changes, ValueError<K>> {
        let state = self.state.upgrade().ok_or(ValueError {
            at: 0,
            target: None,
            kind: ValueErrorKind::Closed,
        })?;
        state.changes(&self.slot, &self.targets)
    }
}

impl<K: AppKey, W: IntoTargets> Drop for Watcher<K, W> {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.feed().unsubscribe(&self.slot);
        }
    }
}

impl<K: AppKey, W: IntoTargets> fmt::Debug for Watcher<K, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watcher").finish_non_exhaustive()
    }
}

impl<K: AppKey> ValueState<K> {
    /// A watcher of `targets`, which are all there. Subscribed under the value
    /// lock, so no change lands between the check and the subscription.
    pub(crate) fn watch<W: IntoTargets>(
        self: &Arc<Self>,
        targets: W,
    ) -> Result<Watcher<K, W>, ValueError<K>> {
        let held = self.enter()?;
        let (mut ids, mut at) = (Vec::new(), 0);
        targets
            .register(held.params(), &mut at, &mut ids)
            .map_err(super::state::error)?;
        ids.sort_unstable();
        ids.dedup();
        Ok(Watcher::new(self, ids, targets))
    }

    /// What `slot` recorded since the last call, in `targets`' shape.
    fn changes<W: IntoTargets>(
        &self,
        slot: &WatchSlot,
        targets: &W,
    ) -> Result<W::Changes, ValueError<K>> {
        let held = self.enter()?;
        let drained = slot.take();
        let ended = drained.is_empty() && !slot.ids.is_empty() && !targets.any_live(held.params());
        if ended {
            return Err(super::state::whole_call(ValueErrorKind::Closed));
        }
        Ok(targets.changes(&drained))
    }
}

#[cfg(test)]
mod tests {
    use tutti_buffers::ChannelLayout;
    use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
    use tutti_units::{Hz, Q, SampleRate};

    use super::*;
    use crate::{DetachedSession, Key, Session};

    fn detached() -> (DetachedSession, Key<SvfFilterNode<f32>>) {
        let mut s = Session::builder()
            .outputs_floor(ChannelLayout::MONO)
            .build_detached(SampleRate::SR_48K)
            .expect("builds");
        let lp = s.graph().add(SvfFilterNode::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                .freq(Hz::new(1_000.0))
                .q(Q::new(0.75)),
        ));
        (s, lp)
    }

    /// **A gesture id is consumed by a batch that moved something and by nothing else**:
    /// a lone write, a set equal to the base and a refused batch leave the next id
    /// where it was.
    #[test]
    fn only_a_batch_that_moved_something_takes_a_gesture_id() {
        let (mut s, lp) = detached();
        let values = Arc::clone(&s.values);
        let feed = values.feed();
        let first = feed.next_gesture();

        s.set(lp.cutoff().to(Hz::new(2.0))).unwrap();
        s.set((lp.cutoff().to(Hz::new(2.0)), lp.q().to(Q::new(0.75))))
            .unwrap();
        let gone = s
            .graph()
            .add(SvfFilterNode::<f32>::from(SvfFilterConfig::new(
                ChannelLayout::MONO,
                SvfType::LowPass,
            )));
        s.remove(gone).unwrap();
        s.set((lp.cutoff().to(Hz::new(3.0)), gone.cutoff().to(Hz::new(3.0))))
            .unwrap_err();
        assert_eq!(feed.next_gesture(), first);

        s.set((lp.cutoff().to(Hz::new(4.0)), lp.q().to(Q::new(1.0))))
            .unwrap();
        assert_eq!(feed.next_gesture(), GestureId(first.0 + 1));
    }

    /// **Dropping a watcher unsubscribes it**: the feed's listeners return to the count
    /// before it was made.
    #[test]
    fn dropping_a_watcher_leaves_the_feed() {
        let (s, lp) = detached();
        let values = Arc::clone(&s.values);
        let feed = values.feed();
        let before = feed.watching();
        let w = s.watch(lp.cutoff()).unwrap();
        let other = s.watch(lp.q()).unwrap();
        assert_eq!(feed.watching(), before + 2);
        drop(w);
        assert_eq!(feed.watching(), before + 1);
        drop(other);
        assert_eq!(feed.watching(), before);
    }
}
