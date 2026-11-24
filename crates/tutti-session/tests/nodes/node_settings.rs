//! A node's discrete settings through its key: `key.sync()` and `key.key_filter()` set and
//! read the word the node reads once per block, as `session.click().mode()` does for the
//! click. Every figure is analytic: the echo of a constant through a delay is its time in
//! frames, a high-passed key's reduction is the static curve at the filter's closed-form
//! magnitude, and a stopped transport locks a beat-synced sawtooth to its first sample.

use core::f64::consts::PI;
use tutti_buffers::{ChannelLayout, ChannelTopology, InterleavedMut};
use tutti_graph::ForkByClone;
use tutti_music::NoteDuration;
use tutti_nodes::testing::{Const, Osc};
use tutti_nodes::{
    CompressorConfig, CompressorControls, CompressorNode, DelayConfig, DelayControls, DelayLayout,
    DelayNode, DelaySync, GateConfig, GateControls, GateNode, KeyFilter, KeySource, LfoConfig,
    LfoNode, LfoShape, LfoSync, MaxDelay, NoteTimes,
};
use tutti_session::{InputPort, Key, MainOutput, OutputPort, Session, ValueErrorKind};
use tutti_units::{
    Amplitude, BeatsPerCycle, CompressionRatio, Db, Feedback, Hz, Q, SampleRate, Seconds,
};

const RATE: usize = 48_000;
const BLOCK: usize = 480;
/// The Q of a Butterworth section.
const BUTTERWORTH: f64 = core::f64::consts::FRAC_1_SQRT_2;
/// The constant the compressor and the gate gain.
const AUDIO: f32 = 0.5;
/// The frames at the end of a settled run the dynamics' figures are read over: a whole number
/// of cycles of the 250 Hz key.
const WINDOW: usize = 4_800;
const THRESHOLD: f64 = -40.0;
const RATIO: f64 = 4.0;

fn session() -> Session {
    Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .build()
        .expect("builds")
}

/// `frames` frames of the main output's channel 0.
fn render(s: &mut Session, frames: usize) -> Vec<f32> {
    let _ = s.update();
    let mut block = vec![0.0; BLOCK * 2];
    let mut out = Vec::new();
    while out.len() < frames {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        out.extend(block.iter().step_by(2));
    }
    out
}

