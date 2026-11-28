//! One render pass feeding several outputs: files
//! of different formats, plan taps, a gain shared between files, a rename
//! that fails half way, and the outputs that contradict each other.
//!
//! Every expectation is derived from the signal, not from another render: the
//! planes of a [`BufferTarget`] output of the same pass are the audio the
//! files are checked against (through each format's own quantizer), a tap on a
//! node is checked against the figure its gain makes of the node's constant,
//! and files that share a gain are checked against the relation that gain
//! implies (the stems sum to the main output).

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use tutti_buffers::ChannelLayout;
use tutti_buffers::ChannelSet;
use tutti_buffers::FloatWidth;
use tutti_buffers::{Bed, ChannelTopology};
use tutti_codec::Encoding;
use tutti_codec::IntPcm;
use tutti_codec::ToFile;
use tutti_codec::{FlacLevel, Format, IntDepth, Pcm};
use tutti_export::{
    BufferTarget, Control, DoneError, FileTarget, Gain, Job, Normalize, Output, Outputs, Placement,
    RenderError, RenderGraph, Setup, TailPolicy, TargetError, Written, render,
};
use tutti_fixtures::WavePlanes;
use tutti_graph::{
    Cx, Editor, Executor, ForkByClone, GraphBuilder, Io, Node, NodeKey, OutputPort, PortSource,
    Prepare, Shape, Status, TapId, TapOptions, TapReader,
};
use tutti_nodes::testing::{Const, Osc};
use tutti_timeline::FrozenClock;
use tutti_units::{Amplitude, Db, Hz, SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_48K;

/// A stereo node that scales its input by a constant: the known gain.
#[derive(Clone)]
struct Scale(f32);

impl Node for Scale {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..2 {
            for (o, i) in outs.get(c).iter_mut().zip(ins.get(c)) {
                *o = *i * self.0;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn stereo_out(g: &mut GraphBuilder, node: NodeKey) {
    g.connect_output(node, 0, 0).connect_output(node, 1, 1);
}

/// A built graph still open to edits: taps are opened on its editor, and
/// [`Rig::assemble`] seals it into the `RenderGraph` a render takes.
struct Rig {
    editor: Editor,
    executor: Executor,
}

fn rig(g: GraphBuilder) -> Rig {
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    Rig { editor, executor }
}

impl Rig {
    /// A tap on both channels of `node`, on the outputs' time, committed.
    fn tap(&mut self, id: u64, node: NodeKey) -> TapReader {
        let sources = (0..2)
            .map(|port| PortSource::Node(OutputPort::new(node, port)))
            .collect();
        let reader = self
            .editor
            .open_tap(TapId::of(id), sources, TapOptions::new(Samples::new(2048)))
            .expect("the node exists");
        self.editor.commit().expect("commits");
        self.executor.apply_pending();
        self.editor.collect();
        reader
    }

    fn assemble(self) -> RenderGraph {
        RenderGraph::assemble(self.editor, self.executor, Setup::new()).expect("built together")
    }
}

fn finish(g: GraphBuilder) -> RenderGraph {
    rig(g).assemble()
}

fn stereo_placement() -> Placement {
    Placement::new(tutti_buffers::Bed::Stereo.topology())
}

fn job(frames: usize) -> Job {
    Job::new(
        RATE,
        Samples::new(frames),
        Samples::new(0),
        TailPolicy::Frames(Samples::ZERO),
    )
    .expect("a render")
}

/// A WAV target of `pcm`, rounded without dither so a render's values are exact.
fn wav(path: &std::path::Path, pcm: Pcm) -> FileTarget {
    let pcm = match pcm {
        Pcm::Int(int) => Pcm::Int(int.no_dither()),
        float => float,
    };
    FileTarget::new(
        ToFile::new(
            path,
            Encoding::new(Format::Wav(pcm))
                .channels(ChannelSet::default_for(ChannelLayout::STEREO)),
        )
        .replace(),
    )
}

fn flac(path: &std::path::Path, depth: IntDepth) -> FileTarget {
    FileTarget::new(
        ToFile::new(
            path,
            Encoding::new(Format::Flac {
                pcm: IntPcm::new(depth).no_dither(),
                level: FlacLevel::default(),
            })
            .channels(ChannelSet::default_for(ChannelLayout::STEREO)),
        )
        .replace(),
    )
}

/// `target`'s file from one render of the graph's main output.
fn render_file(graph: &mut RenderGraph, job: &Job, target: FileTarget) -> Written {
    let mut outputs = Outputs::new();
    let key = outputs.file(Output::main_output(target));
    let done = render(graph, job, &FrozenClock, outputs, &mut Control::none())
        .expect("renders")
        .into_finished()
        .expect("not cancelled");
    done.file(&key).expect("a key of these outputs").clone()
}

/// Decodes a file with `symphonia`, which shares no code with the encoders,
/// into one plane per channel.
fn decode(path: &std::path::Path) -> Vec<Vec<f32>> {
    let file = std::fs::File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(
        path.extension()
            .and_then(|e| e.to_str())
            .expect("an extension"),
    );
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .expect("probe format");
    let track = format
        .default_track(TrackType::Audio)
        .expect("default track")
        .clone();
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .expect("audio parameters")
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default().gapless(false))
        .expect("make decoder");
    let channels = params.channels.as_ref().expect("channels").count();
    let mut planes = vec![Vec::new(); channels];
    let mut interleaved: Vec<f32> = Vec::new();
    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != track.id {
            continue;
        }
        if let Ok(audio) = decoder.decode(&packet) {
            audio.copy_to_vec_interleaved::<f32>(&mut interleaved);
            for frame in interleaved.chunks(channels) {
                for (plane, s) in planes.iter_mut().zip(frame) {
                    plane.push(*s);
                }
            }
        }
    }
    planes
}

/// A decoded plane as integers at `depth`: the float symphonia gives back is
/// exactly `integer / 2^(bits - 1)` for a lossless 16 or 24 bit file.
fn as_ints(plane: &[f32], depth: IntDepth) -> Vec<i32> {
    let scale = f64::from(1u32 << (depth.bits() - 1));
    plane
        .iter()
        .map(|s| (f64::from(*s) * scale).round() as i32)
        .collect()
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// Two tones, one per channel, at different pitches and levels, so a swapped
/// channel or a misplaced frame reads as a different number.
fn two_tones() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let left = g.add(Osc::sine(Hz::new(440.0)).with_amplitude(Amplitude::new(0.5)));
    let right = g.add(Osc::sine(Hz::new(660.0)).with_amplitude(Amplitude::new(0.4)));
    g.connect_output(left, 0, 0).connect_output(right, 0, 1);
    finish(g)
}

/// One pass writes a 24-bit WAV and a 16-bit FLAC, and each file is its own
/// quantization of the audio the pass rendered: the planes a third output
/// holds in memory, put through each format's quantizer, are exactly what each
/// file decodes to. A different quantization per file is what a pass that
/// fed one encoder's output to the other would not give.
///
/// The graph is stepped once for the three outputs: a timeline clock, which
/// advances once per block rendered, stands exactly the render's frames on.
#[test]
fn two_outputs_of_different_formats_are_each_their_own_quantization_of_one_pass() {
    use std::sync::Arc;
    use tutti_tempo::TempoMap;
    use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig};
    use tutti_units::{Beat, Bpm};

    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.wav"), dir.path().join("b.flac"));
    let frames = 9_600;
    let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat::new(0.0),
        tempo_map: Arc::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo")),
        sample_rate: RATE,
        loop_range: None,
    });
    let mut outputs = Outputs::new();
    let (ka, kb) = (
        outputs.file(Output::main_output(wav(
            &a,
            Pcm::Int(IntPcm::new(IntDepth::Int24)),
        ))),
        outputs.file(Output::main_output(flac(&b, IntDepth::Int16))),
    );
    let kheld = outputs.buffers(Output::main_output(BufferTarget::new(
        ChannelLayout::STEREO,
    )));
    let done = render(
        &mut two_tones(),
        &job(frames),
        &timeline,
        outputs,
        &mut Control::none(),
    )
    .expect("renders")
    .into_finished()
    .expect("not cancelled");
    let (wa, wb) = (done.file(&ka).unwrap(), done.file(&kb).unwrap());
    assert_eq!((wa.path.as_path(), wb.path.as_path()), (&*a, &*b));
    let held = done.buffers(&kheld).unwrap().to_planes();
    assert_eq!((held.len(), held[0].len()), (2, frames));
    let peak = held[0].iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.45, "the tone is there: {peak}");

    for (path, depth) in [(&a, IntDepth::Int24), (&b, IntDepth::Int16)] {
        let decoded = decode(path);
        for (channel, plane) in decoded.iter().enumerate() {
            let want: Vec<i32> = held[channel].iter().map(|s| depth.quantize(*s)).collect();
            assert_eq!(
                as_ints(plane, depth),
                want,
                "{}: channel {channel} is the held plane through the {depth:?} quantizer",
                path.display()
            );
        }
    }

    let bps = tutti_music::beats_per_sample(Bpm::of(120.0), RATE);
    let stood = (timeline.beat().get() / bps.get()).round() as usize;
    assert_eq!(
        stood, frames,
        "the graph was stepped once, not once per output"
    );
    assert_eq!(names(dir.path()), ["a.wav", "b.flac"], "no temporary left");
}

