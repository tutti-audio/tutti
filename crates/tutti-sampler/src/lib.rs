#![doc = include_str!("../README.md")]
//! Sample playback for the Tutti audio engine: voices that play audio from memory
//! or stream it from disk, a per-track voice pool, and a phase-vocoder time
//! stretch.
//!
//! Use it to play clips on a timeline or free-running: a one-shot, a looped
//! region, a whole take streamed from disk, varispeed or time-stretched. It is a
//! clip player, not an instrument: there is no `note_on`, no keymap and no voice
//! stealing (see `tutti-polysynth` and `tutti-soundfont` for note-driven
//! playback). Most applications reach it through the `tutti` crate
//! (`tutti::sampler`, feature `sampler`).
//!
//! ## Two playback tiers
//!
//! A voice plays either from memory or streamed from disk, and the tier is the
//! caller's choice; the sampler never picks one on its own.
//!
//! - `MemorySource` plays a decoded `Wave` held in memory. It needs no thread and
//!   no file, and is a graph node on its own.
//! - `DiskVoice` plays a file streamed by the streamer, a background thread owned by
//!   `DiskStreamer`. The streamer decodes ahead of the playhead into a ring per
//!   stream; the audio thread only reads the ring, so the streaming tier is
//!   real-time safe. `probe` reads a file's header and says whether this build
//!   can stream it.
//!
//! Both are wrapped by `Voice` and played by one of two graph nodes:
//!
//! - `VoicePool`: every voice on a track in one node, controlled from the control
//!   thread through a `VoicePoolHandle` (add, remove, re-place, respeed, stretch).
//!   Adding or removing a voice is a queued command, not a graph edit.
//! - `VoiceNode`: a single voice as its own node, for previews and single clips,
//!   controlled through a `VoiceNodeHandle`.
//!
//! A voice placed on the transport (a start beat and an optional duration)
//! derives its read position from the playhead in its block's `Env`, so it enters,
//! exits, loops and follows a seek on the exact frame, wherever that falls in a
//! block. An unplaced voice runs free once started.
//!
//! ## Rates
//!
//! Three rate types keep the two tiers consistent: `PlaybackRate` is varispeed
//! (reading faster raises the pitch), `SrcRatio` is sample-rate conversion
//! (derived from the file and engine rates, never set by the user), and
//! `StretchFactor` drives the phase vocoder (duration changes, pitch does not).
//! A voice asked to stretch or transpose owns a
//! [`PhaseVocoder`](tutti_dsp::PhaseVocoder) that always runs, at unity too, so
//! its latency is constant. A memory voice reads its source the vocoder's latency
//! ahead of its place on the timeline, so it sounds on time and declares none. A
//! disk voice cannot read before where it seats, so it sounds the latency late: a
//! standalone `VoiceNode` declares it for delay compensation, and a `VoicePool`,
//! which takes voices while it plays, cannot. The same holds for any voice started
//! with no timeline position to read ahead of. A stretching voice's seat primes the
//! vocoder over several blocks, silent meanwhile: a seek, and play from a position the
//! transport has not stood at for 2 049 frames, are silent for 4 097
//! frames (85 ms at 48 kHz) and then sound on the timeline; a clip placed ahead of the
//! playhead, or at a position the transport has stood at, primes before it plays and
//! is on time. A loop wrap does not go silent: a second vocoder primes for it ahead
//! (unless the loop was set less than 2 049 frames before its first wrap). Nothing ties the stretch to the tempo.
//! All three are `tutti-units` types.
//!
//! ## Quick start
//!
//! An in-memory voice needs no streamer and no file, so it is a plain graph node:
//! build the `Wave`, wrap it, put it in a graph and render.
//!
//! ```rust
//! use tutti_buffers::MaxBlock;
//! use std::sync::Arc;
//! use tutti_units::{SampleRate, Samples};
//! use tutti_graph::{Prepare, Standalone};
//! use tutti_buffers::Interleaved;
//! use tutti_sampler::{MemorySource, Wave};
//!
//! // 100 stereo frames; `append` takes one frame, not one sample.
//! let mut wave = Wave::new(2, SampleRate::SR_44K1);
//! for _ in 0..100 {
//!     wave.append(Interleaved::new(&[0.5, 0.5], wave.layout()).unwrap()).unwrap();
//! }
//!
//! // Free-running (not placed on the transport), so it plays once started.
//! let source = MemorySource::new(Arc::new(wave)).expect("a stereo wave places");
//! source.play();
//!
//! // No audio input: the voice is the source. Stereo out.
//! let mut graph = Standalone::new(source, Prepare::new(SampleRate::SR_44K1, MaxBlock::of(Samples::new(64))));
//! let out = graph.render(64);
//! assert_eq!(out.len(), 2);
//! assert!(out[0].iter().all(|&s| s != 0.0));
//! ```
//!
//! Streaming needs a real file. Build a `DiskStreamer` once with
//! `DiskStreamer::start` (it fails if the OS refuses the streamer's thread), open a
//! stream with `open`, and build the `DiskVoice` from the reader the open answers
//! with. The `Stream` it answers with is the control side: `Stream::status` reports
//! whether the ring is primed, ended or faulted, and how many frames the voice
//! missed. `DiskStreamer`'s own documentation has that example.
//!
//! ## Sample format
//!
//! Every voice type is generic over the graph's float format, `f32` by default
//! (`MemorySource<F>`, `DiskVoice<F>`, `Voice<F>`, `VoicePool<F>`, `VoiceNode<F>`,
//! `Clip<F>`), and plays in a graph of that format; a disk voice's stream is
//! opened by a [`DiskStreamer`] of the same format. Playback controls (speed,
//! stretch factor, gain) are `f64` units whatever the format; a stretched
//! voice's vocoder runs in the graph's format too.
//!
//! ## Channel ceiling
//!
//! `tutti_buffers::MAX_CHANNELS` is the widest frame a voice reads or emits, the
//! same cap as a bus and the graph root: a voice wider than the root could
//! render would be truncated downstream, so the sampler refuses it here instead
//! (`VoicePool::with_channels` returns `PoolTooWide`).
//!
//! ## Features
//!
//! `default = []`.
//!
//! - No decoder feature: a decoder is `tutti-codec`'s `decode-*` feature, which an
//!   app turns on there. With none on, `probe` and `SampleFacts` are absent: no
//!   header can be read.
//! - `testing`: `DiskStreamer::manual` and `step`, which run the
//!   streamer's cycle by hand instead of as a future on an executor, and the `testing` module (a
//!   mock transport and block driver). For tests; a host has no use for it.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Main types
//!
//! - [`MemorySource`]: in-memory playback of a [`Wave`], configured with
//!   [`MemorySourceConfig`], [`Loop`] and [`VoiceWindow`].
//! - [`DiskVoice`]: disk-streamed playback, built from the [`StreamReader`] a
//!   [`DiskStreamer::adopt`] answers with.
//! - [`DiskStreamer`]: owns the streamer future; a stream it opens is a
//!   [`Stream`] (the control side: steer it, drop it to end it) and a
//!   [`StreamReader`] (the voice's).
//! - [`Voice`], [`VoiceSource`], [`Playback`]: a voice of either tier and its
//!   control state.
//! - [`VoicePool`] with [`VoicePoolHandle`], and [`VoiceNode`] with
//!   [`VoiceNodeHandle`]: the graph nodes that play voices, and their
//!   control-thread handles (add, steer and remove voices; a node's handle
//!   owns the disk stream that feeds it).
//! - [`Clip`]: one sample placed on the timeline, resident or streamed.
//! - [`Loop`]: the one checked loop every tier takes.
//! - `probe`: the header probe that informs a caller's tier choice (the
//!   sampler never decides the tier on its own).
//!
//! ## License
//!
//! MIT OR Apache-2.0.

