//! **How a heavy session's blocks sit against their deadline**, serial and on
//! the worker pool: each block's load is the bench's own timing of
//! `Session::render` over the block's budget, and the blocks over budget are
//! the engine's own count (`Session::blocks`).
//!
//! Render-ahead (an anticipative partition rendered a block or more early)
//! only helps when blocks miss their deadline because a *few* cost far more
//! than the rest, not because the average is too high. This harness reports
//! both, per block: mean, p99 and max load, blocks over budget, and max/mean
//! (the spike indicator). It is a percentile harness, not a criterion
//! benchmark, for the reason `tutti-plugin-host`'s `vst2_in_process` bench gives:
//! criterion's outlier rejection discards the tail, which is the point here.
//!
//! # The scene
//!
//! An offline session (`Session::builder().build()`), 48 kHz stereo,
//! rolling:
//!
//! - 8 `PolySynth` instruments, saw through a Moog ladder at 1.2 kHz, 16
//!   notes held on each (128 voices), each into a `BusStripNode`;
//! - 4 sampler tracks, each a `VoicePool` of 8 memory voices on a 20 s
//!   tone, each voice transposed by its own interval (-347 to +428 cents),
//!   each into a `BusStripNode`;
//! - one send per track (a second `BusStripNode` at -12 dB), summed by the
//!   graph (a fan-in on the reverb's inputs) into a stereo `ConvolverNode`
//!   reverb (2 s noise-tail IRs, a 512-frame partition) and its return strip;
//! - the 12 track strips and the return summed by the graph (a fan-in on the
//!   master strip's inputs) into a master strip and a `LimiterNode`.
//!
//! Four variants, each measured on its own:
//!
//! - **`varispeed`**, the main scene: the sampler voices transpose by
//!   varispeed (`Playback::speed`), a resampled read.
//! - **`vocoder`**: the same intervals as a pitch shift (`Playback::pitch`),
//!   which runs each voice through the sampler's phase vocoder (a 2048-point
//!   FFT every 512 output frames): 32 pitch shifters.
//! - **`no-reverb`**: the main scene without the convolver and its send
//!   bus, the control for what the reverb costs.
//! - **`vocoder-dry`**: `vocoder` without the convolver and its send bus,
//!   the vocoder's cost on its own.
//!
//! Each configuration renders 1 s to warm up (notes attack, voices start),
//! then 10 s measured, block by block, at 64 and 128 frames, serial
//! (`Workers::SERIAL`) and on a pool of one thread per core
//! (`Workers::PerCore`).
//!
//! # Where the spikes land
//!
//! The convolver and the vocoder both do their FFT work once per 512
//! frames. The harness prints, per configuration, the mean load at each
//! block position within the 512-frame cycle (`phase`: position 0 is the
//! block starting at a multiple of 512). Measured here, the convolver's work
//! lands on the block ending at a multiple of 512 (the last position) and,
//! with the voices starting at frame 0, the vocoder's on the block ending
//! 256 frames into the cycle, so the two show at different positions. The
//! variants separate them too: `varispeed` against `no-reverb` is the
//! convolver's share, `vocoder-dry` against `no-reverb` the vocoder's, and
//! `vocoder` both at once.
//!
//! The last column is the 1-minute load average after the configuration
//! (the bench's own threads included), so a figure taken beside other work
//! says so.
//!
//! # What it does not measure
//!
//! - **OS scheduling jitter of a real device thread**: this renders on an
//!   ordinary thread, back to back, with no real-time priority and no wait
//!   for a device; a device callback's wake-up and preemption are not here.
//! - **Out-of-process plugins**: their IPC round trip and the server's
//!   scheduling are not in the scene.
//! - **Third-party plugin spikes**: every node is tutti's own, whose
//!   per-block work is bounded and allocation-free.
//!
//! Offline, a block "over budget" only means its render took longer than
//! its frames last at 48 kHz; nothing drops out.
//!
//! ```text
//! cargo bench -p tutti-session --bench render_load --features synth,sampler,midi
//! ```

use std::sync::Arc;
use tutti_buffers::{ChannelLayout, ChannelTopology, Interleaved, InterleavedMut, MaxBlock, Mono};
use tutti_nodes::{
    BusStripConfig, BusStripNode, ConvolverConfig, ConvolverNode, ImpulseResponses, IrLayout,
    LimiterConfig, LimiterControls, LimiterNode,
};
use tutti_sampler::testing::placed_source;

/// How the sampler voices transpose, and whether the reverb is in the
/// scene.
#[derive(Clone, Copy)]
enum Variant {
    Varispeed,
    Vocoder,
    NoReverb,
    VocoderDry,
}