/// Each output is dithered with the seed its encoding states: a bounce's stems and its master are
/// summed later, so the caller gives each its own, and the pass neither picks nor shares one.
/// Silence into a dithered 16-bit file is the dither alone, so equal files are equal noise.
#[test]
fn each_output_is_dithered_with_its_own_seed() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ["a.wav", "b.wav", "c.wav"].map(|name| dir.path().join(name));
    let dithered = |path: &std::path::Path, seed: u64| {
        FileTarget::new(ToFile::new(
            path,
            Encoding::new(Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16))))
                .channels(ChannelSet::default_for(ChannelLayout::STEREO))
                .dither_seed(seed),
        ))
    };
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let silence = g.add(Const::frame(&[0.0, 0.0]));
    g.pipe_output(silence);
    let mut outputs = Outputs::new();
    for (path, seed) in [(&paths[0], 1), (&paths[1], 2), (&paths[2], 1)] {
        outputs.file(Output::main_output(dithered(path, seed)));
    }
    render(
        &mut finish(g),
        &job(4_096),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect("renders")
    .into_finished()
    .expect("not cancelled");
    let [a, b, c] = paths.map(|path| decode(&path));
    assert!(a[0].iter().any(|&s| s != 0.0), "the file holds dither");
    assert_ne!(a, b, "two seeds shared one noise sequence");
    assert_eq!(a, c, "one seed gave two noise sequences");
}

