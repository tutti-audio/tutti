//! The session's value verbs: [`Session::set`], [`Session::get`] and
//! [`Session::values`].

use super::batch::{IntoAssign, IntoRead};
use super::error::ValueError;
use super::gesture::Gesture;
use super::saved::SavedValues;
use super::state::{ValueState, Values};
use super::target::AnyAssign;
use super::watcher::{IntoTargets, Watcher};
use crate::{AppKey, Clock, Session};

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Writes `assign`, whatever the clock: an assignment (`target.to(v)`), an
    /// edit (`target.modify(f)`), or a tuple, array, `Vec` or `Option` of
    /// them.
    ///
    /// The call takes the value lock, checks every item and runs every
    /// `modify` in argument order (each sees the value the call has built so
    /// far for its target), and only then stores each target's final value: a
    /// failing item lands nothing. A native param is stored at once and the
    /// node reads it at the start of its next block; a batch is never one cut
    /// for the audio thread.
    ///
    /// # Errors
    ///
    /// A [`ValueError`] whose `at` is the first failing item's position
    /// (an `Option`'s `None` counted):
    /// [`NoSuchTarget`](crate::ValueErrorKind::NoSuchTarget) for a node or
    /// param that is not there, [`Refused`](crate::ValueErrorKind::Refused)
    /// for a value the target does not take, and
    /// [`Reentrant`](crate::ValueErrorKind::Reentrant) from inside a `modify`.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_buffers::ChannelLayout;
    /// use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
    /// use tutti_session::Session;
    /// use tutti_units::{Hz, Q};
    ///
    /// let mut session = Session::builder().build().unwrap();
    /// let filter = session
    ///     .graph()
    ///     .add(SvfFilterNode::<f32>::from(
    ///         SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
    ///             .freq(Hz::new(500.0))
    ///             .q(Q::new(0.7)),
    ///     ));
    /// session.set(filter.cutoff().to(Hz::new(800.0))).unwrap();
    /// session.set(filter.cutoff().modify(|hz| *hz * 2.0)).unwrap();
    /// assert_eq!(session.get(filter.cutoff()).unwrap(), Hz::new(1_600.0));
    /// ```
    pub fn set(&self, assign: impl IntoAssign<K>) -> Result<(), ValueError<K>> {
        self.values.set(assign)
    }

    /// Reads `read` in its own shape: a target answers its base value, and a
    /// tuple, array, `Vec` or `Option` of them answers the same shape. A
    /// target's [`info`](crate::ParamTarget::info) answers what a control
    /// shows of it.
    ///
    /// # Errors
    ///
    /// A [`ValueError`] whose `at` is the first failing item's position:
    /// [`NoSuchTarget`](crate::ValueErrorKind::NoSuchTarget), and
    /// [`Reentrant`](crate::ValueErrorKind::Reentrant) from inside a
    /// `modify`.
    pub fn get<R: IntoRead>(&self, read: R) -> Result<R::Output, ValueError<K>> {
        self.values.get(read)
    }

    /// A [`Watcher`] of the changes of `targets`: a target, or a tuple, array,
    /// `Vec` or `Option` of them, each one that is not a plugin's parameter.
    /// A change is a base that moved: a `set` of the value a target already
    /// has changes nothing. A batch that moves two or more targets is one
    /// gesture, its changes sharing a [`GestureId`](crate::GestureId). Only
    /// writes by the app are reported: a tempo a follower moves, or a value a
    /// lane drives, is not.
    ///
    /// # Errors
    ///
    /// A [`ValueError`] whose `at` is the first missing target's position:
    /// [`NoSuchTarget`](crate::ValueErrorKind::NoSuchTarget), and
    /// [`Reentrant`](crate::ValueErrorKind::Reentrant) from inside a
    /// `modify`.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_buffers::ChannelLayout;
    /// use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
    /// use tutti_session::Session;
    /// use tutti_units::{Hz, Q};
    ///
    /// let mut session = Session::builder().build().unwrap();
    /// let filter = session
    ///     .graph()
    ///     .add(SvfFilterNode::<f32>::from(
    ///         SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
    ///             .freq(Hz::new(500.0))
    ///             .q(Q::new(0.7)),
    ///     ));
    /// let mut watcher = session.watch(filter.cutoff()).unwrap();
    /// assert!(watcher.changes().unwrap().is_none());
    /// session.set(filter.cutoff().to(Hz::new(800.0))).unwrap();
    /// let change = watcher.changes().unwrap().unwrap();
    /// assert_eq!(change.value, Hz::new(800.0));
    /// ```
    pub fn watch<W: IntoTargets>(&self, targets: W) -> Result<Watcher<K, W>, ValueError<K>> {
        self.values.watch(targets)
    }

    /// A [`Gesture`] over `targets`: a target, or a tuple, array, `Vec` or
    /// `Option` of them, each one that is not a plugin's parameter, which an app
    /// opens while a control is held. Its [`set`](Gesture::set) writes an
    /// absolute value to each, through the batch of a `set`, and every change
    /// carries the gesture's [`GestureId`](crate::GestureId), not ended until the
    /// gesture is dropped: its end is one change per target it moved, landed
    /// together. A gesture that moved nothing records nothing.
    ///
    /// # Errors
    ///
    /// As [`watch`](Self::watch): the first missing target's
    /// [`NoSuchTarget`](crate::ValueErrorKind::NoSuchTarget), and
    /// [`Reentrant`](crate::ValueErrorKind::Reentrant) from inside a
    /// `modify`.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_buffers::ChannelLayout;
    /// use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
    /// use tutti_session::Session;
    /// use tutti_units::{Hz, Q};
    ///
    /// let mut session = Session::builder().build().unwrap();
    /// let filter = session
    ///     .graph()
    ///     .add(SvfFilterNode::<f32>::from(
    ///         SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
    ///             .freq(Hz::new(500.0))
    ///             .q(Q::new(0.7)),
    ///     ));
    /// let mut watcher = session.watch(filter.cutoff()).unwrap();
    /// let mut drag = session.gesture(filter.cutoff()).unwrap();
    /// drag.set(Hz::new(800.0)).unwrap();
    /// let open = watcher.changes().unwrap().unwrap().gesture.unwrap();
    /// assert!(!open.ended);
    /// drop(drag);
    /// let end = watcher.changes().unwrap().unwrap().gesture.unwrap();
    /// assert!(end.ended);
    /// assert_eq!(end.base, Hz::new(500.0));
    /// ```
    pub fn gesture<W: IntoTargets>(&self, targets: W) -> Result<Gesture<K, W>, ValueError<K>> {
        self.values.gesture(targets)
    }

    /// The base of every saved target at one instant: a [`SavedValues`] that
    /// `set(&saved)` puts back. Every saved target is read under the value
    /// lock, so a batch is in the map whole or not at all. A plugin's
    /// parameters, the policies, a MIDI device's latency and a plugin's
    /// automation mode are not saved.
    ///
    /// # Errors
    ///
    /// [`Reentrant`](crate::ValueErrorKind::Reentrant) from inside a
    /// `modify`.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_buffers::ChannelLayout;
    /// use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
    /// use tutti_session::Session;
    /// use tutti_units::{Hz, Q};
    ///
    /// let mut session = Session::builder().build().unwrap();
    /// let filter = session
    ///     .graph()
    ///     .add(SvfFilterNode::<f32>::from(
    ///         SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
    ///             .freq(Hz::new(500.0))
    ///             .q(Q::new(0.7)),
    ///     ));
    /// let saved = session.saved_values().unwrap();
    /// session.set(filter.cutoff().to(Hz::new(800.0))).unwrap();
    /// session.set(&saved).unwrap();
    /// assert_eq!(session.get(filter.cutoff()).unwrap(), Hz::new(500.0));
    /// ```
    pub fn saved_values(&self) -> Result<SavedValues<K>, ValueError<K>> {
        self.values.saved_values()
    }

    /// The assignments among `items` whose target is saved, as a
    /// [`SavedValues`]: a preset built from assignments (an
    /// [`Assign::erase`](crate::Assign::erase) each) rather than read off the
    /// session. An item whose target is not saved is left out.
    pub fn values_of(&self, items: impl IntoIterator<Item = AnyAssign>) -> SavedValues<K> {
        ValueState::values_of(items)
    }

    /// The session's values for any control-side thread: `Clone + Send`,
    /// borrowing nothing. Never for an audio or MIDI callback.
    pub fn values(&self) -> Values<K> {
        Values::of(&self.values)
    }
}
