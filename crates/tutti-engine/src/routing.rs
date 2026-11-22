//! Where the mix lands: the format the main output is heard in, the main output's channels
//! to the device's, and a gain, mute and solo per device output.
//!
//! The routing is composed into the one fold per `(MainOutputId, device)` pair the
//! callback applies ([`Routing::fold`]); the output gains are the stage after
//! it ([`OutputLevels`]), whose changes ramp over
//! the declick length ([`declick_frames`](tutti_graph::declick_frames)) instead of swapping folds.

use tutti_buffers::{ChannelLayout, ChannelTopology, MAX_CHANNELS};
use tutti_buffers::{Float, Fold, FoldError, FoldLevels};
use tutti_graph::fold_to_topology_with;
use tutti_units::Amplitude;

/// The device channel each channel of what is heard goes to.
///
/// [`identity`](Self::identity), the default, sets no map: the main output reaches the
/// device by speaker, into the device's topology (the identity wherever the two
/// match). A [`channels`](Self::channels) map is by index: heard channel `c`
/// is device output `to[c]`, and an output nothing is sent to is silent. Two
/// channels may not share an output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputMap(Option<Box<[usize]>>);

/// Why an [`OutputMap`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OutputMapError {
    /// A map of no channels.
    #[error("an output map needs at least one channel")]
    Empty,
    /// Two heard channels sent to one output.
    #[error("heard channels {first} and {second} are both sent to output {output}")]
    Shared {
        /// The output both are sent to.
        output: usize,
        /// The lower channel.
        first: usize,
        /// The higher channel.
        second: usize,
    },
    /// An output the engine has no channel for.
    #[error("output {output} is past the {MAX_CHANNELS} channels the engine renders")]
    Past {
        /// The refused output.
        output: usize,
    },
}

impl OutputMap {
    /// No map: the main output reaches the device by speaker.
    #[must_use]
    pub fn identity() -> Self {
        Self(None)
    }

    /// Heard channel `c` goes to output `to[c]`.
    ///
    /// # Errors
    ///
    /// [`OutputMapError`] for no channels, an output shared by two channels,
    /// or an output past the engine's width.
    pub fn channels(to: impl IntoIterator<Item = usize>) -> Result<Self, OutputMapError> {
        let to: Box<[usize]> = to.into_iter().collect();
        if to.is_empty() {
            return Err(OutputMapError::Empty);
        }
        for (second, &output) in to.iter().enumerate() {
            if output >= MAX_CHANNELS {
                return Err(OutputMapError::Past { output });
            }
            if let Some(first) = to[..second].iter().position(|&o| o == output) {
                return Err(OutputMapError::Shared {
                    output,
                    first,
                    second,
                });
            }
        }
        Ok(Self(Some(to)))
    }

    /// The output each heard channel goes to, or `None` for the identity (by
    /// speaker).
    #[must_use]
    pub fn targets(&self) -> Option<&[usize]> {
        self.0.as_deref()
    }
}

/// Why a routing has no fold on a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RoutingError {
    /// The main output cannot be folded into the monitor format, or what is heard into
    /// the device.
    #[error(transparent)]
    Fold(#[from] FoldError),
    /// The map names a channel count other than what is heard.
    #[error("the map names {map} channels and {heard:?} are heard")]
    MapWidth {
        /// The channels the map names.
        map: usize,
        /// The width of what is heard (the monitor format, or the main output).
        heard: ChannelLayout,
    },
    /// The map sends a channel to an output the device does not have.
    #[error("the map sends a channel to output {output} of a {device:?} device")]
    MapPast {
        /// The refused output.
        output: usize,
        /// The device's width.
        device: ChannelLayout,
    },
}

impl RoutingError {
    /// Whether the map is what the pair broke on.
    #[must_use]
    pub fn is_map(&self) -> bool {
        matches!(self, Self::MapWidth { .. } | Self::MapPast { .. })
    }
}