impl Variant {
    fn label(self) -> &'static str {
        match self {
            Self::Varispeed => "varispeed",
            Self::Vocoder => "vocoder",
            Self::NoReverb => "no-reverb",
            Self::VocoderDry => "vocoder-dry",
        }
    }

    /// The variant's scene, for the printed legend.
    fn scene(self) -> &'static str {
        match self {
            Self::Varispeed => "sampler voices by varispeed, convolution send",
            Self::Vocoder => "sampler voices through the phase vocoder, convolution send",
            Self::NoReverb => "sampler voices by varispeed, no reverb",
            Self::VocoderDry => "sampler voices through the phase vocoder, no reverb",
        }
    }

    /// Whether the sampler voices pitch-shift through the phase vocoder
    /// (otherwise they transpose by varispeed).
    fn vocoder(self) -> bool {
        matches!(self, Self::Vocoder | Self::VocoderDry)
    }

    /// Whether the scene has the convolution reverb and its sends.
    fn reverb(self) -> bool {
        matches!(self, Self::Varispeed | Self::Vocoder)
    }
}

use tutti_graph::{Cost, Cx, Event, Io, Node, Offset, Prepare, Shape, Status, Unforkable};
use tutti_platform::Instant;
use tutti_polysynth::{Adsr, FilterType, LadderSaturation, OscillatorType, PolySynth, SynthConfig};
use tutti_rt::SplitMix64;
use tutti_sampler::{Playback, SlotId, VoicePool, VoiceWindow};
use tutti_session::{InputPort, MainOutput, OutputPort, Session, Workers};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Ump, Voice2};
use tutti_units::{
    Amplitude, Beat, Cents, Db, Hz, PlaybackRate, Resonance, SampleRate, Samples, Seconds,
};

const RATE: f64 = 48_000.0;
const SYNTHS: usize = 8;
const NOTES: usize = 16;
const SAMPLER_TRACKS: usize = 4;
const SAMPLER_VOICES: usize = 8;
const WARM: Seconds = Seconds::new(1.0);
const MEASURED: Seconds = Seconds::new(10.0);

/// Holds `notes` notes on its event output: one note-on per block from the
/// first, then nothing.
struct Hold {
    base: u8,
    notes: usize,
    sent: usize,
}

impl Hold {
    const SHAPE: Shape = Shape::EMPTY.events(0, 1).cost(Cost::Light);
    const OUT: tutti_graph::OutEvents = Self::SHAPE
        .event_output(0)
        .expect("one event output is declared");
}