mod clip;
pub use clip::Clip;
mod error;
pub use error::{Error, PlaceError, Result};

/// [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) as a layout's count. The cast is
/// checked at compile time: the build fails if the cap ever grows past what a
/// layout holds.
pub(crate) const MAX_SAMPLER_WIDTH: u16 = {
    assert!(tutti_buffers::MAX_CHANNELS <= u16::MAX as usize);
    tutti_buffers::MAX_CHANNELS as u16
};

/// Reject the empty layout for anything that is a graph node.
///
/// [`ChannelLayout`](tutti_buffers::ChannelLayout) can represent an empty bus
/// (`Multi(0)`) — deliberately, because a plugin port genuinely can be zero
/// wide. A sampler node cannot: a node that reports zero outputs is a node
/// nothing can be wired to. So the widths a node's `Shape` declares go
/// through here.
///
/// This is the one place that clamp lives: the layout carries the declaration
/// and this carries the node-arity invariant, so no call site has to re-remember
/// the rule. The upper bound is *not* applied here — [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) is
/// a per-read stack ceiling, not a limit on what a node may declare, and only
/// [`DiskVoice`] needs it.
#[inline]
pub(crate) fn nonempty(layout: tutti_buffers::ChannelLayout) -> tutti_buffers::ChannelLayout {
    if layout.count() == 0 {
        tutti_buffers::ChannelLayout::MONO
    } else {
        layout
    }
}

