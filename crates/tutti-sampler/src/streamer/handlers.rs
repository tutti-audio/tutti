//! Streamer command handlers + their state types.
//!
//! `Handles` — Arc'd state also held by the driver.
//! `Local`    — streamer-thread-local data (the stream table, scratch).
//! `handle_command` — the driver of [`step`]: it does the file work a command
//! needs, runs the pure step over the table, and performs the effects.

use std::sync::Arc;

use tutti_rt::AtomicCell;
use tutti_units::{SampleRate, Samples, SrcRatio};

use super::cache::LruCache;
use super::cells::Cells;
use super::command::{StreamId, StreamerCommand};
use super::config::BufferConfig;
use super::control::StreamFile;
use super::control_cell::ControlCell;
#[cfg(test)]
use super::decode::Seams;
use super::decode::Source;
use super::loops::{LeadIn, Mapping, RingLoop, apply_mapping, buffer_size_for_file};
use super::metrics::Metrics;
use super::open::Opened;
use super::prefetch::RegionBuffer;
use super::record::{Cmd, Record, Streams, step};
use super::status::StatusCell;
use crate::Loop;
use crate::PlaceError;
use crate::voice::PlayDirection;
use crate::voice::interp::Place;
use crate::voice_width;
use tutti_buffers::ChannelLayout;
use tutti_platform::Asset;
use tutti_rt::RingFloat;

/// Arc'd handles shared between the control side and the streamer loop.
/// `Clone` is cheap — every field is an `Arc`.
#[derive(Clone)]
pub(super) struct Handles<F: RingFloat> {
    pub cache: Arc<LruCache<F>>,
    pub metrics: Arc<Metrics>,
    /// What wakes the streamer: every stream's reader rings it.
    pub bell: tutti_rt::Bell,
    /// The session rate each stream's [`SrcRatio`] is derived against.
    pub session_rate: SessionRate,
    /// What a test replaces (production: none).
    #[cfg(test)]
    pub seams: Seams,
}

#[cfg(test)]
impl<F: RingFloat> Handles<F> {
    /// Handles for a test that drives a cycle itself: a bell nobody awaits and an
    /// executor that is never polled.
    pub(super) fn detached(config: &BufferConfig, rate: SampleRate) -> Self {
        let executor = tutti_platform::Executor::new();
        let bells = tutti_rt::Bells::new(&executor).expect("the bells' tick task");
        Self {
            cache: Arc::new(LruCache::new(
                config.cache_max_entries,
                config.cache_max_bytes,
            )),
            metrics: Arc::new(Metrics::new()),
            bell: bells.bell().0,
            session_rate: SessionRate::new(rate),
            seams: Seams::default(),
        }
    }
}

/// The session (graph) rate, one cell shared by the control side and the
/// streamer ([`DiskStreamer::sample_rate`](crate::DiskStreamer::sample_rate)).
///
/// Shared rather than copied into each because a device restart moves it
/// (`DiskStreamer::set_sample_rate`): a copy taken at build time is how a
/// stream kept converting to 44.1 kHz on a 48 kHz device — every streamed clip
/// ~8.8% sharp and fast, with no error anywhere.
///
/// It is a value and nothing more. A stream's conversion ratio is the
/// streamer's own (it is the one writer, `Record::rerate`): it derives it from
/// this cell when it builds the stream and again on each `SessionRate` command,
/// which the control side sends after storing the rate here. A stream built
/// between the store and the command reads the new rate either way, and the
/// command re-derives it to the same ratio, so no lock orders the two.
#[derive(Clone)]
// The `SessionRate` command sent after a store orders the streamer's read of it.
pub(crate) struct SessionRate(Arc<AtomicCell<SampleRate>>);

impl SessionRate {
    pub(crate) fn new(rate: SampleRate) -> Self {
        Self(Arc::new(AtomicCell::new(rate)))
    }

    pub(crate) fn get(&self) -> SampleRate {
        self.0.load()
    }

    /// Move the rate. Control thread; the streamer re-derives its ratios when
    /// it is told ([`StreamerCommand::SessionRate`]).
    pub(super) fn set(&self, rate: SampleRate) {
        self.0.store(rate);
    }
}

/// Streamer-thread-local state. Never shared. Plain data.
pub(super) struct Local<F: RingFloat> {
    /// Every open stream, one record each.
    pub streams: Streams<F>,
    /// Flat interleaved refill scratch. Streamer-thread-local, so it may grow.
    pub interleave_buffer: Vec<F>,
}

impl<F: RingFloat> Local<F> {
    /// Thread-local state with no streams, a scratch buffer pre-sized for one
    /// `base_chunk_size` refill.
    pub(super) fn new(base_chunk_size: usize) -> Self {
        Self {
            streams: Streams::new(),
            interleave_buffer: Vec::with_capacity(base_chunk_size),
        }
    }
}

