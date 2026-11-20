//! The declared plain-unit range of each parameter, and the conversion between
//! it and the normalized `0..=1` the shared vocabulary speaks.
//!
//! `ProcessContext::param_changes` and [`Control`](tutti_plugin_types::Control)
//! carry **normalized** values (the host's authoring convention), but
//! `AudioUnitSetParameter` takes **native plain units**: AU has no
//! normalization concept at all. Denormalizing needs the declared range, and
//! re-reading `kAudioUnitProperty_ParameterInfo` per automation point on the
//! audio thread would be a property round-trip per block, so the ranges are read
//! once, when the plugin loads, and again when it says its parameter list moved.

use tutti_au_ffi::Unit;
use tutti_plugin_types::{LinearRange, ParamMapping, PlainMap};
use tutti_units::Normalized;

use std::sync::Arc;

use crate::marks::{Marks, Slot};
use crate::parameters;

/// Declared plain-unit bounds for one AU parameter, as reported by
/// `kAudioUnitProperty_ParameterInfo`.
///
/// Raw `f64` Hz/dB/percent/seconds, not `tutti_units` unit newtypes: which
/// physical unit these bounds are in is per-parameter and only known at runtime
/// from `AudioUnitParameterInfo::unit`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParamBounds {
    pub(crate) min: f64,
    pub(crate) max: f64,
}

impl ParamBounds {
    /// The bounds an AU declares, `min` and `max` of
    /// `kAudioUnitProperty_ParameterInfo`: the one reading of them that the
    /// range table and the shared [`ParameterInfo`](tutti_plugin_types::ParameterInfo)
    /// both come from.
    pub(crate) fn of(min: f32, max: f32) -> Self {
        // `AudioUnitParameterInfo` carries the bounds as `Float32`; they widen
        // here, once, into the quantity the rest of the host works in.
        Self {
            min: f64::from(min),
            max: f64::from(max),
        }
    }

    /// The plain-value map of the bounds, in `f64`: the line between them. The
    /// one place the map is built: this type's [`ParamMapping`] is it (narrowed
    /// to the `f32` the unit takes), and the parameter's
    /// [`plain_map`](tutti_plugin_types::ParameterInfo::plain_map) is it.
    pub(crate) fn plain_map(self) -> PlainMap {
        PlainMap::Linear(LinearRange::new(self.min, self.max))
    }

    /// The `Float32` word `AudioUnitSetParameter` takes for the position
    /// `value`: AU's parameter word is `f32`, so this is the one place a value
    /// narrows on its way to the unit.
    pub(crate) fn word(self, value: Normalized) -> f32 {
        self.plain_map().plain(value) as f32
    }
}

/// **This is AU's own conversion, and it is linear.** An AU has no normalized
/// value (`AudioUnitGetParameter` and `AudioUnitSetParameter` speak plain
/// `Float32` only), so the position is the host's, over the declared span. The
/// display-curve flags (`DisplayLogarithmic`, ..., `AudioUnitProperties.h`) are
/// *display scales*: the header gives them no formula, so they stay
/// `DisplayCurve` hints for a UI and do not bend this mapping. The one curve the
/// platform defines is `AUParameterValueFromLinear` (`AudioUnitUtilities.h`),
/// keyed on the unit, which this crate does not call.
impl ParamMapping for ParamBounds {
    /// Maps a position onto `[min, max]`, narrowed to the `Float32` that
    /// `AudioUnitSetParameter` takes (and widened back, so the value is the one
    /// the unit is given: [`ParamBounds::word`] is the same narrowing). A range
    /// with no span yields `min` when finite and `0.0` otherwise
    /// ([`LinearRange`]), so the result is always finite and inside
    /// `[min, max]` (both came from an `f32`).
    fn plain(&self, value: Normalized) -> f64 {
        f64::from(self.word(value))
    }

    /// The AU's plain value back onto `0..=1`, the read direction of the same
    /// boundary: clamped into the range, `0.0` for NaN and for a range with no
    /// span ([`LinearRange`]).
    fn normalized(&self, plain: f64) -> Normalized {
        self.plain_map().normalized(plain)
    }
}

/// The declared bounds of every parameter, sorted by id.
///
/// A `Vec` sorted by id rather than a `HashMap`: AU parameter counts are in the
/// tens, so a binary search beats hashing and keeps the render path
/// allocation-free.
#[derive(Debug, Clone, Default)]
pub(crate) struct RangeTable {
    ranges: Vec<(u32, ParamBounds)>,
    /// The ids flagged `kAudioUnitParameterFlag_NonRealTime`, sorted: a write
    /// "in real-time will cause a glitch or otherwise undesirable effect"
    /// (`AudioUnitProperties.h`), so the audio thread never makes one.
    non_realtime: Vec<u32>,
    /// The ids the AU does not declare writable
    /// (`kAudioUnitParameterFlag_IsWritable` clear), sorted: the host never
    /// writes them.
    read_only: Vec<u32>,
    /// What the render tells the control side of each parameter here: sized for
    /// every id in `ranges`, so each declared parameter has a [`Slot`].
    marks: Arc<Marks>,
}

