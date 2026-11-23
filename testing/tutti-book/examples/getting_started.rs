//! Getting started: a 440 Hz sine, two seconds of it, written to a FLAC file.
//!
//! ```text
//! cargo run -p tutti-book --example getting_started -- <dir>
//! ```
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "an example is a program: its output is its job"
)]

use std::path::Path;

// ANCHOR: imports
use futures_lite::future::block_on;
use std::future::IntoFuture as _;
use tutti::nodes::{OscillatorConfig, OscillatorNode, OscillatorWave};
use tutti::{
    Amplitude, Bounce, ChannelLayout, Format, Hz, MainOutput, SampleRate, Seconds, Session,
};
// ANCHOR_END: imports

// ANCHOR: session
/// A session whose output plays a 440 Hz sine at a quarter of full scale.
fn tone() -> Result<Session, tutti::Error> {
    // No sound card: the session renders when we ask it to.
    let mut session = Session::builder().sample_rate(SampleRate::SR_48K).build()?;

    // An A at 440 Hz, a quarter of full scale.
    let sine = session.graph().add(OscillatorNode::from(
        OscillatorConfig::new(OscillatorWave::Sine, Hz::new(440.0)).level(Amplitude::new(0.25)),
    ));

    // One mono output, heard in the left and the right channel.
    session
        .graph()
        .connect(sine.output(0), MainOutput::channel(0))?;
    session
        .graph()
        .connect(sine.output(0), MainOutput::channel(1))?;
    Ok(session)
}
// ANCHOR_END: session

// ANCHOR: bounce
/// The first two seconds of the session, in stereo.
fn two_seconds() -> Bounce {
    Bounce::new()
        .range(Seconds::new(0.0)..Seconds::new(2.0)) // from 0 s to 2 s
        .channels(ChannelLayout::STEREO) // left and right
}
// ANCHOR_END: bounce

// ANCHOR: main
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    std::fs::create_dir_all(&dir)?;
    let path = Path::new(&dir).join("getting_started.flac");

    // The file's name picks its format: `.flac` is FLAC.
    let mut session = tone()?;
    let rendering = session.bounce(two_seconds().main_output(&path, Format::from_path(&path)?))?;
    // The session renders on a thread of its own; we wait for it, then hand
    // what it made back to the session.
    let rendered = block_on(rendering.into_future());
    session.apply(rendered)?;

    println!("wrote {}", path.display());
    Ok(())
}
// ANCHOR_END: main

#[cfg(test)]
mod tests {
    use super::*;

    /// After the connection's fade-in, both channels carry the sine at its
    /// level: a peak of 0.25 and an RMS of 0.25 / √2.
    #[test]
    fn the_bounce_is_a_sine_at_its_level_on_both_channels() {
        let mut session = tone().unwrap();
        let rendering = session.bounce(two_seconds().main_output_buffers()).unwrap();
        let rendered = session
            .apply(block_on(rendering.into_future()))
            .unwrap()
            .into_main_output();
        let frames = rendered.interleaved();
        let settled = &frames[frames.len() / 2..];
        for channel in 0..2 {
            let x: Vec<f32> = settled.iter().skip(channel).step_by(2).copied().collect();
            let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let rms = (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
            assert!((peak - 0.25).abs() < 1e-3, "channel {channel} peak {peak}");
            assert!(
                (rms - 0.25 / 2f32.sqrt()).abs() < 1e-3,
                "channel {channel} rms {rms}"
            );
        }
    }
}
