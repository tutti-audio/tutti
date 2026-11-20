//! The session's value verbs: [`Session::set`], [`Session::get`] and
//! [`Session::values`].

use super::batch::{IntoAssign, IntoRead};
use super::error::ValueError;
use super::state::Values;
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
    /// use tutti_nodes::{SvfFilterNode, SvfType};
    /// use tutti_session::Session;
    /// use tutti_units::{Hz, Q};
    ///
    /// let (mut session, _background) = Session::builder().build().unwrap();
    /// let (filter, _) = session
    ///     .graph()
    ///     .add(SvfFilterNode::<f32>::new(SvfType::LowPass, Hz::new(500.0), Q::new(0.7)));
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

    /// The session's values for any control-side thread: `Clone + Send`,
    /// borrowing nothing. Never for an audio or MIDI callback.
    pub fn values(&self) -> Values<K> {
        Values::of(&self.values)
    }
}
