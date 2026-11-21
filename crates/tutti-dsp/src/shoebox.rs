//! [`ShoeboxRoom`]: the early reflections of a rectangular room, by the image-source method
//! (Allen and Berkley, "Image method for efficiently simulating small-room acoustics", 1979).
//!
//! The room spans `0..=L` on each axis, and each wall reflects a fraction `1 - alpha` of the
//! amplitude, `alpha` its [`Normalized`] absorption. A reflection is a mirror image of the
//! source: along one axis, for the source at `s` and an integer `n`, the images are at
//! `2 n L + s` (reflected `|n|` times off the near wall and `|n|` off the far one) and at
//! `2 n L - s` (`|n - 1|` times off the near wall and `|n|` off the far one). An
//! image `r` metres from the listener arrives after `r / c` and at the gain
//! `Π (1 - alpha_w)^(reflections off wall w) / max(r, 1 m)`: the inverse-distance law, the
//! direct path at 1 m having gain 1, and a path shorter than that no louder.
//!
//! A room is made from a [`ShoeboxConfig`] (`ShoeboxRoom::try_from`), which refuses a source or
//! listener outside it.
//!
//! The images up to the [`ReflectionOrder`] are the [`TapSet`] of one channel, sorted by delay;
//! the direct path is left out unless asked for, since early reflections are wet only.

use tutti_units::{Gain, Metres, Normalized, SampleRate, SampleSpan, Speed};

use crate::{Inline, Tap, TapSet};

/// The distance at which a path has gain 1: a path shorter than this is not louder.
const REFERENCE: Metres = Metres::of(1.0);

/// A wall of the room: the low or high end of an axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Wall {
    /// The wall at `x = 0`.
    LowX,
    /// The wall at `x = L_x`.
    HighX,
    /// The wall at `y = 0`.
    LowY,
    /// The wall at `y = L_y`.
    HighY,
    /// The wall at `z = 0`.
    LowZ,
    /// The wall at `z = L_z`.
    HighZ,
}

impl Wall {
    const fn index(self) -> usize {
        self as usize
    }
}

/// How many reflections the images have at most.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ReflectionOrder {
    /// One reflection: the six images in the walls.
    #[default]
    First,
    /// Up to two: the first order and the eighteen images in two walls.
    Second,
}

impl ReflectionOrder {
    const fn reflections(self) -> u32 {
        match self {
            Self::First => 1,
            Self::Second => 2,
        }
    }
}

/// Why a [`ShoeboxRoom`] cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ShoeboxError {
    /// A dimension is zero.
    #[error("the room has no extent on axis {axis}")]
    Empty {
        /// 0, 1 or 2 for x, y or z.
        axis: usize,
    },
    /// The source is outside the room.
    #[error("the source is outside the room on axis {axis}")]
    Source {
        /// 0, 1 or 2 for x, y or z.
        axis: usize,
    },
    /// The listener is outside the room.
    #[error("the listener is outside the room on axis {axis}")]
    Listener {
        /// 0, 1 or 2 for x, y or z.
        axis: usize,
    },
}

/// What a [`ShoeboxRoom`] is made from: the room, the source and the listener.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShoeboxConfig {
    size: [Metres; 3],
    source: [Metres; 3],
    listener: [Metres; 3],
    absorption: [Normalized; 6],
    order: ReflectionOrder,
    speed: Speed,
    direct: bool,
}

impl ShoeboxConfig {
    /// A room of `size` (its extent on each axis, from 0) with the source and the listener at
    /// the given positions in it, every wall absorbing `absorption`, first-order, the speed of
    /// sound in air and no direct path.
    #[must_use]
    pub fn new(
        size: [Metres; 3],
        source: [Metres; 3],
        listener: [Metres; 3],
        absorption: Normalized,
    ) -> Self {
        Self {
            size,
            source,
            listener,
            absorption: [absorption; 6],
            order: ReflectionOrder::First,
            speed: Speed::SOUND_IN_AIR_20C,
            direct: false,
        }
    }

    /// One wall's absorption.
    #[must_use]
    pub fn wall(mut self, wall: Wall, absorption: Normalized) -> Self {
        self.absorption[wall.index()] = absorption;
        self
    }

    /// The most reflections an image has.
    #[must_use]
    pub fn order(self, order: ReflectionOrder) -> Self {
        Self { order, ..self }
    }

    /// The speed of sound the delays are computed from.
    #[must_use]
    pub fn speed(self, speed: Speed) -> Self {
        Self { speed, ..self }
    }

