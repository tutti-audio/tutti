//! Decoding audio files: the one module that names `symphonia`.
//!
//! Decoding is plain functions and types of our own (`read_wave`,
//! `read_wave_metadata`, `FileIn`, `DecodeError`); no `symphonia` type
//! appears in a signature or an error, so the library behind them can be
//! swapped by editing this module alone. `cargo xtask check-layout`
//! holds that line.

#[cfg(decode)]
mod error;
mod extensions;
#[cfg(decode)]
mod file_in;
#[cfg(decode)]
mod placement;
#[cfg(decode)]
mod read;
#[cfg(decode)]
mod riff_guard;

pub use extensions::{can_decode, decodable_extensions};
#[cfg(decode)]
pub use file_in::FileIn;
#[cfg(decode)]
pub use read::{WaveMetadata, read_wave, read_wave_metadata};
