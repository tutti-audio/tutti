//! [`Apply`]: an output of work the app ran, which the session takes back.

use core::num::NonZeroU64;

use tutti_rt::Mint;

use crate::pool::{RefreshError, RefreshReport, RefreshedPool};
use crate::{AppKey, Clock, Session};

/// Which session an output was cut from: unique in the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionId(NonZeroU64);

impl SessionId {
    /// The next session's identity.
    pub(crate) fn new() -> Self {
        static NEXT: Mint = Mint::new();
        Self(NEXT.mint())
    }
}

mod sealed {
    use super::{AppKey, Apply, Clock, Session, SessionId};

    /// What every output carries: the session that made it. Per-output state (an
    /// entry's generation) lives in the output itself.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Cut {
        pub(super) session: SessionId,
    }

    impl Cut {
        /// A cut from `session`, made when the session makes the value.
        pub(crate) const fn of<K: AppKey, C: Clock>(session: &Session<K, C>) -> Self {
            Self {
                session: session.id,
            }
        }
    }

    /// Keeps the outputs a session takes back to the ones it defines, and is
    /// where an output applies itself: no outside crate can name it.
    pub trait Sealed {
        /// The session this output was cut from.
        fn cut(&self) -> &Cut;

        /// The refusal for an output of another session or an older state.
        fn stale() -> Self::Error
        where
            Self: Apply;

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

pub(crate) use sealed::Cut;
#[cfg(feature = "sampler")]
pub(crate) use sealed::Sealed;

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Takes back the output of work the app ran, and applies it.
    ///
    /// Control thread. The output is cut from the session's state when the
    /// work was made; when that state has moved on (an entry released,
    /// declared again or loaded again, a newer refresh) the apply refuses by
    /// name (`Error::Stale`) and changes nothing. An output of another session
    /// is stale here. Run the work again to get an
    /// output of the current state.
    ///
    /// # Errors
    ///
    /// The output's own error: `Stale` for an output cut from an older state,
    /// and whatever else applying it can refuse.
    pub fn apply<D: Apply>(&mut self, done: D) -> Result<D::Applied, D::Error> {
        if sealed::Sealed::cut(&done).session != self.id {
            return Err(<D as sealed::Sealed>::stale());
        }
        sealed::Sealed::apply_to(done, self)
    }
}

impl Apply for RefreshedPool {
    type Applied = RefreshReport;
    type Error = RefreshError;
}

impl sealed::Sealed for RefreshedPool {
    fn cut(&self) -> &Cut {
        &self.cut
    }

    fn stale() -> RefreshError {
        RefreshError::Stale
    }

    fn apply_to<K: AppKey, C: Clock>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<RefreshReport, RefreshError> {
        let (starts, refreshed) = session.pool.entries.reload_changed(self.checked)?;
        session.pool_start(starts);
        Ok(refreshed)
    }
}
