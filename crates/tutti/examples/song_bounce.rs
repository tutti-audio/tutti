//! A small song, end to end, through **one import**: a sampler into a filter
//! into a reverb, an LFO sweeping the cutoff, the transport played, and eight
//! beats bounced to a WAV file (the reverb's tail rings on past the eighth).
//!
//! ```text
//! cargo run -p tutti --features "export,sampler,decode-wav" --example song_bounce -- <outdir>
//! ```
//!
//! `tutti`'s root carries the session, the units, the sampler's `Clip`,
//! `Bounce` and `Format`; the `Lfo` is `tutti::modulation::Lfo`; the nodes are `tutti::nodes`; the two bus names a fold
//! needs are `tutti::session::` paths. Every fallible call returns the one
//! `Error`, so `?` runs the whole program.

use futures_lite::future::block_on;
use std::future::IntoFuture as _;
use tutti::modulation::Lfo;
use tutti::nodes::{FreeverbConfig, FreeverbNode, SvfFilterConfig, SvfFilterNode, SvfType};
use tutti::{
    Amplitude, Beat, Bounce, Bpm, ChannelLayout, Clip, Depth, Error, Format, Hz, MainOutput,
    MaxBlock, Pcm, Q, RouteSpec, SampleRate, Samples, Session, SessionEvent, Wave, Written,
};
use tutti_buffers::Interleaved;

const RATE: SampleRate = SampleRate::SR_48K;

/// What `Session::update` found: faults and dropouts (a refused commit, a
/// plugin that died, a lost block). A host logs them; here they are printed.
fn report(events: Vec<SessionEvent>) {
    for event in events {
        eprintln!("session event: {event:?}");
    }
}

/// Two seconds of a 110 Hz saw, mono: four beats at 120 BPM, and enough
/// harmonics for the filter to sweep.
fn saw() -> Wave {
    let mut wave = Wave::new(1, RATE);
    for i in 0..96_000u32 {
        let phase = (i as f32 * 110.0 / RATE.get() as f32).fract();
        wave.append(Interleaved::new(&[0.4 * (2.0 * phase - 1.0)], wave.layout()).unwrap())
            .unwrap();
    }
    wave
}

fn main() -> Result<(), Error> {
    let dir = std::env::args_os()
        .nth(1)
        .map_or_else(std::env::temp_dir, Into::into);
    let written = song(&dir)?;
    println!("wrote {} ({} bytes)", written.path.display(), written.bytes);
    Ok(())
}

/// The song, bounced into `dir`.
fn song(dir: &std::path::Path) -> Result<Written, Error> {
    std::fs::create_dir_all(dir)?;

    let mut s = Session::builder()
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()?;
    s.set(
        s.transport_handle()
            .tempo()
            .to(tutti::tempo::TempoMap::constant(Bpm::of(120.0)).expect("120 BPM is a map")),
    )
    .expect("not following");

    // The sampler: the same mono wave on beats 0 and 4, each a node as it
    // stands, placed at stereo to join the stereo chain.
    let wave = std::sync::Arc::new(saw());
    let first = s.graph().add(
        Clip::new(wave.clone())?
            .channels(ChannelLayout::STEREO)?
            .at(Beat::new(0.0)),
    );
    let second = s.graph().add(
        Clip::new(wave)?
            .channels(ChannelLayout::STEREO)?
            .at(Beat::new(4.0))
            .gain(Amplitude::new(0.8))
            .expect("a finite gain"),
    );

    // A stereo low-pass, then a reverb whose input is mono: that width change
    // is a bus edge, never a truncation (`connect` would refuse it).
    let filter = s.graph().add(SvfFilterNode::from(
        SvfFilterConfig::new(ChannelLayout::STEREO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    let reverb = s
        .graph()
        .add(FreeverbNode::from(FreeverbConfig::general_midi()));
    s.graph().connect(first, filter)?;
    s.graph().connect(second, filter)?; // adds: both clips sum into the filter
    s.graph().connect(filter.bus_out(0), reverb.bus_in(0))?;
    for c in 0..2 {
        s.graph().connect(filter.out(c), MainOutput::channel(c))?;
        s.graph().connect(reverb.out(c), MainOutput::channel(c))?;
    }

    // An LFO on the cutoff: 1 Hz across 200 Hz to 3 kHz.
    let cutoff = filter.cutoff();
    let mut m = s.mods_mut();
    m.range(cutoff, Hz::new(200.0)..Hz::new(3_000.0))?;
    let wobble = m.lfo(Lfo::sine().rate(Hz::new(1.0)));
    m.route(RouteSpec::new(wobble, cutoff, Depth::of(0.5)))?;

    // Commit the graph and the lanes, then roll the transport.
    report(s.update());
    report(s.update());
    s.play()?;

    // Eight beats, to a 24-bit WAV.
    let path = dir.join("song.wav");
    let rendering = s.bounce(
        Bounce::new()
            .range(Beat::new(0.0)..Beat::new(8.0))
            .master(&path, Format::Wav(Pcm::Int24)),
    )?;
    // The render runs on a thread the session starts; an app awaits it on its own
    // executor, here the simplest one.
    let rendered = block_on(rendering.into_future());
    Ok(s.apply(rendered)?.into_master())
}

#[cfg(test)]
mod tests {
    /// **The song bounces eight beats and more, and is not silence.** At 120
    /// BPM eight beats are 192 000 frames; the file (24-bit stereo, 6 bytes a
    /// frame) holds at least that, plus the reverb's tail, and its samples are
    /// not all zero.
    #[test]
    fn the_song_bounces_and_is_heard() {
        let dir = tempfile::tempdir().unwrap();
        let written = super::song(dir.path()).expect("the song renders");
        assert!(written.bytes >= 192_000 * 6, "{} bytes", written.bytes);
        let file = std::fs::read(&written.path).unwrap();
        assert!(file[44..].iter().any(|&b| b != 0), "silence");
    }
}