// One mock transport and block driver for every test of the crate, in-crate
// and in `tests/` (through `testing`): a node reads the transport from
// its block's `Env`, and this is what hands it one.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

// The resident buffer every voice constructor takes (`MemorySource::new`,
// `Voice`'s memory tier) is a `tutti-buffers` type. Re-exported so a consumer building
// a voice needs no direct `tutti-buffers` dependency.
pub use tutti_buffers::Wave;

// Voice playback: the two tier units, the mixer over them, and the kernels they
// share.
mod voice;

// Planar block scratch and its kernels: the voices render into it, the
// stretch vocoder reads and writes it. Crate-level because both use it.
mod lanes;

/// A bound alias, with a blanket impl: a [`Float`](tutti_buffers::Float) the sampler's
/// voices run in, `f32` or `f64`, which a ring holds and the stretch vocoder's FFT
/// transforms.
pub trait SamplerFloat: tutti_rt::RingFloat + tutti_dsp::FftFloat {}

impl<F: tutti_rt::RingFloat + tutti_dsp::FftFloat> SamplerFloat for F {}

// DSP leaves + value types from `voice`, usable as plain
// `tutti_graph` nodes. The streamer's `LruCache` /
// `StreamPin` are internal machinery a consumer never constructs, so they stay
// `pub(crate)`.
// `DiskVoiceConfig` and `DiskSource` are deliberately NOT re-exported: nothing
// outside this crate constructs them. `DiskSource` in particular is
// `DiskVoice`'s `inner` — one capability, and only the outer type is a doorway.
//
pub use voice::{
    Direction, DiskVoice, DiskVoiceControls, Loop, LoopError, MemorySource, MemorySourceConfig,
    PlayError, Playback, PoolDrops, PoolTooWide, SendError, SlotId, StreamingAddError, Voice,
    VoiceNode, VoiceNodeHandle, VoicePool, VoicePoolHandle, VoiceSource, VoiceWindow, memory_voice,
    voice_width,
};
// The async disk-streaming engine (streamer future + the `DiskStreamer` handle).
pub(crate) mod streamer;

pub use streamer::{
    AdoptError, Block, ControlCell, DiskStreamer, DiskStreamerConfig, GUARD_FRAMES, OpenError,
    OpenStream, OpenedStream, RefusedStreamStart, Running, Stream, StreamFault, StreamFile,
    StreamId, StreamPhase, StreamReader, StreamStart, StreamStatus,
};

// The hand-driven streamer cycle, alongside `DiskStreamer::manual`: its driver
// type and its verdict.
#[cfg(any(test, feature = "testing"))]
pub use streamer::{Manual, StepOutcome};

// Header-only probe: what a file is, and whether *this build's* streamer can
// stream it. Public because the tier decision is the caller's or the
// session's, never the sampler's alone, but the
// streamability half of it is this crate's own capability — so the host and the
// streamer read one function rather than two copies of a rule.
//
// Codec-gated because it *is* the codec layer: `read_wave_metadata` and
// `WaveMetadata` are themselves gated in `tutti-codec`, so with no format feature
// there is no header to read.
#[cfg(codec)]
mod probe;
#[cfg(codec)]
pub use probe::{ProbeError, SampleFacts, probe};
