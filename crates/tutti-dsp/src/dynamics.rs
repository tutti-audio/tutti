//! Dynamics algorithms: the gain curves, the compressor, gate, limiter and brickwall over
//! buffers views, the multiband compressor, and the limiter's lookahead ring.

mod brickwall;
mod compressor;
mod curves;
mod gate;
mod key_filter;
mod key_source;
mod limiter;
mod lookahead;
mod multiband;
mod shared;
mod window;

pub use brickwall::{BrickwallBank, BrickwallControls, BrickwallLanes};
pub use compressor::{CompressorBank, CompressorControls, CompressorLanes};
pub use curves::{
    LimiterCurve, amplitude_to_db, compute_compressor_gain_reduction, compute_limiter_gain,
};
pub use gate::{GateBank, GateControls, GateLanes};
pub use key_filter::KeyFilter;
pub use key_source::KeySource;
pub use limiter::{LimiterBank, LimiterControls, LimiterLanes, Lookahead, LookaheadError};
pub use multiband::{
    BandReductions, BandSettings, BankShape, MultibandBank, MultibandLanes, MultibandSettings,
};
pub use shared::GAIN_GLIDE;