/// Stereo `0.5` through a `0.5` gain, on the main output, with a tap on each of the
/// source (before the gain) and the gain (after it).
fn gain_graph() -> (Rig, NodeKey, NodeKey) {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let source = g.add(Const::frame(&[0.5, 0.5]));
    let gain = g.add(ForkByClone(Scale(0.5)));
    g.pipe(source, gain);
    stereo_out(&mut g, gain);
    (rig(g), source, gain)
}

/// A tap on a node's port renders that node's own signal, not the main output's:
/// a constant `0.5` read before a `0.5` gain is `0.5`, read after it `0.25`,
/// and the main output (after the gain) is `0.25`. The three outputs come from one
/// pass.
#[test]
fn a_tap_renders_the_signal_of_the_node_it_is_on() {
    let (mut rig, source, gain) = gain_graph();
    let before = rig.tap(1, source);
    let after = rig.tap(2, gain);
    let mut graph = rig.assemble();
    let mut outputs = Outputs::new();
    let keys = [
        outputs.buffers(Output::main_output(BufferTarget::new(
            ChannelLayout::STEREO,
        ))),
        outputs.buffers(Output::tap(
            before,
            stereo_placement(),
            BufferTarget::new(ChannelLayout::STEREO),
        )),
        outputs.buffers(Output::tap(
            after,
            stereo_placement(),
            BufferTarget::new(ChannelLayout::STEREO),
        )),
    ];
    let done = render(
        &mut graph,
        &job(3_000),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect("renders")
    .into_finished()
    .expect("not cancelled");
    for (name, key, want) in [
        ("main_output", &keys[0], 0.25),
        ("before the gain", &keys[1], 0.5),
        ("after the gain", &keys[2], 0.25),
    ] {
        let got = done.buffers(key).unwrap().to_planes();
        assert_eq!(got.len(), 2, "{name}");
        for (c, plane) in got.iter().enumerate() {
            assert_eq!(plane.len(), 3_000, "{name}");
            assert!(plane.iter().all(|s| *s == want), "{name} channel {c}");
        }
    }
}

/// A tap shares the main output's gate: with a latency trim of more than a block
/// the first block is dropped whole, its tap frames are still drained, and the
/// file starts where the main output would: at frame 1500 of a sine.
///
/// The sine is `0.4 sin(2 pi 440 n / 48000)` through a `0.5` gain, so the tap
/// on the gain reads `0.2 sin(...)`; its first kept frame is frame 1500 of it.
#[test]
fn a_tap_is_drained_and_gated_like_the_main_output() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let osc = g.add(
        Osc::sine(Hz::new(440.0))
            .with_amplitude(Amplitude::new(0.4))
            .with_layout(ChannelLayout::STEREO),
    );
    let gain = g.add(ForkByClone(Scale(0.5)));
    g.pipe(osc, gain);
    stereo_out(&mut g, gain);
    let mut rig = rig(g);
    let reader = rig.tap(1, gain);
    let mut graph = rig.assemble();
    let job = Job::new(
        RATE,
        Samples::new(2_000),
        Samples::new(1_500),
        TailPolicy::Frames(Samples::ZERO),
    )
    .expect("a render");
    let mut outputs = Outputs::new();
    let key = outputs.buffers(Output::tap(
        reader,
        stereo_placement(),
        BufferTarget::new(ChannelLayout::STEREO),
    ));
    let done = render(
        &mut graph,
        &job,
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect("renders")
    .into_finished()
    .expect("not cancelled");
    let got = done.buffers(&key).unwrap().to_planes();
    assert_eq!(got[0].len(), 2_000);
    for (i, s) in got[0].iter().enumerate() {
        let n = (1_500 + i) as f64;
        let want = 0.2 * (std::f64::consts::TAU * 440.0 * n / 48_000.0).sin();
        assert!(
            (f64::from(*s) - want).abs() < 2e-4,
            "frame {i}: {s} vs {want}"
        );
    }
}

/// A tap whose lanes cannot be placed on its target, and a tap that is not as
/// wide as its placement, are named errors, found before anything is rendered
/// and leaving no file.
#[test]
fn a_tap_that_cannot_be_placed_is_a_named_error() {
    let dir = tempfile::tempdir().unwrap();
    let (mut rig, source, gain) = gain_graph();
    let wide = rig.tap(1, source);
    let narrow = rig.tap(2, gain);
    let mut graph = rig.assemble();

    // Two lanes, a placement of one.
    let mut outputs = Outputs::new();
    outputs.file(Output::tap(
        wide,
        Placement::new(tutti_buffers::Bed::Mono.topology()),
        wav(
            &dir.path().join("a.wav"),
            Pcm::Int(IntPcm::new(IntDepth::Int16)),
        ),
    ));
    let err = render(
        &mut graph,
        &job(1_000),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect_err("two lanes are not one");
    assert!(
        matches!(
            err,
            RenderError::Target {
                output: 0,
                why: TargetError::Lanes { lanes, placement }
            } if lanes == ChannelLayout::STEREO && placement == ChannelLayout::MONO
        ),
        "{err:?}"
    );

    // The same check at the second output: it is made for every output before
    // the first file is begun, so the main output's is never created.
    let mut outputs = Outputs::new();
    outputs.file(Output::main_output(wav(
        &dir.path().join("m.wav"),
        Pcm::Int(IntPcm::new(IntDepth::Int16)),
    )));
    outputs.file(Output::tap(
        narrow,
        Placement::new(ChannelTopology::unassigned(ChannelLayout::from(3u16)).unwrap()),
        wav(
            &dir.path().join("b.wav"),
            Pcm::Int(IntPcm::new(IntDepth::Int16)),
        ),
    ));
    let err = render(
        &mut graph,
        &job(1_000),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect_err("two lanes are not three");
    assert!(matches!(
        err,
        RenderError::Target {
            output: 1,
            why: TargetError::Lanes { .. }
        }
    ));
    assert!(names(dir.path()).is_empty(), "nothing was left behind");
}

/// A placement that is as wide as its tap but cannot be folded onto the
/// target is `TargetError::Placement`, and the first output's file, already
/// begun by then, is removed.
#[test]
fn a_placement_the_target_cannot_take_is_a_target_error_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut rig, source, _) = gain_graph();
    let reader = rig.tap(1, source);
    let mut graph = rig.assemble();
    let mut outputs = Outputs::new();
    outputs.file(Output::main_output(wav(
        &dir.path().join("m.wav"),
        Pcm::Int(IntPcm::new(IntDepth::Int16)),
    )));
    outputs.file(Output::tap(
        reader,
        // Two unassigned feeds onto a mono file: unassigned channels are
        // not downmixed, so there is no place for them.
        Placement::new(ChannelTopology::unassigned(ChannelLayout::STEREO).unwrap()),
        FileTarget::new(ToFile::new(
            dir.path().join("t.wav"),
            Encoding::new(Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16))))
                .channels(ChannelSet::default_for(ChannelLayout::MONO)),
        )),
    ));
    let err = render(
        &mut graph,
        &job(1_000),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect_err("unassigned feeds are not L and R");
    assert!(
        matches!(
            err,
            RenderError::Target {
                output: 1,
                why: TargetError::Placement(_)
            }
        ),
        "{err:?}"
    );
    assert!(names(dir.path()).is_empty(), "the first file was removed");
}