/// The main output's way to the device: the format it is heard in, the map, and the
/// levels the monitor fold takes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Routing {
    /// The format the main output is heard in; `None` is the main output's own.
    pub monitor: Option<ChannelTopology>,
    /// The device channel each heard channel goes to.
    pub map: OutputMap,
    /// The downmix levels of the fold into `monitor`.
    pub levels: FoldLevels,
}

impl Routing {
    /// The one fold from `main_output` into `device`: main output to monitor, then the map's
    /// routing or else the fold by speaker into the device's topology, composed
    /// as one matrix ([`Fold::then`]).
    ///
    /// # Errors
    ///
    /// [`RoutingError`] naming what has no place: the monitor fold, the map's
    /// width or outputs, or the fold into the device.
    pub fn fold<F: Float>(
        &self,
        main_output: &ChannelTopology,
        device: &ChannelTopology,
    ) -> Result<Fold<F>, RoutingError> {
        let heard = self.monitor.as_ref().unwrap_or(main_output);
        let to_heard = match &self.monitor {
            Some(monitor) => Some(fold_to_topology_with(main_output, monitor, self.levels)?),
            None => None,
        };
        let to_device = match self.map.targets() {
            None => fold_to_topology_with(heard, device, FoldLevels::default())?,
            Some(to) => {
                if to.len() != usize::from(heard.width().count()) {
                    return Err(RoutingError::MapWidth {
                        map: to.len(),
                        heard: heard.width(),
                    });
                }
                if let Some(&output) = to
                    .iter()
                    .find(|&&o| o >= usize::from(device.width().count()))
                {
                    return Err(RoutingError::MapPast {
                        output,
                        device: device.width(),
                    });
                }
                Fold::route(heard.width(), device.width(), to)?
            }
        };
        match to_heard {
            Some(first) => first
                .then(&to_device)
                .map_err(|e| RoutingError::Fold(FoldError::Buffer(e))),
            None => Ok(to_device),
        }
    }
}

/// A device output's gain, mute and solo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputStrip {
    /// The output's gain.
    pub gain: Amplitude,
    /// Silences the output, whatever else is set.
    pub mute: bool,
    /// While any output is soloed, only the soloed ones are heard.
    pub solo: bool,
}

impl Default for OutputStrip {
    fn default() -> Self {
        Self {
            gain: Amplitude::UNITY,
            mute: false,
            solo: false,
        }
    }
}

/// Why an output level was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OutputLevelError {
    /// An output the engine has no channel for.
    #[error("output {output} is past the {MAX_CHANNELS} channels the engine renders")]
    Past {
        /// The refused output.
        output: usize,
    },
    /// An output's gain is NaN, infinite or negative.
    #[error("the gain of output {output} is not a gain (NaN, infinite or negative)")]
    OutputGain {
        /// The output.
        output: usize,
    },
    /// The dim gain is NaN, infinite or negative.
    #[error("the dim gain is not a gain (NaN, infinite or negative)")]
    DimGain,
}

/// Whether `a` can scale a signal: finite and not negative.
fn is_gain(a: Amplitude) -> bool {
    a.get().is_finite() && a.get() >= 0.0
}

/// The gains of the device outputs: a strip each, and a dim over all.
///
/// An output with no strip is at unity. Mute wins over solo; while any output
/// is soloed an output that is not is silent; the dim, when engaged, scales
/// every output that is heard.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutputLevels {
    strips: Vec<OutputStrip>,
    dim: Option<Amplitude>,
}

impl OutputLevels {
    /// Every output at unity, none muted or soloed, no dim.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The strip of `output`: the default (unity) for one never set.
    #[must_use]
    pub fn strip(&self, output: usize) -> OutputStrip {
        self.strips.get(output).copied().unwrap_or_default()
    }

