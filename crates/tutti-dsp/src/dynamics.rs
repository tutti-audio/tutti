//! Dynamics algorithms: the gain curves, the compressor, gate, limiter and brickwall over
//! buffers views, the dynamic EQ band, the multiband compressor, the transient shaper, and the
//! limiter's lookahead ring.

mod brickwall;
mod compressor;
mod curves;
mod dynamic_eq;
mod gain_pass;
mod gate;
mod key_filter;
mod key_source;
mod limiter;
mod lookahead;
mod multiband;
mod sliding_min;
mod transient;

pub use brickwall::{BrickwallBank, BrickwallControls, BrickwallLanes};
pub use compressor::{CompressorBank, CompressorControls, CompressorLanes};
pub use curves::{
    LimiterCurve, amplitude_to_db, compute_compressor_gain_reduction, compute_limiter_gain,
};
pub use dynamic_eq::{DynamicEqBank, DynamicEqControls, DynamicMode};
pub use gain_pass::GAIN_GLIDE;
pub(crate) use gain_pass::{floored, key_levels};
pub use gate::{GateBank, GateControls, GateLanes};
pub use key_filter::KeyFilter;
pub use key_source::KeySource;
pub use limiter::{
    LimiterBank, LimiterControls, LimiterLanes, Lookahead, LookaheadError, PeakDetection,
    TruePeakError,
};
pub use multiband::{
    BandReductions, BandSettings, BankShape, MultibandBank, MultibandLanes, MultibandSettings,
};
pub use transient::{TransientBank, TransientControls};