/// Three files from one pass: a main output and two stems that sum to it, the
/// main output normalized and the stems taking its gain. All three report the same
/// gain, the stems' files are the stems scaled by it, and they still add up to
/// the main output's: the balance the shared gain exists to keep.
///
/// The gain is the main output's: `Written::gain` is the peak target less the
/// main output's measured true peak, and a stem has no reading of its own.
#[test]
fn stems_that_take_the_masters_gain_still_sum_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let a = g.add(Const::frame(&[0.2, 0.1]));
    let b = g.add(Const::frame(&[0.1, 0.25]));
    stereo_out(&mut g, a);
    stereo_out(&mut g, b);
    let mut rig = rig(g);
    let (ta, tb) = (rig.tap(1, a), rig.tap(2, b));
    let mut graph = rig.assemble();

    let mode = Normalize::peak(Db::new(-6.0));
    let files: Vec<std::path::PathBuf> = ["main_output.wav", "a.wav", "b.wav"]
        .iter()
        .map(|n| dir.path().join(n))
        .collect();
    let float = |path: &std::path::Path, gain| {
        FileTarget::new(ToFile::new(
            path,
            Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32)))
                .channels(ChannelSet::default_for(ChannelLayout::STEREO)),
        ))
        .normalize(gain)
    };
    let mut outputs = Outputs::new();
    let main_key = outputs.file(Output::main_output(float(&files[0], Gain::Own(mode))));
    let key_a = outputs.file(Output::tap(
        ta,
        stereo_placement(),
        float(&files[1], Gain::Of(main_key.clone())),
    ));
    let key_b = outputs.file(Output::tap(
        tb,
        stereo_placement(),
        float(&files[2], Gain::Of(main_key.clone())),
    ));
    let done = render(
        &mut graph,
        &job(24_000),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect("renders")
    .into_finished()
    .expect("not cancelled");
    let (main_output, sa, sb) = (
        done.file(&main_key).unwrap(),
        done.file(&key_a).unwrap(),
        done.file(&key_b).unwrap(),
    );

    let gain = main_output.gain.expect("the main output normalized");
    let reading = main_output.measured.expect("and measured itself");
    assert!(
        (gain.get() - (-6.0 - reading.true_peak.get())).abs() < 1e-5,
        "the gain is the target less the true peak: {gain:?} for {reading:?}"
    );
    assert_eq!((sa.gain, sb.gain), (Some(gain), Some(gain)));
    assert_eq!(
        (sa.measured, sb.measured),
        (None, None),
        "they measured nothing"
    );

    // The decoded files are `f32`: the scale narrows once.
    let scale = gain.to_amplitude().get() as f32;
    let (m, da, db) = (decode(&files[0]), decode(&files[1]), decode(&files[2]));
    for c in 0..2 {
        let want_a = [0.2, 0.1][c] * scale;
        let want_b = [0.1, 0.25][c] * scale;
        for i in 0..24_000 {
            assert!(
                (da[c][i] - want_a).abs() < 1e-6,
                "a, channel {c}, frame {i}"
            );
            assert!(
                (db[c][i] - want_b).abs() < 1e-6,
                "b, channel {c}, frame {i}"
            );
            assert!(
                (da[c][i] + db[c][i] - m[c][i]).abs() < 1e-6,
                "the stems sum to the main output: channel {c}, frame {i}"
            );
        }
    }
}