impl Node for Hold {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        if self.sent < self.notes {
            let note = Ump::from(Message::midi2(
                MidiChannel::FIRST,
                Voice2::note_on(
                    NoteNumber::of(
                        // Spread over four octaves: no two voices share a pitch.
                        self.base + (self.sent as u8 * 3) % 48,
                    ),
                    NoteVelocity::new(0xC000),
                ),
            ));
            let at = Offset::of(Samples::new(0), Samples::new(io.frames()));
            if io.event_out(Self::OUT).push(Event::midi(at, note)).is_ok() {
                self.sent += 1;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Deterministic white noise in `-1..1` from a seeded stream, so every run
/// renders the same scene.
fn noise(rng: &mut SplitMix64) -> f32 {
    ((rng.next_u32() >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
}

/// A 2 s exponentially decaying noise tail: a plain reverb IR.
fn ir(seed: u64) -> Vec<f32> {
    let mut s = SplitMix64::new(seed);
    let len = Seconds::new(2.0).to_samples(SampleRate::of(RATE)).get();
    (0..len)
        .map(|i| noise(&mut s) * (-6.9 * i as f32 / len as f32).exp() * 0.05)
        .collect()
}

fn connect_stereo(
    s: &mut Session,
    to: impl Into<tutti_graph::NodeKey>,
    first_port: usize,
    from: impl Into<tutti_graph::NodeKey>,
) {
    let (to, from) = (to.into(), from.into());
    for c in 0..2 {
        s.graph()
            .set_sources(
                InputPort::new(to, first_port + c),
                [OutputPort::new(from, c)],
            )
            .unwrap();
    }
}

/// Adds `from`'s stereo pair to the sources of `to`'s first two ports: the
/// graph sums what several calls add.
fn add_stereo(
    s: &mut Session,
    to: impl Into<tutti_graph::NodeKey>,
    from: impl Into<tutti_graph::NodeKey>,
) {
    let (to, from) = (to.into(), from.into());
    for c in 0..2 {
        s.graph()
            .connect(OutputPort::new(from, c), InputPort::new(to, c))
            .unwrap();
    }
}

/// The scene the module docs describe, on `workers` render threads, its
/// transport rolling.
fn scene(block: usize, workers: Workers, variant: Variant) -> Session {
    let mut s = Session::builder()
        .render_workers(workers)
        .sample_rate(SampleRate::of(RATE))
        .max_block(MaxBlock::of(Samples::new(block)))
        .build()
        .expect("builds");

    let mut strips = Vec::new();
    let mut sends = Vec::new();
    for i in 0..SYNTHS {
        let synth = PolySynth::from(
            SynthConfig::new()
                .polyphony(std::num::NonZeroUsize::new(NOTES).expect("at least one voice"))
                .oscillator(OscillatorType::Saw)
                .filter(FilterType::Moog {
                    cutoff: Hz::new(1_200.0),
                    resonance: Resonance::new(0.3),
                    saturation: LadderSaturation::PerStage,
                })
                .envelope(Adsr::new(
                    Seconds::new(0.005),
                    Seconds::new(0.2),
                    Amplitude::new(0.7),
                    Seconds::new(0.3),
                )),
        );
        let synth = s.graph().add(synth);
        let hold = s.graph().add(Unforkable(Hold {
            base: 36 + i as u8,
            notes: NOTES,
            sent: 0,
        }));
        s.graph()
            .connect(hold.event_output(0), synth.event_input(0))
            .unwrap();
        strips.push(synth.erase());
    }

    let mut tone = tutti_buffers::Wave::new(1, SampleRate::of(RATE));
    let mut seed = SplitMix64::new(0x1234_5678);
    let frames = Seconds::new(20.0).to_samples(SampleRate::of(RATE)).get();
    for i in 0..frames {
        let t = i as f32 / RATE as f32;
        let x = (std::f32::consts::TAU * 220.0 * t).sin() * 0.3 + noise(&mut seed) * 0.05;
        tone.append(Interleaved::new(&[x], tone.layout()).unwrap())
            .unwrap();
    }
    let tone = Arc::new(tone);
    for t in 0..SAMPLER_TRACKS {
        let mut pool = VoicePool::new();
        for v in 0..SAMPLER_VOICES {
            pool.insert(
                SlotId(v as u128 + 1),
                placed_source(Arc::clone(&tone), VoiceWindow::from(Beat::new(0.0))),
                {
                    let k = (t * SAMPLER_VOICES + v) as f64;
                    let interval = Cents::new(-350.0 + 25.0 * k + 3.0);
                    let play = Playback {
                        gain: Amplitude::new(0.1),
                        ..Default::default()
                    };
                    if variant.vocoder() {
                        Playback {
                            pitch: interval,
                            ..play
                        }
                    } else {
                        Playback {
                            speed: PlaybackRate::clamped(interval.to_pitch_ratio()),
                            ..play
                        }
                    }
                },
            )
            .expect("a finite gain");
        }
        let pool = s.graph().add(pool);
        strips.push(pool.erase());
    }

    // Each source into its channel strip, and (with the reverb) a send off
    // the source.
    let reverb = variant.reverb();
    let tracks: Vec<_> = strips
        .into_iter()
        .map(|src| {
            let strip = s.graph().add(BusStripNode::from(BusStripConfig::new(
                ChannelTopology::STEREO,
            )));
            connect_stereo(&mut s, strip, 0, src);
            if reverb {
                let send = BusStripNode::from(BusStripConfig::new(ChannelTopology::STEREO));
                send.volume().store(Db::new(-12.0).to_amplitude());
                let send = s.graph().add(send);
                connect_stereo(&mut s, send, 0, src);
                sends.push(send);
            }
            strip
        })
        .collect();

    let mut sums = tracks;
    if reverb {
        let (l, r) = (ir(1), ir(2));
        let conv = s.graph().add(ConvolverNode::from(
            ConvolverConfig::new(
                ChannelTopology::STEREO,
                IrLayout::PerChannel,
                Arc::new(ImpulseResponses::new(&[Mono::new(&l), Mono::new(&r)])),
            )
            .unwrap()
            .block_size(Samples::new(PARTITION)),
        ));
        for send in &sends {
            add_stereo(&mut s, conv, *send);
        }
        let ret = s.graph().add(BusStripNode::from(BusStripConfig::new(
            ChannelTopology::STEREO,
        )));
        connect_stereo(&mut s, ret, 0, conv);
        sums.push(ret);
    }

    let master = s.graph().add(BusStripNode::from(BusStripConfig::new(
        ChannelTopology::STEREO,
    )));
    for strip in &sums {
        add_stereo(&mut s, master, *strip);
    }
    let limiter = s.graph().add(LimiterNode::<f32>::from(
        LimiterConfig::new(ChannelTopology::STEREO).controls(LimiterControls {
            threshold: Db::new(-1.0),
            ceiling: Db::new(0.0),
            ..LimiterControls::DEFAULT
        }),
    ));
    connect_stereo(&mut s, limiter, 0, master);
    for c in 0..2 {
        s.graph()
            .set_sources(MainOutput::channel(c), [OutputPort::new(limiter, c)])
            .unwrap();
    }
    let _ = s.update();
    s.play().expect("room");
    s
}

/// The 512-frame cycle: the convolver's partition, and the vocoder's
/// output hop.
const PARTITION: usize = 512;

struct Figures {
    mean: f32,
    p99: f32,
    max: f32,
    over: usize,
    blocks: usize,
    /// Mean load at each block position within the 512-frame cycle
    /// (position 0 starts at a multiple of 512).
    phase: Vec<f32>,
    peak_out: f32,
}

fn measure(block: usize, workers: Workers, variant: Variant) -> Figures {
    let mut s = scene(block, workers, variant);
    let counter = s.blocks().expect("an offline engine");
    let mut out = vec![0.0f32; block * 2];
    let blocks = |span: Seconds| span.to_samples(SampleRate::of(RATE)).get().div_ceil(block);
    let warm = blocks(WARM);
    for _ in 0..warm {
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
    }
    let n = blocks(MEASURED);
    let mut loads = Vec::with_capacity(n);
    let mut peak_out = 0.0f32;
    let before = counter.get();
    let budget = Seconds::new(block as f64 / RATE);
    for _ in 0..n {
        let start = Instant::now();
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
        loads.push((Seconds::new(start.elapsed().as_secs_f64()) / budget) as f32);
        peak_out = out.iter().fold(peak_out, |m, x| m.max(x.abs()));
    }
    let counted = counter.get();
    let over = (counted.over_budget - before.over_budget) as usize;
    assert_eq!(
        counted.blocks - before.blocks,
        n as u64,
        "the engine timed every block rendered"
    );
    let positions = (PARTITION / block).max(1);
    let mut phase = vec![(0.0f32, 0usize); positions];
    for (k, &l) in loads.iter().enumerate() {
        let p = &mut phase[(warm + k) % positions];
        p.0 += l;
        p.1 += 1;
    }
    let mean = loads.iter().sum::<f32>() / n as f32;
    let max = loads.iter().copied().fold(0.0, f32::max);
    let mut sorted = loads;
    sorted.sort_by(f32::total_cmp);
    let p99 = sorted[(n * 99).div_ceil(100) - 1];
    Figures {
        mean,
        p99,
        max,
        over,
        blocks: n,
        phase: phase.iter().map(|&(sum, n)| sum / n as f32).collect(),
        peak_out,
    }
}

/// The 1-minute load average, where the OS reports one: a figure taken
/// beside other work is not a quiet machine's, and the reader should see
/// which it was.
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|l| l.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "n/a".to_owned())
}

/// Every variant, in the order measured.
const VARIANTS: [Variant; 4] = [
    Variant::Varispeed,
    Variant::NoReverb,
    Variant::Vocoder,
    Variant::VocoderDry,
];

fn main() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!(
        "render_load: {SYNTHS} synths x {NOTES} notes, {SAMPLER_TRACKS} sampler tracks x \
         {SAMPLER_VOICES} voices, 48 kHz; {cores} cores available"
    );
    for variant in VARIANTS {
        println!("  {:>11}: {}", variant.label(), variant.scene());
    }
    println!(
        "{:>5} {:>11} {:>7} {:>7} {:>7} {:>8} {:>11} {:>8} {:>8}  phase (mean load by position in 512 frames)",
        "block", "variant", "workers", "mean", "p99", "max", "over/blocks", "max/mean", "loadavg"
    );
    for variant in VARIANTS {
        for block in [64usize, 128] {
            for (label, workers) in [("1", Workers::SERIAL), ("cores", Workers::PerCore)] {
                let f = measure(block, workers, variant);
                // A silent render would measure nothing worth reading.
                assert!(f.peak_out > 0.0, "the scene sounds");
                let phase: Vec<String> = f.phase.iter().map(|l| format!("{l:.3}")).collect();
                println!(
                    "{:>5} {:>11} {:>7} {:>7.3} {:>7.3} {:>8.3} {:>11} {:>8.2} {:>8}  {}",
                    block,
                    variant.label(),
                    label,
                    f.mean,
                    f.p99,
                    f.max,
                    format!("{}/{}", f.over, f.blocks),
                    f.max / f.mean,
                    loadavg(),
                    phase.join(" "),
                );
            }
        }
    }
}
