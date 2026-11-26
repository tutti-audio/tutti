//! [`Rendered`]: planar audio held in memory, which only this crate builds
//! unchecked.

use tutti_buffers::{ChannelLayout, LayoutError};
use tutti_buffers::{Float, Planes, Wave};
use tutti_units::{SampleRate, Samples};

/// Rendered audio held in memory, one `Vec` per channel.
///
/// Returned by a [`BufferTarget`](crate::BufferTarget) output of
/// [`render`](crate::render()) ([`Produced::Buffers`](crate::Produced::Buffers)); written with
/// `tutti_codec::write` (it converts into a `Wave`). Planes rather than a
/// `(left, right)` pair, so a surround render keeps every channel.
///
/// The fields are private: a `Rendered` is built by a render or by
/// [`Rendered::new`], which checks that the planes pair up, so one that exists
/// has at least one channel, every plane the same length and a rate a render
/// can run at. The getters are the reading.
///
/// ```
/// # use tutti_export::Rendered;
/// # use tutti_units::SampleRate;
/// let rendered = Rendered::new(vec![vec![0.0f32; 4], vec![0.0; 4]], SampleRate::SR_48K, 0)?;
/// assert_eq!(rendered.channels(), 2);
/// assert_eq!(rendered.planes()[1].len(), 4);
/// # Ok::<(), tutti_export::RenderedError>(())
/// ```
///
/// A struct literal and a write to a field are refused outside this crate
/// (each has a compiling twin through
/// [`new`](Self::new) and the
/// getters above):
///
/// ```compile_fail,E0451
/// # use tutti_export::Rendered;
/// # use tutti_units::SampleRate;
/// let _ = Rendered {
///     planes: vec![vec![0.0f32; 4]],
///     sample_rate: SampleRate::SR_48K,
///     dropped_events: 0,
/// };
/// ```
///
/// ```compile_fail,E0616
/// # use tutti_export::Rendered;
/// # use tutti_units::SampleRate;
/// let mut rendered = Rendered::new(vec![vec![0.0f32; 4]], SampleRate::SR_48K, 0).unwrap();
/// rendered.planes = vec![vec![0.0f32; 4], vec![0.0; 9]];
/// ```
#[derive(Clone, PartialEq)]
pub struct Rendered<F: Float = f32> {
    planes: Vec<Vec<F>>,
    sample_rate: SampleRate,
    dropped_events: u64,
}

/// Why [`Rendered::new`] refused.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum RenderedError {
    /// No planes: a render has at least one channel.
    #[error("rendered audio has no channels")]
    NoPlanes,
    /// A plane's length is not the first plane's.
    #[error("plane {plane} has {found:?} frames and plane 0 has {expected:?}")]
    UnequalPlanes {
        /// The first plane that differs.
        plane: usize,
        /// The length of plane 0.
        expected: Samples,
        /// The length of that plane.
        found: Samples,
    },
    /// More planes than a [`ChannelLayout`] holds.
    #[error(transparent)]
    Layout(#[from] LayoutError),
}

impl<F: Float> std::fmt::Debug for Rendered<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rendered")
            .field("channels", &self.channels())
            .field("frames", &self.frames())
            .field("sample_rate", &self.sample_rate)
            .field("dropped_events", &self.dropped_events)
            .finish_non_exhaustive()
    }
}

impl<F: Float> Rendered<F> {
    /// Planes of audio at `sample_rate`, from a render that lost
    /// `dropped_events`.
    ///
    /// # Errors
    ///
    /// [`RenderedError::NoPlanes`] for no planes,
    /// [`RenderedError::Layout`] for more planes than a layout holds,
    /// [`RenderedError::UnequalPlanes`] for planes of different lengths.
    pub fn new(
        planes: Vec<Vec<F>>,
        sample_rate: SampleRate,
        dropped_events: u64,
    ) -> Result<Self, RenderedError> {
        let Some(first) = planes.first() else {
            return Err(RenderedError::NoPlanes);
        };
        ChannelLayout::try_from(planes.len())?;
        if let Some((plane, other)) = planes
            .iter()
            .enumerate()
            .find(|(_, p)| p.len() != first.len())
        {
            return Err(RenderedError::UnequalPlanes {
                plane,
                expected: Samples::new(first.len()),
                found: Samples::new(other.len()),
            });
        }
        Ok(Self {
            planes,
            sample_rate,
            dropped_events,
        })
    }