/// A rename that fails after another landed is `NotCommitted`, naming the file
/// that landed, which is kept; the file that did not land is kept where the
/// commit error names it, and what was in its way is as it was. A first rename
/// that fails is the plain commit error, with nothing landed and the outputs
/// after it removed.
///
/// The second target is a directory, which a file cannot replace.
#[test]
fn a_rename_that_fails_after_another_landed_is_not_committed() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second) = (dir.path().join("first.wav"), dir.path().join("second.wav"));
    std::fs::write(&first, b"old").unwrap();
    std::fs::create_dir(&second).unwrap();
    std::fs::write(second.join("keep"), b"x").unwrap();

    let mut outputs = Outputs::new();
    outputs.file(Output::main_output(wav(
        &first,
        Pcm::Int(IntPcm::new(IntDepth::Int16)),
    )));
    outputs.file(Output::main_output(wav(
        &second,
        Pcm::Int(IntPcm::new(IntDepth::Int16)),
    )));
    let err = render(
        &mut two_tones(),
        &job(2_000),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect_err("a file cannot replace a directory");
    match err {
        RenderError::NotCommitted { landed, cause } => {
            assert_eq!(landed, std::slice::from_ref(&first));
            let tutti_codec::EncodeError::Commit(commit) = &cause else {
                panic!("a failed rename is a commit error, got {cause:?}");
            };
            let kept = commit.kept().to_path_buf();
            assert!(
                std::fs::read(&kept).unwrap().starts_with(b"RIFF"),
                "the unlanded file is kept whole"
            );
            std::fs::remove_file(&kept).unwrap();
        }
        other => panic!("{other:?}"),
    }
    let new = std::fs::read(&first).unwrap();
    assert!(
        new.starts_with(b"RIFF"),
        "the first file landed and is kept"
    );
    assert!(
        second.join("keep").exists(),
        "what was in the way is as it was"
    );
    assert_eq!(
        names(dir.path()),
        ["first.wav", "second.wav"],
        "no temporary"
    );

    // The first rename failing lands nothing: the plain error.
    std::fs::remove_file(&first).unwrap();
    std::fs::create_dir(&first).unwrap();
    let mut outputs = Outputs::new();
    outputs.file(Output::main_output(wav(
        &first,
        Pcm::Int(IntPcm::new(IntDepth::Int16)),
    )));
    outputs.file(Output::main_output(wav(
        &dir.path().join("third.wav"),
        Pcm::Int(IntPcm::new(IntDepth::Int16)),
    )));
    let err = render(
        &mut two_tones(),
        &job(2_000),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect_err("a file cannot replace a directory");
    let RenderError::Encode(tutti_codec::EncodeError::Commit(commit)) = &err else {
        panic!("a failed first rename is a commit error, got {err:?}");
    };
    std::fs::remove_file(commit.kept()).unwrap();
    assert!(!dir.path().join("third.wav").exists());
    assert_eq!(
        names(dir.path()),
        ["first.wav", "second.wav"],
        "only the targets remain"
    );
}

/// Outputs that contradict each other are named errors before anything is
/// rendered: none at all, a gain taken from a file that does not measure itself
/// (one that takes none, or another's), a gain taken from another `Outputs`'
/// key, and two files at one path.
#[test]
fn outputs_that_contradict_each_other_are_named_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mode = Normalize::peak(Db::new(-6.0));
    let file = |n: &str, gain: Option<Gain>| {
        let target = wav(&dir.path().join(n), Pcm::Int(IntPcm::new(IntDepth::Int16)));
        Output::main_output(match gain {
            Some(gain) => target.normalize(gain),
            None => target,
        })
    };
    let run = |outputs: Outputs| {
        render(
            &mut two_tones(),
            &job(2_000),
            &FrozenClock,
            outputs,
            &mut Control::none(),
        )
    };

    assert!(matches!(
        run(Outputs::new()).expect_err("nothing to render"),
        RenderError::NoOutputs
    ));

    // A gain from a file that takes none.
    let mut outputs = Outputs::new();
    let plain = outputs.file(file("a.wav", None));
    outputs.file(file("b.wav", Some(Gain::Of(plain))));
    let err = run(outputs).expect_err("a gain from nowhere");
    assert!(
        matches!(
            err,
            RenderError::Target {
                output: 1,
                why: TargetError::GainSource { index: 0 }
            }
        ),
        "{err:?}"
    );

    // A gain from a file that itself takes another's.
    let mut outputs = Outputs::new();
    let own = outputs.file(file("a.wav", Some(Gain::Own(mode))));
    let follower = outputs.file(file("b.wav", Some(Gain::Of(own))));
    outputs.file(file("c.wav", Some(Gain::Of(follower))));
    let err = run(outputs).expect_err("a gain from a follower");
    assert!(
        matches!(
            err,
            RenderError::Target {
                output: 2,
                why: TargetError::GainSource { index: 1 }
            }
        ),
        "{err:?}"
    );

    // A key of another `Outputs` is refused by name, though its index is in range.
    let mut elsewhere = Outputs::new();
    let foreign = elsewhere.file(file("z.wav", Some(Gain::Own(mode))));
    let mut outputs = Outputs::new();
    outputs.file(file("a.wav", Some(Gain::Own(mode))));
    outputs.file(file("b.wav", Some(Gain::Of(foreign))));
    let err = run(outputs).expect_err("a key of another Outputs");
    assert!(
        matches!(
            err,
            RenderError::Target {
                output: 1,
                why: TargetError::ForeignGain
            }
        ),
        "{err:?}"
    );

    let mut outputs = Outputs::new();
    outputs.file(file("a.wav", None));
    outputs.file(file("a.wav", None));
    let err = run(outputs).expect_err("one path twice");
    assert!(
        matches!(
            err,
            RenderError::Target {
                output: 1,
                why: TargetError::SamePath { other: 0 }
            }
        ),
        "{err:?}"
    );
    assert!(names(dir.path()).is_empty());
}

