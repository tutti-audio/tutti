//! [`Apply`]: an output of work the app ran, which the session takes back.

use core::num::NonZeroU64;

use tutti_rt::Mint;

use crate::{AppKey, Clock, Session, SessionFloat};

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
    use super::{AppKey, Clock, Session, SessionFloat, SessionId};

    /// What every output carries: the session that made it. Per-output state (an
    /// entry's generation) lives in the output itself.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Cut {
        pub(super) session: SessionId,
    }

    impl Cut {
        /// A cut from `session`, made when the session makes the value.
        #[cfg(feature = "sampler")]
        pub(crate) const fn of<K: AppKey, C: Clock>(session: &Session<K, C>) -> Self {
            Self {
                session: session.id,
            }
        }
    }

    /// Keeps the outputs a session takes back to the ones it defines, and is
    /// where an output applies itself: no outside crate can name it. `F` is
    /// the float of the session that takes the output.
    pub trait Sealed<F: SessionFloat> {
        /// What a successful apply returns.
        type Applied;
        /// Why an apply refused.
        type Error;

        /// The session this output was cut from.
        fn cut(&self) -> &Cut;

        /// The refusal for an output of another session or an older state.
        fn stale() -> Self::Error;

        /// Applies the output to `session`.
        fn apply_to<K: AppKey, C: Clock<Float = F>>(
            self,
            session: &mut Session<K, C>,
        ) -> Result<Self::Applied, Self::Error>;
    }
}

/// An output of app-run work that the session takes back
/// ([`Session::apply`]), by a session whose float is `F`.
///
/// The work's `run` reads and computes without the session; the output holds
/// what it found and the generation of the state it was cut from. `apply` is
/// the one place it changes the session. Sealed: the session defines its
/// outputs. An output made in one float is applied only by a session of that
/// float: the other is a type error. Its result is the output's `Applied`, and
/// its refusal the output's `Error`, which has a `Stale` variant for an output
/// cut from an older state of the session.
pub trait Apply<F: SessionFloat>:
    sealed::Sealed<F, Applied = <Self as Apply<F>>::Applied, Error = <Self as Apply<F>>::Error>
{
    /// What a successful apply returns.
    type Applied;
    /// Why an apply refused: `Stale` for an output of another session or an
    /// older state, and whatever else applying it can refuse.
    type Error;
}

impl<F: SessionFloat, T: sealed::Sealed<F>> Apply<F> for T {
    type Applied = <T as sealed::Sealed<F>>::Applied;
    type Error = <T as sealed::Sealed<F>>::Error;
}

#[cfg(feature = "sampler")]
pub(crate) use sealed::{Cut, Sealed};

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Takes back the output of work the app ran, and applies it.
    ///
    /// Control thread. It never waits: an output whose target is busy comes back in
    /// its error's `Busy`. The output is cut from the session's state when the
    /// work was made; when that state has moved on (a node removed, a plugin
    /// instance replaced, a streamer gone) the apply refuses by
    /// name (`Error::Stale`) and changes nothing. An output of another session
    /// is stale here. Run the work again to get an
    /// output of the current state.
    ///
    /// # Errors
    ///
    /// The output's own error: `Stale` for an output cut from an older state,
    /// and whatever else applying it can refuse.
    pub fn apply<D: Apply<C::Float>>(
        &mut self,
        done: D,
    ) -> Result<<D as sealed::Sealed<C::Float>>::Applied, <D as sealed::Sealed<C::Float>>::Error>
    {
        if sealed::Sealed::<C::Float>::cut(&done).session != self.id {
            return Err(<D as sealed::Sealed<C::Float>>::stale());
        }
        sealed::Sealed::<C::Float>::apply_to(done, self)
    }
}
