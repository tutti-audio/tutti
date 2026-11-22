//! The session's value verbs: [`Session::set`], [`Session::get`] and
//! [`Session::values`].

use super::batch::{IntoAssign, IntoRead};
use super::error::ValueError;
use super::state::Values;
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

    /// The session's values for any control-side thread: `Clone + Send`,
    /// borrowing nothing. Never for an audio or MIDI callback.
    pub fn values(&self) -> Values<K> {
        Values::of(&self.values)
    }
}