/// A key is read from the `Done` of the `Outputs` that made it: another render's
/// key is a named error for a file and for planes alike, though its index is in
/// range, and a second read of one key is the same value.
#[test]
fn a_done_refuses_the_key_of_another_outputs_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let render_two = |name: &str| {
        let mut outputs = Outputs::new();
        let file = outputs.file(Output::main_output(wav(
            &dir.path().join(name),
            Pcm::Int(IntPcm::new(IntDepth::Int16)),
        )));
        let planes = outputs.buffers(Output::main_output(BufferTarget::new(
            ChannelLayout::STEREO,
        )));
        let done = render(
            &mut two_tones(),
            &job(500),
            &FrozenClock,
            outputs,
            &mut Control::none(),
        )
        .expect("renders")
        .into_finished()
        .expect("not cancelled");
        (done, file, planes)
    };
    let (done, file, planes) = render_two("a.wav");
    let (other, other_file, other_planes) = render_two("b.wav");
    assert_eq!(done.file(&other_file), Err(DoneError::Foreign));
    assert_eq!(
        done.buffers(&other_planes).map(|_| ()),
        Err(DoneError::Foreign)
    );
    assert_eq!(other.file(&file), Err(DoneError::Foreign));
    assert_eq!(done.file(&file), done.file(&file));
    assert_eq!(done.buffers(&planes).unwrap().channels(), 2);
    assert_eq!(done.buffers(&planes).unwrap().len().get(), 500);

    // A taken result is a named error afterwards, not a default.
    let (mut done, file, planes) = (done, file, planes);
    assert_eq!(done.take_file(&file).unwrap().frames.get(), 500);
    assert_eq!(done.file(&file), Err(DoneError::Taken));
    assert_eq!(done.take_buffers(&planes).unwrap().channels(), 2);
    assert_eq!(
        done.take_buffers(&planes).map(|_| ()),
        Err(DoneError::Taken)
    );
}