    /// The most outputs with a strip set: one past the last output set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.strips.len()
    }

    /// Whether no strip was ever set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.strips.is_empty()
    }

    /// Sets the strip of `output`.
    ///
    /// # Errors
    ///
    /// [`OutputLevelError::Past`] for an output the engine has no channel for,
    /// [`OutputLevelError::OutputGain`] for a gain that is NaN, infinite or
    /// negative.
    pub fn set_strip(&mut self, output: usize, strip: OutputStrip) -> Result<(), OutputLevelError> {
        if output >= MAX_CHANNELS {
            return Err(OutputLevelError::Past { output });
        }
        if !is_gain(strip.gain) {
            return Err(OutputLevelError::OutputGain { output });
        }
        if self.strips.len() <= output {
            self.strips.resize(output + 1, OutputStrip::default());
        }
        self.strips[output] = strip;
        Ok(())
    }

    /// This with the dim engaged at `gain`, or released for `None`.
    ///
    /// # Errors
    ///
    /// [`OutputLevelError::DimGain`] for a gain that is NaN, infinite or
    /// negative.
    pub fn with_dim(mut self, gain: Option<Amplitude>) -> Result<Self, OutputLevelError> {
        if gain.is_some_and(|g| !is_gain(g)) {
            return Err(OutputLevelError::DimGain);
        }
        self.dim = gain;
        Ok(self)
    }

    /// The dim gain, if engaged.
    #[must_use]
    pub fn dim(&self) -> Option<Amplitude> {
        self.dim
    }

    /// The gain each output is heard at: its strip's, silent when muted or when
    /// another is soloed and it is not, times the dim.
    pub(crate) fn targets(&self) -> StageTargets {
        let any_solo = self.strips.iter().any(|s| s.solo);
        let dim = self.dim.unwrap_or(Amplitude::UNITY);
        let mut gains = [Amplitude::UNITY; MAX_CHANNELS];
        for (output, gain) in gains.iter_mut().enumerate() {
            let strip = self.strip(output);
            let silent = strip.mute || (any_solo && !strip.solo);
            *gain = if silent {
                Amplitude::SILENT
            } else {
                // Two finite gains, so the product is finite unless it
                // overflows: kept at the largest finite gain.
                (strip.gain * dim.get()).min(Amplitude::new(f64::MAX))
            };
        }
        StageTargets(gains)
    }
}

/// The gain each device output is ramped to: what the callback lends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StageTargets(pub(crate) [Amplitude; MAX_CHANNELS]);

impl StageTargets {
    /// Every output at unity.
    pub(crate) const UNITY: Self = Self([Amplitude::UNITY; MAX_CHANNELS]);

