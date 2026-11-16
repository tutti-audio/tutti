//! The frames an analysis reads: a window and the hop between them.

use tutti_dsp::StftGeometry;
use tutti_types::{SampleRate, Samples};

use crate::Result;

/// A window and the hop between successive windows, in [`Samples`].
///
/// A framing holds no sample rate: the call that takes the config and the
/// signal binds the rate and derives the [`StftGeometry`], and checks the
/// framing against what the analysis accepts (a window range, a hop no wider
/// than the window, the overlap a tiny hop makes), answering with a named
/// [`Error`](crate::Error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Framing {
    window: Samples,
    hop: Samples,
}

impl Framing {
    /// Frames `window` samples long, `hop` samples apart.
    #[must_use]
    pub const fn new(window: Samples, hop: Samples) -> Self {
        Self { window, hop }
    }

    /// The frame length, in [`Samples`].
    #[inline]
    #[must_use]
    pub const fn window(&self) -> Samples {
        self.window
    }

    /// The advance between frames, in [`Samples`].
    #[inline]
    #[must_use]
    pub const fn hop(&self) -> Samples {
        self.hop
    }

    /// The geometry of these frames at `rate`.
    ///
    /// # Errors
    ///
    /// [`Error::Dsp`](crate::Error::Dsp) for a zero window or a zero hop.
    pub(crate) fn geometry(self, rate: SampleRate) -> Result<StftGeometry> {
        Ok(StftGeometry::new(rate, self.window, self.hop)?)
    }

    /// Whether the window and the hop are not zero.
    ///
    /// # Errors
    ///
    /// [`Error::Dsp`](crate::Error::Dsp) for a zero window or a zero hop.
    pub(crate) fn check(self) -> Result<()> {
        // A rate has no part in the check; the geometry is the one place a
        // zero length is named.
        self.geometry(SampleRate::SR_44K1).map(|_| ())
    }
}
