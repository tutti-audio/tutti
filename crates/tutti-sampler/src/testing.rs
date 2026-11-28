//! The one transport the crate's tests share, and the block driver they
//! drive a node under it with.
//!
//! A node reads the transport from its block's `Env`, so a test
//! holds a [`MockTransport`] — a playhead it moves between blocks, as a host
//! moves one — and hands each block the `Env` it describes
//! ([`MockTransport::env`], or [`block`] / [`play`], which drive a node
//! through `tutti_graph::contract::drive_input`).
//!
//! Constructed from [`Beat`] and [`Bpm`] rather than two bare `f64`s. Position
//! and tempo have the same primitive representation, so an argument-order slip
//! compiles clean and yields a transport at the wrong tempo *and* the wrong
//! position — a test that then fails for a reason nowhere near the code under
//! test. Unit types make the swap a compile error.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tutti_buffers::{ChannelLayout, Float, Planes, Wave};

use tutti_graph::{Env, Node, Prepare, Shared, TransportChanges, TransportPosition, contract};
use tutti_music::LoopRange;
use tutti_units::{Beat, Bpm, Frame, SampleRate, Samples};

use crate::{Loop, MemorySource, MemorySourceConfig, VoiceWindow};

/// The loop over file frames `start..end` with `crossfade` frames of fade
/// into it, for a test that has literal frames.
///
/// # Panics
///
/// On an empty span or a crossfade longer than the loop: a test's loop is a
/// literal, and a typo in one should stop it, not play something else.
pub fn loop_frames(start: usize, end: usize, crossfade: usize) -> Loop {
    Loop::new(Samples::new(start)..Samples::new(end))
        .and_then(|l| l.with_crossfade(Samples::new(crossfade)))
        .expect("a test's loop is a literal that `Loop` accepts")
}

/// A stereo source over `wave`, free-running from its first frame: what a test that
/// plays one wave on its own cursor builds.
///
/// # Panics
///
/// If the wave has no place in a stereo frame: a test's wave is one the sampler plays.
pub fn free_source<F: Float>(wave: impl Into<Shared<Wave<F>>>) -> MemorySource<F> {
    MemorySource::from(
        MemorySourceConfig::new(wave, ChannelLayout::STEREO)
            .expect("a test's wave has a place in a stereo frame"),
    )
}

/// A stereo source over `wave`, placed on the transport at `window`.
///
/// # Panics
///
/// As [`free_source`].
pub fn placed_source<F: Float>(
    wave: impl Into<Shared<Wave<F>>>,
    window: VoiceWindow,
) -> MemorySource<F> {
    MemorySource::from(
        MemorySourceConfig::new(wave, ChannelLayout::STEREO)
            .expect("a test's wave has a place in a stereo frame")
            .window(window),
    )
}

/// A source config over `wave`, `channels` wide: free-running, or placed at `window`.
///
/// # Panics
///
/// If the wave has no place in `channels`: a test's wave is one the sampler plays.
pub fn source_config<F: Float>(
    wave: impl Into<Shared<Wave<F>>>,
    channels: impl Into<ChannelLayout>,
    window: Option<VoiceWindow>,
) -> MemorySourceConfig<F> {
    let config = MemorySourceConfig::new(wave, channels)
        .expect("a test's wave has a place in the width it asks for");
    match window {
        Some(window) => config.window(window),
        None => config,
    }
}

/// The node over `source`, played as `play` says: placed when the source has a window,
/// free-running when it has none. `play`'s gain, speed and loop replace the source's.
///
/// # Panics
///
/// On a non-finite gain in `play`: a test's gain is a literal.
pub fn voice_node<F: crate::SamplerFloat>(
    source: MemorySourceConfig<F>,
    play: crate::Playback,
) -> crate::VoiceNode<F> {
    use crate::VoiceNodeConfig;
    let source = source.gain(play.gain).speed(play.speed);
    let source = match play.loop_ {
        Some(looped) => source.looped(looped),
        None => source,
    };
    let expected = "a test's gain is finite";
    if source.window_value().is_some() {
        let config = VoiceNodeConfig::placed(source).expect(expected);
        crate::VoiceNode::from(
            config
                .direction(play.direction)
                .stretch(play.stretch)
                .pitch(play.pitch),
        )
    } else {
        let config = VoiceNodeConfig::free(source).expect(expected);
        crate::VoiceNode::from(
            config
                .direction(play.direction)
                .stretch(play.stretch)
                .pitch(play.pitch),
        )
    }
}

/// A stereo source config over `wave`, free-running.
///
/// # Panics
///
/// As [`free_source`].
pub fn free_config<F: Float>(wave: impl Into<Shared<Wave<F>>>) -> MemorySourceConfig<F> {
    source_config(wave, ChannelLayout::STEREO, None)
}

/// A stereo source config over `wave`, placed on the transport at `window`.
///
/// # Panics
///
/// As [`free_source`].
pub fn placed_config<F: Float>(
    wave: impl Into<Shared<Wave<F>>>,
    window: VoiceWindow,
) -> MemorySourceConfig<F> {
    source_config(wave, ChannelLayout::STEREO, Some(window))
}