    /// The direct path is a tap too.
    #[must_use]
    pub fn direct(self) -> Self {
        Self {
            direct: true,
            ..self
        }
    }
}

/// A rectangular room with a source and a listener inside it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShoeboxRoom {
    config: ShoeboxConfig,
}

impl TryFrom<ShoeboxConfig> for ShoeboxRoom {
    type Error = ShoeboxError;

    /// The room `config` describes.
    ///
    /// # Errors
    ///
    /// [`ShoeboxError`] for a dimension of zero, and for a source or listener outside the
    /// room (a position on a wall is inside).
    fn try_from(config: ShoeboxConfig) -> Result<Self, ShoeboxError> {
        for axis in 0..3 {
            if config.size[axis] == Metres::ZERO {
                return Err(ShoeboxError::Empty { axis });
            }
            if config.source[axis] > config.size[axis] {
                return Err(ShoeboxError::Source { axis });
            }
            if config.listener[axis] > config.size[axis] {
                return Err(ShoeboxError::Listener { axis });
            }
        }
        Ok(Self { config })
    }
}

impl ShoeboxRoom {
    /// The taps at `rate`: one channel, sorted by delay, each image's delay `r / c` in frames
    /// and gain `Π (1 - alpha_w) / max(r, 1 m)` over the walls it was reflected off. At most 25 taps.
    ///
    /// # Panics
    ///
    /// Never: the images of the supported orders fit a [`TapSet`].
    #[must_use]
    pub fn taps(&self, rate: SampleRate) -> TapSet {
        let order = self.config.order.reflections();
        let mut images: Vec<Tap> = Vec::new();
        for x in axis_images(order) {
            for y in axis_images(order) {
                for z in axis_images(order) {
                    let reflections = x.reflections() + y.reflections() + z.reflections();
                    if reflections <= order && (reflections > 0 || self.config.direct) {
                        images.push(self.tap([x, y, z], rate));
                    }
                }
            }
        }
        images.sort_by(|a, b| {
            a.delay
                .get()
                .total_cmp(&b.delay.get())
                .then(a.gain.get().total_cmp(&b.gain.get()))
        });
        TapSet {
            taps: Inline::from_slice(&images).expect("at most 25 images fit the taps"),
        }
    }

    /// The tap of the image `[x, y, z]`.
    fn tap(&self, image: [AxisImage; 3], rate: SampleRate) -> Tap {
        let mut squared = 0.0;
        let mut gain = 1.0;
        for (axis, image) in image.into_iter().enumerate() {
            // The image's signed coordinate and its gap to the listener: no unit covers a
            // signed coordinate, so this one sum of squares is plain metres.
            let length = self.config.size[axis].get();
            let mirrored = if image.mirrored {
                -self.config.source[axis].get()
            } else {
                self.config.source[axis].get()
            };
            let gap =
                2.0 * f64::from(image.n) * length + mirrored - self.config.listener[axis].get();
            squared += gap * gap;
            let (low, high) = image.walls();
            gain *= (1.0 - self.config.absorption[2 * axis].get()).powi(low)
                * (1.0 - self.config.absorption[2 * axis + 1].get()).powi(high);
        }
        let distance = Metres::new(squared.sqrt()).expect("a distance in a finite room is finite");
        let level = REFERENCE
            / if distance < REFERENCE {
                REFERENCE
            } else {
                distance
            };
        let delay = SampleSpan::from_seconds(distance / self.config.speed, rate)
            .expect("a distance of a finite room is a finite delay");
        Tap {
            delay,
            gain: Gain::new(gain * level),
            channel: 0,
        }
    }
}

/// One axis's image: the source at `2 n L + s`, or mirrored at `2 n L - s`.
#[derive(Clone, Copy, Debug)]
struct AxisImage {
    n: i32,
    mirrored: bool,
}

impl AxisImage {
    /// The reflections off the near wall and off the far one: `|n - q|` and `|n|`, with `q` 1
    /// for a mirrored image.
    fn walls(self) -> (i32, i32) {
        ((self.n - i32::from(self.mirrored)).abs(), self.n.abs())
    }

    fn reflections(self) -> u32 {
        let (low, high) = self.walls();
        (low + high).unsigned_abs()
    }
}

/// The images on one axis with at most `order` reflections.
fn axis_images(order: u32) -> impl Iterator<Item = AxisImage> {
    (-1..=1)
        .flat_map(|n| [false, true].map(move |mirrored| AxisImage { n, mirrored }))
        .filter(move |image| image.reflections() <= order)
}

#[cfg(test)]
mod tests;