/// An output whose audio cannot be normalized names itself: with an audible
/// main output first and a silent stem second, `Unmeasurable` is the second's.
#[test]
fn an_unmeasurable_output_names_its_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let loud = g.add(Const::frame(&[0.5, 0.5]));
    let silent = g.add(Const::frame(&[0.0, 0.0]));
    stereo_out(&mut g, loud);
    stereo_out(&mut g, silent);
    let mut rig = rig(g);
    let reader = rig.tap(1, silent);
    let mut graph = rig.assemble();

    let mut outputs = Outputs::new();
    outputs.file(Output::main_output(
        FileTarget::new(ToFile::new(
            dir.path().join("m.wav"),
            Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32)))
                .channels(ChannelSet::default_for(ChannelLayout::STEREO)),
        ))
        .normalize(Gain::Own(Normalize::peak(Db::new(-6.0)))),
    ));
    outputs.file(Output::tap(
        reader,
        stereo_placement(),
        FileTarget::new(ToFile::new(
            dir.path().join("s.wav"),
            Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32)))
                .channels(ChannelSet::default_for(ChannelLayout::STEREO)),
        ))
        .normalize(Gain::Own(Normalize::peak(Db::new(-6.0)))),
    ));
    let err = render(
        &mut graph,
        &job(2_000),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect_err("silence has no peak to normalize");
    assert!(
        matches!(
            err,
            RenderError::Unmeasurable {
                output: Some(1),
                ..
            }
        ),
        "{err:?}"
    );
    assert!(names(dir.path()).is_empty(), "nothing was put in place");
}

/// A graph of `width` outputs, each a constant `0.5`.
fn wide(width: u16) -> RenderGraph {
    let layout = ChannelLayout::from(width);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
    let node = g.add(Const::new(0.5, layout));
    for c in 0..usize::from(width) {
        g.connect_output(node, c, c);
    }
    finish(g)
}

fn file_of(path: &std::path::Path, width: u16) -> FileTarget {
    FileTarget::new(
        ToFile::new(
            path,
            Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32)))
                .channels(ChannelSet::default_for(ChannelLayout::from(width))),
        )
        .replace(),
    )
}

