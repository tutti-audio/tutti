//! Bounce a project to a file and measure it: a session and `Bounce`, through
//! **one dependency**.
//!
//! This is the program `tutti-export`'s own `examples/export.rs` is, one layer
//! up (its header reads "What using this crate looks like, end to end"). That
//! one renders a graph it built and sealed itself, with the pipeline's `Job`
//! and targets: it proves `tutti-export` is usable standalone, which is the
//! layering claim this crate must not obscure. This one has a `Session`, which
//! knows the project: the rate, the clock, the graph's latency and tail and the
//! main output's width are derived from it, and a request names
//! only what it wants.
//!
//! The imports group by *subsystem* rather than by crate boundary, which is
//! the actual readability win. The measurement is `tutti-measure`'s, a
//! dev-dependency of this crate.
//!
//! Run: `cargo run -p tutti --features "export,decode-wav" \
//!        --example headless_export -- <outdir>`

use futures_lite::future::block_on;
use std::future::IntoFuture as _;
use tutti::codec::Encoding;
use tutti::graph::{ForkByClone, Node};
use tutti::{
    Amplitude, Bounce, ChannelLayout, Format, Hz, Interleaved, MainOutput, Pcm, Range, SampleRate,
    Seconds, Session,
};
use tutti::{ToFile, ToMemory};
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_measure::{LoudnessConfig, measure_loudness};
use tutti_nodes::testing::{Const, Osc};

/// The rate every render below runs at.
const RATE: SampleRate = SampleRate::SR_48K;

/// A session whose outputs hear `node`, one second of a project.
fn session_with(node: impl Node + Clone + 'static) -> Session {
    let mut session = Session::builder()
        .sample_rate(RATE)
        .build()
        .expect("a session builds");
    let node = session.graph().add(ForkByClone(node));
    session.graph().connect(node, MainOutput).unwrap();
    session
}

/// A 440 Hz tone at -12 dBFS, in stereo.
fn tone() -> Session {
    session_with(
        Osc::sine(Hz::new(440.0))
            .with_amplitude(Amplitude::new(0.25))
            .with_layout(ChannelLayout::STEREO),
    )
}

/// A mono project, to show the channel fold.
fn mono_tone() -> Session {
    session_with(Const::mono(0.5))
}

/// The format of every file here.
const FORMAT: Format = Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24)));

/// The first `seconds` of the project.
fn first(seconds: f64) -> Bounce {
    Bounce::new().range(Range::Seconds(Seconds::new(0.0), Seconds::new(seconds)))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir)?;

    // 1. A project to a file.
    let path = dir.join("tone.wav");
    let mut session = tone();
    let rendered = block_on(
        session
            .bounce(first(1.0).main_output(ToFile::new(
                Encoding::new(FORMAT),
                // Replaces the file an earlier run of this example wrote.
                tutti::codec::ToFile::new(&path).replace(),
            )))?
            .into_future(),
    );
    let written = session.apply(rendered)?.into_main_output();
    println!("wrote {} ({written:?})", path.display());

    // 2. The same project into memory, and a measurement of it.
    let mut session = tone();
    let rendered = block_on(
        session
            .bounce(first(1.0).main_output(ToMemory::new()))?
            .into_future(),
    );
    let rendered = session.apply(rendered)?.into_main_output();
    let flat = rendered.interleaved();
    // `tutti` exports no path to `Bed`, so the placement is the default layout for
    // two channels, which is a stereo bed.
    let cfg = LoudnessConfig::new(tutti_buffers::Bed::Stereo.topology());
    let loudness = measure_loudness(
        &cfg,
        Interleaved::new(&flat, ChannelLayout::STEREO).unwrap(),
        rendered.sample_rate(),
    )
    .expect("stereo is meterable");
    println!(
        "loudness: {:.1} LUFS, true peak {:.1} dBTP",
        loudness.lufs.get(),
        loudness.true_peak.get()
    );

    // 3. A mono project into a stereo file: the fold, which the export layer
    //    does for you and the graph does not.
    let mono_path = dir.join("mono-to-stereo.wav");
    let mut session = mono_tone();
    let rendered = block_on(
        session
            .bounce(first(0.5).main_output(ToFile::new(
                Encoding::new(FORMAT).width(ChannelLayout::STEREO),
                tutti::codec::ToFile::new(&mono_path).replace(),
            )))?
            .into_future(),
    );
    session.apply(rendered)?;
    println!("wrote {}", mono_path.display());

    Ok(())
}