impl RangeTable {
    /// Reads every parameter's declared range off the AU. `previous` is the
    /// marks of the table this replaces, kept when they have room for every id.
    pub(crate) fn read(unit: &Unit, previous: Option<&Arc<Marks>>) -> Self {
        let list = parameters::list(unit);
        let mut non_realtime: Vec<u32> = list
            .iter()
            .filter(|p| p.non_realtime)
            .map(|p| p.id)
            .collect();
        non_realtime.sort_unstable();
        let mut read_only: Vec<u32> = list.iter().filter(|p| !p.writable).map(|p| p.id).collect();
        read_only.sort_unstable();
        let mut ranges: Vec<(u32, ParamBounds)> = list
            .into_iter()
            .map(|p| (p.id, ParamBounds::of(p.range.min, p.range.max)))
            .collect();
        ranges.sort_unstable_by_key(|&(id, _)| id);
        let ids: Vec<u32> = ranges.iter().map(|&(id, _)| id).collect();
        Self {
            marks: Marks::covering(previous, &ids),
            ranges,
            non_realtime,
            read_only,
        }
    }

    /// The marks the render sets and the control tick takes.
    pub(crate) fn marks(&self) -> &Arc<Marks> {
        &self.marks
    }

    /// `id`'s place in [`marks`](Self::marks); every declared id has one.
    pub(crate) fn slot(&self, id: u32) -> Option<Slot> {
        self.marks.slot(id)
    }

    /// Whether the AU flagged `id` `NonRealTime`: a write to it belongs between
    /// blocks, on the control side.
    pub(crate) fn is_non_realtime(&self, id: u32) -> bool {
        self.non_realtime.binary_search(&id).is_ok()
    }

    /// Whether the AU declared `id` read-only: not writable by the host.
    pub(crate) fn is_read_only(&self, id: u32) -> bool {
        self.read_only.binary_search(&id).is_ok()
    }

    /// What the AU declares of `id`'s writability, `None` for a parameter it
    /// never declared: the figure the write policy
    /// ([`ParamWriteError::check`](tutti_plugin_types::ParamWriteError::check))
    /// reads.
    pub(crate) fn access(&self, id: u32) -> Option<tutti_plugin_types::ParamAccess> {
        self.bounds(id)
            .map(|_| tutti_plugin_types::ParamAccess::reported(self.is_read_only(id)))
    }

    /// Whether `plain`, a value the unit reported for `id`, is the narrowing of
    /// the position the host last wrote there: the unit repeating the host's
    /// own write, not an edit of its own. Any other report clears the record
    /// ([`tutti_plugin_types::WrittenValues::absorbs`]). `false` for a parameter
    /// off the table or never written.
    pub(crate) fn absorbs_report(&self, id: u32, plain: f32) -> bool {
        self.bounds(id)
            .is_some_and(|bounds| self.marks.absorbs(id, |value| bounds.word(value) == plain))
    }

