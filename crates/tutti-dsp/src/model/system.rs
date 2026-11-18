//! [`System`]: what every processor answers, [`Ring`], how long it rings, and the
//! [`TAIL_FLOOR`] it is measured to.

use tutti_units::{Db, Samples, Seconds};

/// The level a tail is measured to when nothing says otherwise:
/// -60 dB, the reverberation time's definition.
pub const TAIL_FLOOR: Db = Db(-60.0);

/// How long a system rings: [`System::tail`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ring {
    /// It falls to the floor in this long.
    Finite(Seconds),
    /// It never does: a lossless loop, or one that grows.
    Unbounded,
}

/// A causal system from input signals to output signals:
/// the base of every processor class. It answers where the response to a unit
/// impulse starts and where it effectively ends.
pub trait System {
    /// The processing part of the onset of the impulse response, in frames: a
    /// lookahead or a convolver's partition. **Musical delay (a pre-delay, a delay
    /// time, a tap spacing) is not latency** and is never counted here.
    fn latency(&self) -> Samples;

    /// How long the response rings past its onset, until it is `floor` below
    /// what it carried. Musical delay counts toward it, as a reverb's pre-delay
    /// does. [`Ring::Unbounded`] when it never gets there.
    fn tail(&self, floor: Db) -> Ring;
}
