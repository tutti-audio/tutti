//! Typed params of a node: [`ParamTarget`], what a [`Key`]'s param methods
//! return (`lp.cutoff()`), generated from the node's own declaration
//! ([`HasParam`], which `tutti_graph::declare_params!` writes from the node's
//! cells).
//!
//! A node's params are declared once, next to its cells, as a kind and the
//! unit its cell is in (`Cutoff => Hz = &n.cutoff`). A [`Key<N>`] then has a
//! method per kind the node declares:
//! `Key<SvfFilterNode<f32>>::cutoff()` is a [`ParamTarget<Hz>`], and
//! `gate.cutoff()` on a gate does not compile. A node with several params of
//! one kind (a multiband compressor's threshold per band) also has the
//! indexed method, `mb.threshold_at(2)`.
//!
//! The kind and the unit are the declaration's; whether the node *has* the
//! param now (a free-running LFO has a rate, a beat-synced one does not) is
//! the session's, and a write to a param the node lacks is
//! [`ValueErrorKind::NoSuchTarget`](crate::ValueErrorKind::NoSuchTarget).

use std::any::TypeId;

use tutti_graph::{HasParam, HasParams};
use tutti_units::{UnitAddr, UnitParam};

use crate::{Key, ParamTarget};

/// Generates a [`Key`]'s param methods, one pair per kind, from the kind's
/// name.
macro_rules! param_methods {
    ($($name:ident, $at:ident => $kind:ident;)+) => {
        impl<N: ?Sized> Key<N> {$(
            #[doc = concat!(
                "The node's `", stringify!($kind), "` param, in the unit the node declares it in ",
                "(only a node that declares it has this method)."
            )]
            #[must_use]
            pub fn $name(self) -> ParamTarget<<N as HasParam<{ UnitParam::$kind as u16 }>>::Unit>
            where
                N: HasParam<{ UnitParam::$kind as u16 }> + 'static,
            {
                ParamTarget::new(self.erase(), UnitAddr::first(UnitParam::$kind), TypeId::of::<N>())
            }

            #[doc = concat!(
                "The node's `", stringify!($kind), "` param at `index` among its params of ",
                "that kind (`", stringify!($name), "()` is index 0). Whether the node has that ",
                "index is the graph's to say when the reference is used."
            )]
            #[must_use]
            pub fn $at(self, index: u8) -> ParamTarget<<N as HasParam<{ UnitParam::$kind as u16 }>>::Unit>
            where
                N: HasParams<{ UnitParam::$kind as u16 }> + 'static,
            {
                ParamTarget::new(self.erase(), UnitAddr::new(UnitParam::$kind, index), TypeId::of::<N>())
            }
        )+}
    };
}

param_methods! {
    cutoff, cutoff_at => Cutoff;
    q, q_at => Q;
    gain_db, gain_db_at => GainDb;
    wet, wet_at => Wet;
    feedback, feedback_at => Feedback;
    delay_time, delay_time_at => DelayTime;
    rate, rate_at => Rate;
    depth, depth_at => Depth;
    room_size, room_size_at => RoomSize;
    damping, damping_at => Damping;
    threshold, threshold_at => Threshold;
    ratio, ratio_at => Ratio;
    attack, attack_at => Attack;
    release, release_at => Release;
    ceiling, ceiling_at => Ceiling;
    drive, drive_at => Drive;
    makeup, makeup_at => Makeup;
    volume, volume_at => Volume;
    detune, detune_at => Detune;
    stereo_spread, stereo_spread_at => StereoSpread;
    pan, pan_at => Pan;
    balance, balance_at => Balance;
    bit_depth, bit_depth_at => BitDepth;
    hold_factor, hold_factor_at => HoldFactor;
    semitones, semitones_at => Semitones;
    pitch, pitch_at => Pitch;
    distance, distance_at => Distance;
    knee, knee_at => Knee;
    decay, decay_at => Decay;
    diffusion, diffusion_at => Diffusion;
    azimuth, azimuth_at => Azimuth;
    elevation, elevation_at => Elevation;
    spread, spread_at => Spread;
    stereo_width, stereo_width_at => StereoWidth;
    bypass, bypass_at => Bypass;
    solo, solo_at => Solo;
    freeze, freeze_at => Freeze;
    hold, hold_at => Hold;
    range, range_at => Range;
}