/// `node` with fresh cells and the controls that write them, without a graph:
/// what a test that drives a node by hand steers it through. A node inserted
/// into a graph takes its controls from `into_parts`.
pub fn controlled<F: crate::SamplerFloat>(
    node: crate::VoiceNode<F>,
) -> (crate::VoiceNode<F>, crate::VoiceNodeControls<F>) {
    node.controlled()
}

/// The spare vocoders `pool`'s voices carry: what a loop under the voices asks of a pool's
/// memory, for a test of which voices carry one.
pub fn spare_count<F: crate::SamplerFloat>(pool: &crate::VoicePool<F>) -> usize {
    pool.spare_count()
}

/// A transport a test can move between blocks, as a real one moves.
///
/// Interior-mutable and shared (`Arc`), so a test can move it from inside a
/// closure that renders. A mock that never moved would have every frame
/// derive the same position, and an equivalence test over it would pass no
/// matter what the code under test does.
#[derive(Debug)]
pub struct MockTransport {
    playing: AtomicBool,
    /// `f64` bits — `AtomicF64` is not in std.
    beat: AtomicU64,
    tempo: AtomicU64,
    /// The loop the transport reports, if armed.
    looping: tutti_rt::Mutex<Option<LoopRange>>,
}

impl MockTransport {
    /// Rolling, at `beat` and `tempo`.
    pub fn rolling(beat: Beat, tempo: Bpm) -> Arc<Self> {
        Arc::new(Self {
            playing: AtomicBool::new(true),
            beat: AtomicU64::new(beat.get().to_bits()),
            tempo: AtomicU64::new(tempo.get().to_bits()),
            looping: tutti_rt::Mutex::new(None),
        })
    }

    /// Stopped, at `beat` and `tempo`. The placement gate reads whether the
    /// transport plays first, so this is how a test asserts
    /// silence-while-stopped.
    pub fn stopped(beat: Beat, tempo: Bpm) -> Arc<Self> {
        let t = Self::rolling(beat, tempo);
        t.playing.store(false, Ordering::Relaxed);
        t
    }

    /// Starts or stops the transport where it stands — the beat does not move.
    pub fn set_rolling(&self, rolling: bool) {
        self.playing.store(rolling, Ordering::Relaxed);
    }

    /// Jumps the playhead — a seek or a scrub, seen by the next block.
    pub fn set_beat(&self, beat: Beat) {
        self.beat.store(beat.get().to_bits(), Ordering::Relaxed);
    }

    /// Arms the transport's loop over `looping` (`None`: not looping), as the next
    /// block's `Env` carries it. The playhead does not wrap: a test that wants a wrap
    /// moves it with [`set_beat`](Self::set_beat).
    pub fn set_loop(&self, looping: Option<LoopRange>) {
        *self.looping.lock() = looping;
    }

    /// A seek: [`set_beat`](Self::set_beat), by the name a host uses.
    pub fn seek(&self, beat: Beat) {
        self.set_beat(beat);
    }

    /// The playhead's beat.
    pub fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }

    /// Whether it plays.
    pub fn is_rolling(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Its tempo.
    pub fn tempo(&self) -> Bpm {
        Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed))).unwrap_or_default()
    }

    /// Moves by `samples` at `sample_rate`, the way a block-driven transport does
    /// after a block is rendered. Negative `samples` rewinds, so a test can
    /// replay a span twice — which is why it is a signed `i64` and not
    /// [`Samples`] (an unsigned count that cannot carry the rewind).
    pub fn advance(&self, samples: i64, sample_rate: impl Into<SampleRate>) {
        let tempo = self.tempo().get();
        let beats = samples as f64 * tempo / 60.0 / sample_rate.into().get();
        let now = self.beat().get();
        self.beat.store((now + beats).to_bits(), Ordering::Relaxed);
    }

    /// The transport as a block's `Env` carries it: here, now.
    pub fn transport(&self) -> TransportPosition {
        TransportPosition::new(
            self.is_rolling(),
            self.tempo(),
            self.beat(),
            *self.looping.lock(),
        )
    }

    /// A block of `frames` at `rate` starting where the playhead stands.
    pub fn env(&self, frames: usize, rate: impl Into<SampleRate>) -> Env {
        Env {
            frame: Frame::new(0),
            sample_rate: rate.into(),
            block_len: Samples::new(frames),
            transport: self.transport(),
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        }
    }
}

/// `node`, prepared at `rate` for blocks of up to `max_block` frames.
pub fn prepared<N: Node>(node: N, rate: impl Into<SampleRate>, max_block: usize) -> N {
    contract::prepared(node, rate.into(), max_block)
}

/// One block of `frames` of `node` (no inputs) under `t` at `rate`; the
/// transport is not moved. One `Vec` per output channel.
pub fn block<F: Float>(
    node: &mut dyn Node<F>,
    t: &MockTransport,
    rate: impl Into<SampleRate>,
    frames: usize,
) -> Vec<Vec<F>> {
    let env = t.env(frames, rate);
    contract::drive_input(node, &env, Planes::default(), &[], &[]).audio
}