    /// The gain `output` is ramped to.
    pub(crate) fn gain(&self, output: usize) -> Amplitude {
        self.0.get(output).copied().unwrap_or(Amplitude::UNITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::Bed;

    fn gains(levels: &OutputLevels) -> Vec<f64> {
        let t = levels.targets();
        (0..4).map(|o| t.gain(o).get()).collect()
    }

    /// An output with no strip is at unity; a strip's gain is its own.
    #[test]
    fn an_unset_output_is_unity_and_a_strip_is_its_gain() {
        let mut levels = OutputLevels::new();
        levels
            .set_strip(
                1,
                OutputStrip {
                    gain: Amplitude::new(0.25),
                    ..OutputStrip::default()
                },
            )
            .unwrap();
        assert_eq!(gains(&levels), [1.0, 0.25, 1.0, 1.0]);
        assert_eq!(levels.len(), 2);
    }

    /// Solo silences every output that is not soloed; mute wins over solo; the
    /// dim scales what is heard.
    #[test]
    fn solo_mute_and_dim_make_the_targets() {
        let solo = OutputStrip {
            solo: true,
            ..OutputStrip::default()
        };
        let mut levels = OutputLevels::new();
        levels.set_strip(2, solo).unwrap();
        assert_eq!(gains(&levels), [0.0, 0.0, 1.0, 0.0]);
        levels
            .set_strip(2, OutputStrip { mute: true, ..solo })
            .unwrap();
        assert_eq!(gains(&levels), [0.0, 0.0, 0.0, 0.0], "mute wins over solo");
        let levels = OutputLevels::new()
            .with_dim(Some(Amplitude::new(0.1)))
            .unwrap();
        assert_eq!(gains(&levels), [0.1, 0.1, 0.1, 0.1]);
    }

    /// An output past the engine's channels has no strip.
    #[test]
    fn a_strip_past_the_engine_is_refused() {
        assert_eq!(
            OutputLevels::new().set_strip(MAX_CHANNELS, OutputStrip::default()),
            Err(OutputLevelError::Past {
                output: MAX_CHANNELS
            })
        );
    }

    /// A gain that is NaN, infinite or negative is refused by name, for a strip
    /// and for the dim, so none reaches the device; silence is a gain.
    #[test]
    fn a_gain_that_is_not_a_gain_is_refused() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let strip = OutputStrip {
                gain: Amplitude::new(bad),
                ..OutputStrip::default()
            };
            assert_eq!(
                OutputLevels::new().set_strip(3, strip),
                Err(OutputLevelError::OutputGain { output: 3 })
            );
            assert_eq!(
                OutputLevels::new().with_dim(Some(Amplitude::new(bad))),
                Err(OutputLevelError::DimGain)
            );
        }
        assert!(
            OutputLevels::new()
                .with_dim(Some(Amplitude::new(0.0)))
                .is_ok()
        );
        // Two finite gains never make a non-finite target.
        let mut huge = OutputLevels::new();
        let big = OutputStrip {
            gain: Amplitude::new(f64::MAX),
            ..OutputStrip::default()
        };
        huge.set_strip(0, big).unwrap();
        let t = huge
            .with_dim(Some(Amplitude::new(f64::MAX)))
            .unwrap()
            .targets();
        assert!(t.gain(0).get().is_finite());
    }

    /// A map is a list of distinct outputs.
    #[test]
    fn a_map_names_distinct_outputs() {
        assert_eq!(OutputMap::channels([]), Err(OutputMapError::Empty));
        assert_eq!(
            OutputMap::channels([3, 1, 3]),
            Err(OutputMapError::Shared {
                output: 3,
                first: 0,
                second: 2
            })
        );
        assert_eq!(
            OutputMap::channels([MAX_CHANNELS]),
            Err(OutputMapError::Past {
                output: MAX_CHANNELS
            })
        );
        assert_eq!(
            OutputMap::channels([2, 5]).unwrap().targets(),
            Some(&[2, 5][..])
        );
        assert_eq!(OutputMap::identity().targets(), None);
    }

    /// A routing names what it broke on: the map's width against what is heard,
    /// the map's outputs against the device.
    #[test]
    fn a_routing_names_the_map_it_breaks_on() {
        let five_one = Bed::Surround51.topology();
        let stereo = Bed::Stereo.topology();
        let routing = Routing {
            monitor: Some(stereo.clone()),
            map: OutputMap::channels([2, 3]).unwrap(),
            ..Routing::default()
        };
        // Two heard channels on a six-output device: fine.
        assert!(routing.fold::<f32>(&five_one, &five_one).is_ok());
        // On a stereo device output 3 does not exist.
        assert_eq!(
            routing.fold::<f32>(&five_one, &stereo).map(|_| ()),
            Err(RoutingError::MapPast {
                output: 2,
                device: ChannelLayout::STEREO
            })
        );
        // A map of three channels against a stereo monitor.
        let wide = Routing {
            map: OutputMap::channels([0, 1, 2]).unwrap(),
            ..routing
        };
        assert_eq!(
            wide.fold::<f32>(&five_one, &five_one).map(|_| ()),
            Err(RoutingError::MapWidth {
                map: 3,
                heard: ChannelLayout::STEREO
            })
        );
        assert!(wide.fold::<f32>(&five_one, &five_one).unwrap_err().is_map());
    }
}
