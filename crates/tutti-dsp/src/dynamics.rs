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
mod leveller;
mod limiter;
mod lookahead;
mod multiband;
mod sliding_min;
mod transient;

pub use brickwall::{BrickwallBank, BrickwallControls, BrickwallSignals};
pub use compressor::{CompressorBank, CompressorControls, CompressorSignals};
pub use curves::{
    LimiterCurve, amplitude_to_db, compute_compressor_gain_reduction, compute_limiter_gain,
};
pub use dynamic_eq::{DynamicEqBank, DynamicEqControls, DynamicMode};
pub use gain_pass::GAIN_RAMP;
pub(crate) use gain_pass::{floored, key_levels};
pub use gate::{GateBank, GateControls, GateSignals};
pub use key_filter::KeyFilter;
pub use key_source::KeySource;
pub use leveller::{LevellerBank, LevellerControls};
pub use limiter::{
    LimiterBank, LimiterControls, LimiterSignals, Lookahead, LookaheadError, PeakDetection,
    TruePeakError,
};
pub use multiband::{
    BandReductions, BandSettings, BankShape, MultibandBands, MultibandBandsError, MultibandBank,
    MultibandSettings, MultibandSignals,
};
pub use transient::{TransientBank, TransientControls};