/// A file whose encoding names no channels takes the feed's and its `Written` says the
/// placement is the default; one whose encoding names its channels, the default 5.1 too, says it
/// was stated.
#[test]
fn a_file_reports_whether_its_placement_is_the_default_or_stated() {
    let dir = tempfile::tempdir().unwrap();
    let placement = |target: FileTarget| render_file(&mut wide(6), &job(2_000), target).placement;
    let unstated = FileTarget::new(ToFile::new(
        dir.path().join("a.wav"),
        Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32))),
    ));
    assert_eq!(placement(unstated), tutti_buffers::Placement::Default);
    let counted = FileTarget::new(ToFile::new(
        dir.path().join("c.wav"),
        Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32))).width(ChannelLayout::STEREO),
    ));
    assert_eq!(placement(counted), tutti_buffers::Placement::Default);
    let stated = FileTarget::new(ToFile::new(
        dir.path().join("b.wav"),
        Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32)))
            .channels(ChannelSet::from(Bed::Surround51.topology())),
    ));
    assert_eq!(placement(stated), tutti_buffers::Placement::Stated);
}

/// A Lufs output of a width with no default layout is refused up front with
/// a name that says to state a topology, and nothing is rendered or written; a
/// peak output of the same width needs no placement and renders.
#[test]
fn a_lufs_output_with_no_placement_is_refused_before_the_render() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = wide(3);
    let lufs =
        file_of(&dir.path().join("l.wav"), 3).normalize(Gain::Own(Normalize::lufs(Db::new(-23.0))));
    let mut outputs = Outputs::new();
    outputs.file(Output::main_output(lufs));
    let err = render(
        &mut graph,
        &job(2_000),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect_err("three channels name no speakers");
    assert!(
        matches!(
            err,
            RenderError::Target {
                output: 0,
                why: TargetError::UnplacedLoudness { channels }
            } if channels == ChannelLayout::from(3u16)
        ),
        "{err:?}"
    );
    assert!(names(dir.path()).is_empty());
}

/// A graph of `width` outputs, each a 1 kHz sine of amplitude 0.5.
fn tone(width: u16) -> RenderGraph {
    let layout = ChannelLayout::from(width);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
    let node = g.add(
        Osc::sine(Hz::new(1_000.0))
            .with_amplitude(Amplitude::new(0.5))
            .with_layout(layout),
    );
    for c in 0..usize::from(width) {
        g.connect_output(node, c, c);
    }
    finish(g)
}

/// A Peak output at a width with no placement reaches its target by the true
/// peak alone: a sine of amplitude 0.5 (-6.02 dBTP) normalized to -12 dBTP
/// takes `-12 + 6.02` dB, and the file's peak is `10^(-12/20)`. It has no
/// placement to weigh a loudness by, so it reports none.
#[test]
fn a_peak_output_with_no_placement_reaches_its_target_and_reports_no_loudness() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.wav");
    let target = file_of(&path, 3).normalize(Gain::Own(Normalize::peak(Db::new(-12.0))));
    let written = render_file(&mut tone(3), &job(48_000), target);
    assert_eq!(written.measured, None);
    let gain = written.gain.expect("normalized").get();
    assert!(
        (gain - (-12.0 - Db::from_amplitude(Amplitude::new(0.5)).get())).abs() < 0.1,
        "{gain}"
    );
    let peak = decode(&path)
        .iter()
        .flatten()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    assert!((peak - 10f32.powf(-12.0 / 20.0)).abs() < 0.005, "{peak}");
}

/// A Peak output at a width with a default layout still reads a real
/// loudness, weighted by it: six channels of the same tone read the five
/// non-LFE channels at weights 1, 1, 1, 1.41, 1.41 (sum 5.82) (BS.1770-5 Table 5, system
/// B): `10 log10(0.5^2 / 2 * 5.82)` = -1.38 LUFS, where the LFE counted too
/// would read -0.64.
#[test]
fn a_peak_output_with_a_default_layout_reports_its_weighted_loudness() {
    let dir = tempfile::tempdir().unwrap();
    // The encoding names no channels: the file has the graph's, the default 5.1.
    let target = FileTarget::new(ToFile::new(
        dir.path().join("p.wav"),
        Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32))),
    ))
    .normalize(Gain::Own(Normalize::peak(Db::new(-12.0))));
    let written = render_file(&mut tone(6), &job(48_000), target);
    assert_eq!(written.placement, tutti_buffers::Placement::Default);
    let lufs = written.measured.expect("a placed loudness").lufs.get();
    let analytic = Db::from_power(0.5f64.powi(2) / 2.0 * 5.82).get();
    assert!((lufs - analytic).abs() < 0.1, "{lufs} vs {analytic}");
}
