//! The block seam under every clock, without a sound card: the output block, the
//! stream lifecycle and the restart path, run through `ManualStreamDriver`, and the timer driver on its own thread.

mod fixtures;

mod lifecycle;
mod output_block;
mod process_audio;
mod timer;