/// Dispatch one command on the streamer thread: a pure [`step`] over the
/// table, which silently no-ops for a stream the streamer no longer holds:
/// commands race the streams they name, and there is no caller left to report
/// to by the time the streamer sees one.
pub(super) fn handle_command<F: RingFloat>(
    cmd: StreamerCommand<F>,
    shared: &Handles<F>,
    local: &mut Local<F>,
) {
    match cmd {
        // The stream is live before the handle that names it is used: a
        // record is never refilled while `Opening`.
        StreamerCommand::Adopt(record) => {
            let id = record.id();
            let _ = step(&mut local.streams, Cmd::Open(record));
            let _ = step(&mut local.streams, Cmd::Opened(id));
        }
        StreamerCommand::Close { stream } => {
            let _ = step(&mut local.streams, Cmd::Close(stream));
        }
        StreamerCommand::SessionRate => local.streams.rerate(shared.session_rate.get()),
    }
}

/// The incremental decoder a seekable stream reads through; none without a
/// codec.
#[cfg(codec)]
type Decoder<F> = tutti_codec::FileIn<F>;
#[cfg(not(codec))]
type Decoder<F> = (std::convert::Infallible, std::marker::PhantomData<F>);

/// Build the stream `id` on `asset` from frame `start`: its record and
/// what its reader is made of. Control thread: it allocates the ring.
///
/// Builds the region ring at the file's own width and pins the wave in the
/// cache for the stream's lifetime. The placement into `width` (the file's own
/// when `None`) is built, and so refused, before anything is built.
pub(super) fn build_stream<F: RingFloat>(
    id: StreamId,
    asset: Asset,
    start: u64,
    width: Option<ChannelLayout>,
    source: Source<F>,
    shared: &Handles<F>,
    config: &BufferConfig,
) -> Result<(Record<F>, Opened<F>), PlaceError> {
    let (file_length, file_sr, file_topology, decoder, resident): (_, _, _, Option<Decoder<F>>, _) =
        match source {
            // Decoder metadata carries the header's integer rate.
            #[cfg(codec)]
            Source::Stream(meta, decoder) => (
                meta.total_frames.unwrap_or(0),
                meta.sample_rate,
                decoder.topology().clone(),
                Some(*decoder),
                None,
            ),
            Source::Whole(wave) => (
                wave.len().get() as u64,
                wave.sample_rate(),
                wave.topology().clone(),
                None,
                Some(wave),
            ),
        };

    // Where the file's channels go in the voice, built (and so refused) now,
    // before a ring exists to free again.
    let width = voice_width(width.unwrap_or_else(|| file_topology.width()))?;
    let place = Place::for_topology(&file_topology, width)?;

    // Sizing only: a rate that moves before the ratio is set below changes the
    // ring's depth, never what it plays.
    let buffer_capacity = buffer_size_for_file(file_length, shared.session_rate.get());

    // The ring carries the file at its OWN width and order, and its topology:
    // the voice that reads the ring places it through the same `Place` the
    // in-memory tier uses, so folding or permuting here would only be a second
    // copy of that placement (and the loop's crossfades and an
    // offline fork's own read of the file all work at the file's width).
    let (mut producer, ring, pos) = RegionBuffer::for_file(
        id,
        asset.clone(),
        buffer_capacity,
        file_topology,
        Some(Samples::new(file_length as usize)),
        config.seek_crossfade_frames,
    );

    // `mut` is used by the `set_decoder` call, which every codec feature
    // gates — so a build with none of them on sees an unused `mut` rather than
    // a dead binding.
    #[cfg(codec)]
    if let Some(decoder) = decoder {
        producer.set_decoder(decoder);
    }
    #[cfg(not(codec))]
    let _ = decoder;
    #[cfg(test)]
    if let Some(hook) = &shared.seams.read_hook {
        producer.set_read_hook(Arc::clone(hook));
    }
    // A file that cannot seek is held whole by its writer, not only by the
    // cache: a file too large for the cache still streams.
    let whole = resident.clone();
    if let Some(wave) = resident {
        producer.set_resident(wave);
    }

    // The window starts where the reader will play: the offset. A placed
    // voice reads at its clock; the compiler aligns the voice in the graph.
    RegionBuffer::place(&mut producer, start);

    // Pin the streamed wave in the LRU cache for the stream's lifetime. On the
    // whole-file path the open's decode inserted the file into the cache, so this
    // protects that resident Wave from being evicted mid-read (a fully-buffered
    // stream goes cold and would otherwise be picked as the LRU victim). On the
    // incremental-decode path the wave isn't cached, so `pin` finds no entry and
    // the guard is inert — harmless. Held by the record, so it releases when
    // the stream closes.
    let pin = shared.cache.pin(asset.identity().as_ref());

    // The conversion ratio is the streamer's to write: derived here against the
    // session rate as it is now, and again on each rate event.
    let cells = Arc::new(Cells::new());
    cells.set_src_ratio(SrcRatio::for_rates(file_sr, shared.session_rate.get()));
    let status = Arc::new(StatusCell::new());
    // What a fork plays, immutable: the asset and the file's rate, and the wave when
    // the writer holds it whole.
    let file = Arc::new(StreamFile::new(asset, file_sr, whole));
    let control = ring.control_cell();
    // The loop and direction the control side asks for: the record reads
    // them, the stream's handle (and the voice node, for a node) publishes.
    let looping = ControlCell::ringing(None::<Loop>, Arc::new(shared.bell.clone()));
    let direction = ControlCell::ringing(PlayDirection::Forward, Arc::new(shared.bell.clone()));
    let record = Record::new(
        producer,
        Arc::clone(&cells),
        file_sr,
        Samples::new(file_length as usize),
        pin,
        Arc::clone(&status),
        looping.reader(),
        direction.reader(),
    );
    Ok((
        record,
        Opened {
            id,
            ring,
            pos,
            cells,
            status,
            file,
            control,
            looping,
            direction,
            place,
            width,
            bell: Arc::new(shared.bell.clone()),
        },
    ))
}