    /// Built by this crate's own passes, which hold the pairing by
    /// construction (`debug_assert`ed here, checked by [`new`](Self::new) for
    /// a caller).
    pub(crate) fn from_render(
        planes: Vec<Vec<F>>,
        sample_rate: SampleRate,
        dropped_events: u64,
    ) -> Self {
        debug_assert!(
            Self::new(planes.clone(), sample_rate, dropped_events).is_ok(),
            "a render's planes pair up"
        );
        Self {
            planes,
            sample_rate,
            dropped_events,
        }
    }

    /// The planes, one per channel, all the same length. Planar, not
    /// interleaved: [`interleaved`](Self::interleaved) is the conversion.
    pub fn planes(&self) -> &[Vec<F>] {
        &self.planes
    }

    /// The planes, taken.
    pub fn into_planes(self) -> Vec<Vec<F>> {
        self.planes
    }

    /// The rate these samples are at, which is not necessarily the rate a
    /// config asked to write: a buffers output
    /// does not resample, so this is the render rate. An encoder must read it
    /// from here rather than from a config, or it converts from a rate the
    /// samples were never at.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Events the graph lost during the render (not a note-off it refused and
    /// sent again); nonzero only under
    /// [`EventLossPolicy::Allow`](crate::EventLossPolicy::Allow) (under `Fail`
    /// the render is an error instead). A lost note-on or note-off makes the
    /// audio wrong.
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    /// Returns the frames per plane — the length of one channel, not the total
    /// sample count.
    pub fn frames(&self) -> Samples {
        Samples::new(self.planes.first().map_or(0, |p| p.len()))
    }

    /// Returns the channel count — the interleave stride, and the number of
    /// planes.
    pub fn channels(&self) -> usize {
        self.planes.len()
    }

    /// Returns the width these planes carry as a [`ChannelLayout`].
    ///
    /// [`channels`](Self::channels) is the same number as a raw stride.
    pub fn layout(&self) -> ChannelLayout {
        // `new` refuses more planes than a layout holds.
        ChannelLayout::clamped(self.planes.len())
    }

    /// Multiplies every sample by `gain` — the apply half of a
    /// measure-then-apply normalization.
    pub fn apply_gain(&mut self, gain: tutti_units::Db) {
        let scale = F::from_f64(gain.to_amplitude().get());
        for plane in self.planes.iter_mut() {
            for s in plane.iter_mut() {
                *s *= scale;
            }
        }
    }

    /// Returns the planes interleaved — the shape a meter or an encoder takes.
    ///
    /// Allocates the whole render. A `Rendered` holds an entire offline
    /// pass in memory, so this doubles that for the duration of the call; on a
    /// long export it is the largest single allocation in the crate. Prefer
    /// [`interleaved_into`](Self::interleaved_into) wherever the interleaved
    /// copy is read and dropped rather than kept.
    pub fn interleaved(&self) -> Vec<F> {
        let mut out = Vec::new();
        self.interleaved_into(&mut out);
        out
    }

    /// Interleaves into a caller-owned buffer, reusing its allocation.
    ///
    /// `out` is cleared first, so it is a destination and not an accumulator.
    /// Prefer it over [`interleaved`](Self::interleaved) when interleaving more
    /// than once.
    pub fn interleaved_into(&self, out: &mut Vec<F>) {
        out.clear();
        if self.planes.is_empty() {
            return;
        }
        out.resize(self.layout().interleaved_len(self.frames()), F::ZERO);
        // At least one plane of one length, and `out` is whole frames of them:
        // the views always build and agree.
        let written = tutti_buffers::Planes::from_vecs(&self.planes).and_then(|planes| {
            tutti_buffers::InterleavedMut::new(out, self.layout())
                .and_then(|out| tutti_buffers::interleave(planes, out))
        });
        debug_assert!(
            written.is_ok(),
            "a render's planes are one length: {written:?}"
        );
        if written.is_err() {
            // Not interleaved: silent, never a partial write.
            out.fill(F::ZERO);
        }
    }
}

impl<F: Float> From<Rendered<F>> for Wave<F> {
    /// The planes as a [`Wave`] at the render's rate, one channel per plane,
    /// in the engine's order for the width where it has one (unassigned
    /// channels where it has none: a render keeps no topology of its own).
    /// Infallible: a `Rendered` has at least one channel, no more than a
    /// layout holds, and planes of one length, which is what a `Wave` is.
    fn from(rendered: Rendered<F>) -> Self {
        let rate = rendered.sample_rate;
        let planes = rendered.into_planes();
        let planes =
            Planes::from_vecs(&planes).expect("a render has at least one plane, all of one length");
        Wave::from_planes(rate, planes)
    }
}

#[cfg(test)]
mod tests;