/// A constant through a delay of a quarter note (at 120 bpm: half a second) or 0.25 s, to
/// channel 0, committed once. A session's default transport is stopped at 120 bpm.
fn delay_rig() -> (Session, Key<DelayNode<f32>>) {
    let mut s = session();
    let g = s.graph();
    let source = g.add(ForkByClone(Const::mono(AUDIO)));
    let layout = DelayLayout::builder(ChannelTopology::MONO, MaxDelay::of(Seconds::new(2.0)))
        .notes(NoteTimes::all(NoteDuration::QUARTER))
        .build()
        .expect("a layout");
    let delay = g.add(DelayNode::<f32>::from(DelayConfig::new(layout).controls(
        DelayControls {
            time: Seconds::new(0.25),
            feedback: Feedback::NONE,
            ..DelayControls::DEFAULT
        },
    )));
    g.connect(OutputPort::new(source, 0), InputPort::new(delay, 0))
        .unwrap()
        .unfaded();
    g.connect(OutputPort::new(delay, 0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    (s, delay)
}

/// The first frame a delay's output is not silent on.
fn echo_frame(out: &[f32]) -> usize {
    out.iter().position(|&v| v != 0.0).expect("an echo")
}

/// **A delay's sync set before its first block is the word its first block reads**: free it
/// echoes a constant at its 0.25 s (12 000 frames), synced it echoes at the quarter note of the
/// session's 120 bpm (24 000 frames).
#[test]
fn a_delay_synced_through_its_key_echoes_at_its_note() {
    let (mut free, _) = delay_rig();
    assert_eq!(echo_frame(&render(&mut free, 30_000)), 12_000);

    let (mut s, delay) = delay_rig();
    s.set(delay.sync().to(DelaySync::Synced)).unwrap();
    assert_eq!(echo_frame(&render(&mut s, 30_000)), 24_000);
}

/// `AUDIO` through `node` as the audio, with a 250 Hz tone of amplitude 0.5 as its key.
fn keyed<N>(node: N) -> (Session, Key<N>)
where
    N: tutti_graph::IntoNode<f32, Controls = tutti_graph::ParamSet> + Send + 'static,
{
    let mut s = session();
    let g = s.graph();
    let audio = g.add(ForkByClone(Const::mono(AUDIO)));
    let key = g.add(ForkByClone(
        Osc::sine(Hz::new(250.0)).with_amplitude(Amplitude::new(0.5)),
    ));
    let node = g.add(node);
    g.connect(OutputPort::new(audio, 0), InputPort::new(node, 0))
        .unwrap()
        .unfaded();
    g.connect(OutputPort::new(key, 0), InputPort::new(node, 1))
        .unwrap()
        .unfaded();
    g.connect(OutputPort::new(node, 0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    (s, node)
}

/// The closed-form magnitude of the 2nd-order high-pass at `f` for a cutoff `fc` and `q`, in dB.
fn high_pass_db(f: f64, fc: f64, q: f64) -> f64 {
    let rate = RATE as f64;
    let w = (PI * f / rate).tan() / (PI * fc / rate).tan();
    20.0 * (w * w / (1.0 - w * w).hypot(w / q)).log10()
}

fn amplitude_db(a: f64) -> f64 {
    Amplitude::new(a).to_db().get()
}

/// The deepest reduction over the last [`WINDOW`] frames of `out`, in dB.
fn deepest_reduction_db(out: &[f32]) -> f64 {
    let lowest = out[out.len() - WINDOW..]
        .iter()
        .fold(f32::MAX, |m, &x| m.min(x));
    -amplitude_db(f64::from(lowest) / f64::from(AUDIO))
}

/// The highest gain over the last [`WINDOW`] frames of `out`.
fn highest_gain(out: &[f32]) -> f64 {
    let highest = out[out.len() - WINDOW..]
        .iter()
        .fold(f32::MIN, |m, &x| m.max(x));
    f64::from(highest) / f64::from(AUDIO)
}

/// **A compressor's key filter set through its key moves the reduction onto the curve at the
/// filtered level**: a 250 Hz key at 0.5 is reduced by `(level - threshold)(1 - 1/ratio)` while
/// off, and by the same curve at the level the 1 kHz Butterworth high-pass leaves it once
/// set, and back when set off again.
#[test]
fn a_compressors_key_filter_set_through_its_key_moves_the_reduction() {
    let (f, fc, a) = (250.0, 1_000.0, 0.5);
    let curve = |level: f64| (level - THRESHOLD).max(0.0) * (1.0 - 1.0 / RATIO);
    let off = curve(amplitude_db(a));
    let on = curve(amplitude_db(a) + high_pass_db(f, fc, BUTTERWORTH));
    assert!(on < off - 5.0, "the filter is heard");
    let (mut s, comp) = keyed(CompressorNode::<f32>::from(
        CompressorConfig::new(ChannelTopology::MONO)
            .key(KeySource::Sidechain)
            .controls(CompressorControls {
                threshold: Db::new(THRESHOLD),
                ratio: CompressionRatio::new(RATIO),
                key_cutoff: Hz::new(fc),
                key_q: Q::new(BUTTERWORTH),
                ..CompressorControls::DEFAULT
            }),
    ));
    let tolerance = 0.02 - 20.0 * (PI * f / RATE as f64).cos().log10();
    for (filter, want) in [
        (KeyFilter::Off, off),
        (KeyFilter::HighPass, on),
        (KeyFilter::Off, off),
    ] {
        s.set(comp.key_filter().to(filter)).unwrap();
        let got = deepest_reduction_db(&render(&mut s, 24_000));
        assert!(
            (got - want).abs() < tolerance,
            "{filter:?}: {got} dB, curve says {want} dB"
        );
    }
}

/// **A gate's key filter set through its key closes it on a key the filter takes under the
/// threshold**: a 250 Hz key opens the gate while off, and under the 1 kHz high-pass its peak
/// is 0.3 dB under the threshold, so the gate closes; set off again, it opens.
#[test]
fn a_gates_key_filter_set_through_its_key_closes_it() {
    let (f, fc, a) = (250.0, 1_000.0, 0.5);
    let threshold = amplitude_db(a) + high_pass_db(f, fc, BUTTERWORTH) + 0.3;
    let (mut s, gate) = keyed(GateNode::<f32>::from(
        GateConfig::new(ChannelTopology::MONO)
            .key(KeySource::Sidechain)
            .controls(GateControls {
                threshold: Db::new(threshold),
                range: Db::new(-80.0),
                key_cutoff: Hz::new(fc),
                key_q: Q::new(BUTTERWORTH),
                ..GateControls::DEFAULT
            }),
    ));
    for (filter, opens) in [
        (KeyFilter::Off, true),
        (KeyFilter::HighPass, false),
        (KeyFilter::Off, true),
    ] {
        s.set(gate.key_filter().to(filter)).unwrap();
        let gain = highest_gain(&render(&mut s, 24_000));
        if opens {
            assert!(gain > 0.999, "{filter:?}: gain {gain}");
        } else {
            assert!(gain < 1e-3, "{filter:?}: gain {gain}");
        }
    }
}

/// **An LFO's sync set through its key locks it to the beat**: free, a sawtooth at 1 Hz is
/// `2 frac(k / 48 000) - 1` from its first frame; set synced under the session's stopped
/// transport (beat 0) it settles on the locked phase 0, a sawtooth's `-1`, once the ramp
/// (0.05 s) is over.
#[test]
fn an_lfos_sync_set_through_its_key_locks_it_to_the_beat() {
    let mut s = session();
    let g = s.graph();
    let lfo = g.add(LfoNode::from(
        LfoConfig::new(LfoShape::Sawtooth)
            .rate(Hz::new(1.0))
            .beats_per_cycle(BeatsPerCycle::of(1.0)),
    ));
    g.connect(OutputPort::new(lfo, 0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    let free = render(&mut s, 960);
    for (k, &v) in free.iter().enumerate() {
        let want = 2.0 * (k as f64 / RATE as f64) - 1.0;
        assert!((f64::from(v) - want).abs() < 1e-5, "free, frame {k}: {v}");
    }
    s.set(lfo.sync().to(LfoSync::Synced)).unwrap();
    let synced = render(&mut s, 4_800);
    for &v in &synced[synced.len() - 480..] {
        assert!((f64::from(v) + 1.0).abs() < 1e-5, "synced: {v}");
    }
}

/// **A setting reads back every word it was set to**, for each setting a node has.
#[test]
fn a_setting_reads_back_every_word_it_was_set_to() {
    let (mut s, delay) = delay_rig();
    for word in [DelaySync::Synced, DelaySync::Free] {
        s.set(delay.sync().to(word)).unwrap();
        assert_eq!(s.get(delay.sync()).unwrap(), word);
    }
    let gate = s.graph().add(GateNode::<f32>::from(GateConfig::new(
        ChannelTopology::MONO,
    )));
    for word in [KeyFilter::HighPass, KeyFilter::BandPass, KeyFilter::Off] {
        s.set(gate.key_filter().to(word)).unwrap();
        assert_eq!(s.get(gate.key_filter()).unwrap(), word);
    }
    let lfo = s.graph().add(LfoNode::from(LfoConfig::new(LfoShape::Sine)));
    for word in [LfoSync::Synced, LfoSync::Free] {
        s.set(lfo.sync().to(word)).unwrap();
        assert_eq!(s.get(lfo.sync()).unwrap(), word);
    }
}

/// **A removed node's setting is refused at the call** with the named error, whatever the
/// word, and is no longer readable.
#[test]
fn a_removed_nodes_setting_is_refused() {
    let (mut s, delay) = delay_rig();
    s.set(delay.sync().to(DelaySync::Synced)).unwrap();
    s.remove(delay).unwrap();
    let refused = s.set(delay.sync().to(DelaySync::Free)).unwrap_err();
    assert_eq!(refused.kind, ValueErrorKind::NoSuchTarget);
    assert!(refused.target.is_some_and(|t| t.is(delay.sync())));
    assert_eq!(
        s.get(delay.sync()).unwrap_err().kind,
        ValueErrorKind::NoSuchTarget
    );
}

/// **A saved session keeps a node's setting**: restored over a later write, the word is the
/// one saved.
#[test]
fn a_restored_session_keeps_the_setting_it_saved() {
    let (s, delay) = delay_rig();
    s.set(delay.sync().to(DelaySync::Synced)).unwrap();
    let saved = s.saved_values().unwrap();
    s.set(delay.sync().to(DelaySync::Free)).unwrap();
    s.set(&saved).unwrap();
    assert_eq!(s.get(delay.sync()).unwrap(), DelaySync::Synced);
}

/// A setting enum of the test's own, which the session does not know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Pace {
    Slow,
    Fast,
    Frantic,
}
tutti_rt::atomic_word!(Pace {
    Slow,
    Fast,
    Frantic
});

/// A node with a `Sync` setting in the test's own enum.
struct Metronome {
    pace: std::sync::Arc<tutti_rt::AtomicCell<Pace>>,
}

const SYNC: u16 = tutti_units::SettingKind::Sync as u16;

impl tutti_graph::HasSetting<SYNC> for Metronome {
    type Word = Pace;
}

impl tutti_graph::Node for Metronome {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(&mut self, _: &tutti_graph::Cx<'_>, _: tutti_graph::Io<'_>) -> tutti_graph::Status {
        tutti_graph::Status::Silent
    }
    fn reset(&mut self) {}
}

impl tutti_graph::IntoNode for Metronome {
    type Controls = ();

    fn into_parts(self) -> tutti_graph::NodeParts<()> {
        let setting =
            tutti_graph::ErasedSetting::new::<Self, SYNC>(std::sync::Arc::clone(&self.pace));
        tutti_graph::NodeParts::new(Box::new(self), (), None, None).settings(vec![setting])
    }
}

/// **A node's setting in an enum the session has never heard of is set and read through
/// `key.sync()`**, and the node's own cell holds what was set: the session addresses the word,
/// never the node's enum.
#[test]
fn a_setting_in_the_nodes_own_enum_is_set_and_read_through_its_key() {
    let pace = std::sync::Arc::new(tutti_rt::AtomicCell::new(Pace::Slow));
    let mut s = session();
    let node = s.graph().add(Metronome {
        pace: std::sync::Arc::clone(&pace),
    });
    for word in [Pace::Frantic, Pace::Fast, Pace::Slow] {
        s.set(node.sync().to(word)).unwrap();
        assert_eq!(s.get(node.sync()).unwrap(), word);
        assert_eq!(pace.load(), word);
    }
}
