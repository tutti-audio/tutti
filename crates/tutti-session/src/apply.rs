//! [`Apply`]: an output of work the app ran, which the session takes back.

use crate::pool::{RefreshError, RefreshReport, RefreshedPool};
use crate::{AppKey, Clock, Session};

mod sealed {
    use super::{AppKey, Apply, Clock, Session};

    /// Keeps the outputs a session takes back to the ones it defines, and is
    /// where an output applies itself: no outside crate can name it.
    pub trait Sealed {
        /// Applies the output to `session`.
        fn apply_to<K: AppKey, C: Clock>(
            self,
            session: &mut Session<K, C>,
        ) -> Result<Self::Applied, Self::Error>
        where
            Self: Apply;
    }
}

/// An output of app-run work that the session takes back
/// ([`Session::apply`]).
///
/// The work's `run` reads and computes without the session; the output holds
/// what it found and the generation of the state it was cut from. `apply` is
/// the one place it changes the session. Sealed: the session defines its
/// outputs.
pub trait Apply: sealed::Sealed {
    /// What a successful apply returns.
    type Applied;
    /// Why an apply refused; every output's error has a `Stale` variant for an
    /// output cut from an older state of the session.
    type Error;
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Takes back the output of work the app ran, and applies it.
    ///
    /// Control thread. The output is cut from the session's state when the
    /// work was made; when that state has moved on (an entry released,
    /// declared again or loaded again, a newer refresh) the apply refuses by
    /// name (`Error::Stale`) and changes nothing. Run the work again to get an
    /// output of the current state.
    ///
    /// # Errors
    ///
    /// The output's own error: `Stale` for an output cut from an older state,
    /// and whatever else applying it can refuse.
    pub fn apply<D: Apply>(&mut self, done: D) -> Result<D::Applied, D::Error> {
        sealed::Sealed::apply_to(done, self)
    }
}

impl Apply for RefreshedPool {
    type Applied = RefreshReport;
    type Error = RefreshError;
}

impl sealed::Sealed for RefreshedPool {
    fn apply_to<K: AppKey, C: Clock>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<RefreshReport, RefreshError> {
        let (starts, refreshed) = session.pool.entries.reload_changed(self.checked)?;
        session.pool_start(starts);
        Ok(refreshed)
    }
}