/// [`block`] for a node prepared under `prepare`: an offline one renders
/// outside `RtScope`, so it may allocate.
pub fn block_in<F: Float>(
    node: &mut dyn Node<F>,
    prepare: &Prepare,
    t: &MockTransport,
    frames: usize,
) -> Vec<Vec<F>> {
    let env = t.env(frames, prepare.sample_rate());
    contract::drive_input_in(node, prepare, &env, Planes::default(), &[], &[]).audio
}

/// `frames` of `node` in blocks of `block_len`, the transport moved after
/// each, as a host renders; planar.
pub fn play<F: Float>(
    node: &mut dyn Node<F>,
    t: &MockTransport,
    rate: impl Into<SampleRate> + Copy,
    frames: usize,
    block_len: usize,
) -> Vec<Vec<F>> {
    let rate: SampleRate = rate.into();
    let mut out: Vec<Vec<F>> = Vec::new();
    let mut done = 0;
    while done < frames {
        let n = block_len.min(frames - done);
        let b = block(node, t, rate, n);
        if out.is_empty() {
            out = vec![Vec::with_capacity(frames); b.len()];
        }
        for (o, c) in out.iter_mut().zip(b) {
            o.extend(c);
        }
        t.advance(n as i64, rate);
        done += n;
    }
    out
}

// ---------------------------------------------------------------------------
// Opening a stream in a test.
// ---------------------------------------------------------------------------

/// The stream of `opening` on a hand-stepped streamer: reads the file, adopts
/// it, steps the streamer until it has taken the stream, then until it has
/// primed (`step_until_idle`), so the reader's ring holds what the threaded
/// streamer would have read by the time a voice plays. The
/// [`StreamHandle`](crate::StreamHandle) is the caller's to keep while the voice plays:
/// dropping it ends the stream.
///
/// # Errors
///
/// The file's own failure ([`OpenStream::run`](crate::OpenStream::run)).
///
/// # Panics
///
/// If the streamer refuses the adopt: a test that expects a refusal calls
/// [`adopt`](crate::DiskStreamer::adopt) itself.
pub fn wait_for_open<F: tutti_rt::RingFloat>(
    streamer: &mut crate::DiskStreamer<crate::Manual<F>, F>,
    opening: crate::OpenStream<F>,
) -> Result<(crate::StreamHandle<F>, crate::StreamReader<F>), crate::OpenError> {
    let (stream, reader) = streamer
        .adopt(opening.run()?)
        .expect("a hand-stepped streamer adopts what it read");
    let _ = streamer.step_until_idle(1_000);
    Ok((stream, reader))
}

/// The stream of `opening` on a streamer running its own thread: reads the
/// file and adopts it. The stream is `Opening` until the streamer has primed
/// its ring.
///
/// # Errors
///
/// The file's own failure ([`OpenStream::run`](crate::OpenStream::run)).
///
/// # Panics
///
/// If the streamer refuses the adopt.
pub fn wait_for_open_threaded<D, F: tutti_rt::RingFloat>(
    streamer: &crate::DiskStreamer<D, F>,
    opening: crate::OpenStream<F>,
) -> Result<(crate::StreamHandle<F>, crate::StreamReader<F>), crate::OpenError> {
    Ok(streamer
        .adopt(opening.run()?)
        .expect("a running streamer adopts what it read"))
}

/// `pool` with the controls that steer it, without a graph: what a test that
/// renders a pool by hand writes its table through ([`Published::replace`](tutti_graph::Published::replace)).
/// A pool inserted into a graph takes its controls from `into_parts`.
pub fn controlled_pool<F: crate::SamplerFloat>(
    pool: crate::VoicePool<F>,
) -> (crate::VoicePool<F>, crate::VoicePoolControls<F>) {
    pool.controlled()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_moves_forward_and_rewinds() {
        let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
        // 120 BPM = 2 beats/s, so one second of samples is two beats.
        t.advance(44_100, SampleRate::SR_44K1);
        assert!((t.beat().get() - 2.0).abs() < 1e-9);
        t.advance(-44_100, SampleRate::SR_44K1);
        assert!(t.beat().get().abs() < 1e-9);
    }

    #[test]
    fn set_beat_jumps_without_regard_to_tempo() {
        let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
        t.set_beat(Beat::new(64.0));
        assert_eq!(t.beat(), Beat::new(64.0));
    }

    #[test]
    fn stopped_is_not_rolling_but_still_reports_its_position() {
        let t = MockTransport::stopped(Beat::new(8.0), Bpm::of(90.0));
        assert!(!t.is_rolling());
        assert_eq!(t.beat(), Beat::new(8.0));
        assert_eq!(t.tempo(), Bpm::of(90.0));
        let env = t.env(4, SampleRate::SR_48K);
        assert!(!env.transport.playing);
        assert_eq!(env.transport.beat(), Beat::new(8.0));
    }
}