/// Takes the loop and direction each live stream's control side now asks for:
/// the cells its voice steers are the streamer's to read, and a change is
/// applied here, once, as the ring's mapping.
///
/// A loop goes through [`apply_mapping`]: free when it changes no frame the
/// ring holds, else a switch the reader crosses (see `loops`' module docs)
/// — heard where the memory tier hears it when that is far enough ahead, else
/// just past the block the reader is in, crossfaded. A reversed stream
/// ignores its loop, so there the change is only stored.
///
/// Only the frames the loop needs are read — its fade's lead-in, and its body
/// when it is short (`RingLoop::capture`), through the region's decoder: a
/// loop change never decodes the whole file. A lead-in that cannot be read
/// plays the loop hard, is logged, and is what the ring's published control
/// says is applied (so a fork plays it hard too). The file work runs
/// contained like a refill.
pub(super) fn follow_steering<F: RingFloat>(config: &BufferConfig, local: &mut Local<F>) {
    for record in local.streams.records_mut() {
        if !record.is_live() {
            continue;
        }
        if let Some(ask) = record.loop_asked() {
            let _ = record.contained(|record| capture_loop(record, ask, config));
        }
        if let Some(direction) = record.direction_asked() {
            let _ = record.contained(|record| turn(record, direction, config));
        }
    }
    local.streams.reap();
}

/// Captures `ask` into `record`'s ring and records it as applied: the file
/// work of [`follow_steering`]'s loop.
fn capture_loop<F: RingFloat>(record: &mut Record<F>, ask: Option<Loop>, config: &BufferConfig) {
    let len = record.file_frames.get();
    let rate = record.cells.read_rate();
    let writer = &mut record.out;

    let channels = writer.channels();
    let mut recorded = ask;
    let ring_loop = ask.and_then(|ask| {
        let (ring_loop, lead_in) =
            RingLoop::capture(ask, len, channels, &mut |at, out| writer.read_file(at, out))?;
        if lead_in == LeadIn::Unreadable {
            tracing::warn!(
                "could not read the crossfade lead-in of loop {:?} in {}; \
                 the loop plays hard",
                ask,
                writer.asset().name()
            );
            recorded = Some(ask.hard());
        }
        Some(ring_loop)
    });
    let new = Mapping {
        ring_loop,
        ..writer.content().current.clone()
    };
    apply_mapping(writer, new, config.seek_crossfade_frames, rate);

    // The loop as applied, published beside the map apply_mapping published
    // (or kept, when the mapping did not change).
    writer.ring().publish_loop(recorded);
}

/// Turn a stream to `direction`.
///
/// Reverse is a mapping (the file mirrored, the loop ignored), so the change is
/// [`apply_mapping`]'s: a switch the reader crossfades across just past the
/// block it is in. The direction is the published map's own, so the one
/// publish carries it to the voice and to a fork.
fn turn<F: RingFloat>(record: &mut Record<F>, direction: PlayDirection, config: &BufferConfig) {
    let reverse = direction.is_reverse();
    let rate = record.cells.read_rate();
    let writer = &mut record.out;
    if writer.content().current.reverse == reverse {
        return;
    }
    let new = Mapping {
        reverse,
        ..writer.content().current.clone()
    };
    apply_mapping(writer, new, config.seek_crossfade_frames, rate);
}