    /// Every declared id, ascending.
    pub(crate) fn ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.ranges.iter().map(|&(id, _)| id)
    }

    /// The declared bounds for `id`, or `None` for a parameter the AU never
    /// declared: it has no range to denormalize against, so a caller skips or
    /// passes it through rather than guessing one.
    pub(crate) fn bounds(&self, id: u32) -> Option<ParamBounds> {
        self.ranges
            .binary_search_by_key(&id, |&(pid, _)| pid)
            .ok()
            .map(|i| self.ranges[i].1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: f64) -> Normalized {
        Normalized::of(v)
    }

    #[test]
    fn plain_maps_onto_the_declared_range_not_identity() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };
        assert_eq!(cutoff.plain(n(0.0)), 10.0);
        assert_eq!(cutoff.plain(n(1.0)), 22_050.0);
        assert_eq!(cutoff.plain(n(0.5)), 11_030.0);
        // Not 1.0: 1 Hz, an inaudible filter.
        assert_ne!(cutoff.plain(n(1.0)), 1.0);

        // Negative minima (AUDelay Feedback is [-99.9, 99.9]) must map too; the
        // old clamp to [0,1] made the entire negative half unreachable.
        let feedback = ParamBounds {
            min: -99.9,
            max: 99.9,
        };
        assert!((feedback.plain(n(0.0)) - -99.9).abs() < 1e-3);
        assert!(feedback.plain(n(0.5)).abs() < 1e-3);
        assert!((feedback.plain(n(1.0)) - 99.9).abs() < 1e-3);

        // A degenerate range yields `min` rather than NaN/inf.
        let degenerate = ParamBounds { min: 3.0, max: 3.0 };
        assert_eq!(degenerate.plain(n(0.5)), 3.0);
    }

    /// The live-path half: `plain`'s value goes straight into
    /// `AudioUnitSetParameter` on a running unit, so a NaN escaping here is a NaN
    /// in a live filter coefficient. The bounds come from the plugin's own
    /// `kAudioUnitProperty_ParameterInfo`, so they are not trusted.
    #[test]
    fn a_malformed_range_never_reaches_a_live_au_parameter_as_non_finite() {
        for (min, max) in [
            (f64::NAN, 1.0),
            (0.0, f64::NAN),
            (f64::NEG_INFINITY, 1.0),
            (0.0, f64::INFINITY),
        ] {
            let broken = ParamBounds { min, max };
            for v in [0.0, 0.5, 1.0] {
                assert!(
                    broken.plain(n(v)).is_finite(),
                    "plain({v}) with bounds [{min}, {max}] returned non-finite"
                );
            }
            assert_eq!(broken.normalized(0.5).get(), 0.0);
        }
    }

    /// AU's position is the line over the declared range. The display curves
    /// (`DisplayLogarithmic`, ...) are display hints and do not bend it: a log
    /// cutoff's midpoint is the arithmetic one. (The laws of the map are
    /// `mapping_contract!`'s, below.)
    #[test]
    fn the_position_is_the_line() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };
        assert_eq!(cutoff.normalized(10.0).get(), 0.0);
        assert_eq!(cutoff.normalized(22_050.0).get(), 1.0);
        assert_eq!(cutoff.normalized(11_030.0).get(), 0.5);
        assert_eq!(
            ParamBounds { min: 3.0, max: 3.0 }.normalized(3.0).get(),
            0.0
        );
    }

    tutti_plugin_types::mapping_contract!(
        cutoff_map,
        ParamBounds {
            min: 10.0,
            max: 22_050.0
        }
    );
    tutti_plugin_types::mapping_contract!(
        feedback_map,
        ParamBounds {
            min: -99.9,
            max: 99.9
        }
    );
    tutti_plugin_types::mapping_contract!(one_point_map, ParamBounds { min: 3.0, max: 3.0 });

    #[test]
    fn a_table_finds_ids_by_binary_search() {
        let table = RangeTable {
            ranges: vec![
                (2u32, ParamBounds { min: 0.0, max: 1.0 }),
                (
                    7u32,
                    ParamBounds {
                        min: 10.0,
                        max: 22_050.0,
                    },
                ),
            ],
            non_realtime: vec![7],
            read_only: vec![2],
            marks: Arc::new(Marks::new(vec![2, 7])),
        };
        assert!(table.is_non_realtime(7));
        assert!(!table.is_non_realtime(2));
        assert!(table.is_read_only(2) && !table.is_read_only(7));
        assert_eq!(table.bounds(7).map(|b| b.max), Some(22_050.0));
        assert_eq!(table.bounds(2).map(|b| b.min), Some(0.0));
        // A parameter the AU never declared has no range to denormalize
        // against, so it must be reported missing (and skipped), not guessed.
        assert!(table.bounds(3).is_none());
        assert!(table.slot(7).is_some() && table.slot(3).is_none());
    }

    fn written_table() -> RangeTable {
        RangeTable {
            ranges: vec![(
                7u32,
                ParamBounds {
                    min: 10.0,
                    max: 22_050.0,
                },
            )],
            non_realtime: Vec::new(),
            read_only: Vec::new(),
            marks: Arc::new(Marks::new(vec![7])),
        }
    }

    /// The unit reporting the `Float32` narrowing of a position the host wrote
    /// (one that differs from it only below `f32` precision) is an echo, however
    /// often it does, so the control side drops it and the `f64` written stays
    /// the value.
    #[test]
    fn a_report_equal_to_the_narrowing_of_a_write_is_an_echo_every_time() {
        let table = written_table();
        let bounds = table.bounds(7).unwrap();
        let written = n(0.1 + 1e-12);
        table.marks().record(table.slot(7).unwrap(), written);
        let word = bounds.word(written);
        assert_ne!(f64::from(word), bounds.plain_map().plain(written));
        assert!(table.absorbs_report(7, word));
        assert!(table.absorbs_report(7, word));
    }

    /// A report off the narrowing is the unit's own change, and clears the
    /// record: the narrowing of the old write, reported after, is a change too.
    #[test]
    fn the_old_narrowing_after_the_units_own_change_is_reported() {
        let table = written_table();
        let bounds = table.bounds(7).unwrap();
        assert!(!table.absorbs_report(7, 100.0), "nothing was written");
        let written = n(0.1 + 1e-12);
        table.marks().record(table.slot(7).unwrap(), written);
        let word = bounds.word(written);
        assert!(!table.absorbs_report(7, 500.0));
        assert!(!table.absorbs_report(7, word));
        assert!(
            !table.absorbs_report(3, 100.0),
            "an undeclared id absorbs nothing"
        );
    }

    /// A state the unit sets itself forgets the host's writes.
    #[test]
    fn the_narrowing_of_a_write_after_the_written_values_are_cleared_is_reported() {
        let table = written_table();
        let bounds = table.bounds(7).unwrap();
        let written = n(0.1 + 1e-12);
        table.marks().record(table.slot(7).unwrap(), written);
        table.marks().clear_written();
        assert!(!table.absorbs_report(7, bounds.word(written)));
    }
}
