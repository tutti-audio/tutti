//! Asynchronous disk I/O for audio file streaming.
//!
//! A background "streamer" future decodes from disk into one position-indexed
//! ring per stream, which the stream's voice reads on the audio thread (it
//! never pops: a position, not an order, is what it reads). Nothing here blocks
//! or allocates on the audio thread — the split is what makes the streaming
//! tier real-time safe.
//!
//! The streamer holds one [`record`] per open stream and changes the table by
//! a pure step; the control side of a stream is [`Stream`], whose drop ends it.

mod bell;
mod block;
mod cache;
mod cells;
mod command;
mod config;
#[cfg(test)]
mod contained;
mod control;
mod control_cell;
mod decode;
mod handlers;
mod io;
mod loop_body;
mod loops;
mod metrics;
mod open;
mod prefetch;
mod record;
mod reply;
mod status;
mod step;
mod stream;
mod thread;
#[cfg(test)]
mod wake;

pub(crate) use cells::Cells;
pub use command::StreamId;
pub(crate) use command::StreamerCommand;
pub(crate) use config::BufferConfig;
pub use loops::GUARD_FRAMES;
#[cfg(test)]
pub(crate) use loops::RingControl;
pub(crate) use loops::{Arrangement, RingMap, straight};
pub(crate) use prefetch::SharedReader;
#[cfg(any(test, feature = "testing"))]
pub use thread::Manual;
pub use thread::Running;

pub(crate) use bell::StreamerSender;

// The streamer's public face: the handle a host holds to drive disk streaming,
// and the one failure every stream-control command can report.
pub use block::Block;
pub(crate) use block::RingReader;
pub use control::StreamFile;
pub use control_cell::ControlCell;
pub use open::{Open, OpenError, PendingOpen, Placement, RefusedPlacement, StreamReader};
pub(crate) use status::StatusCell;
pub use status::{StreamFault, StreamPhase, StreamStatus};
pub use stream::Stream;
pub(crate) use stream::StreamerPort;

// The hand-driven cycle's verdict. Public only alongside the driver that
// produces it — the threaded streamer consumes its own outcomes.
#[cfg(any(test, feature = "testing"))]
pub use step::StepOutcome;

// Test-only re-exports for unit tests outside the streamer module tree (e.g.
// `units::disk_voice`) that build readers directly. Gated so they
// do not count as dead code in normal builds.
#[cfg(test)]
pub(crate) use prefetch::RegionBuffer;

use crate::error::Result;
use decode::Seams;
use handlers::Handles;
use thread::Built;
use tutti_platform::Asset;
use tutti_platform::Executor;
use tutti_rt::Bells;
use tutti_rt::Mint;
use tutti_rt::RingFloat;
use tutti_units::SampleRate;

/// The disk-streaming engine: owns the streamer future, run by an
/// [`Executor`], that reads files into per-stream rings for
/// [`DiskVoice`](crate::DiskVoice)s to play.
///
/// A host builds one at startup with [`start`](Self::start) and holds it for the
/// lifetime of the audio engine; dropping it asks the streamer future to end. A
/// stream is opened with [`open`](Self::open), whose reply is the stream's
/// control-side [`Stream`] and the [`StreamReader`] a
/// [`DiskVoice`](crate::DiskVoice) is built from; [`sample_rate`](Self::sample_rate)
/// is the session rate it streams at.
///
/// Its type says how it is driven: [`DiskStreamer<Running, f32>`](Running) (the
/// default, from `start`) as a future on an executor, or, with the `testing`
/// feature, `DiskStreamer<Manual<f32>, f32>` from `manual`,
/// stepped by the caller.
///
/// All of this is control-thread API. The audio thread only ever touches the
/// `DiskVoice` built from the reader, which reads its ring without locking or
/// allocating.
///
/// # Examples
///
/// Open a file and build the voice that plays it:
///
/// ```no_run
/// use tutti_units::Beat;
/// use tutti_sampler::{DiskStreamer, DiskVoice, Open, Placement, Running, VoiceWindow};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let executor = tutti_platform::Executor::threaded();
/// let bells = tutti_rt::Bells::new()?;
/// let streamer = DiskStreamer::<Running, f32>::start(
///     tutti_units::SampleRate::SR_48K,
///     Default::default(),
///     &executor,
///     &bells,
/// )?;
///
/// // The streamer opens the file on its own; the reply is polled, and
/// // a poll that is not ready hands the `PendingOpen` back (awaiting it is the
/// // other way to wait).
/// let mut opening = streamer.open("take_1.wav", Placement::HEAD);
/// let (stream, reader) = loop {
///     match opening.poll() {
///         Open::Ready(stream, reader) => break (stream, reader),
///         Open::Pending(again) => opening = again,
///         Open::Failed(error) => return Err(error.into()),
///     }
///     std::thread::sleep(std::time::Duration::from_millis(1));
/// };
///
/// // The reader is the voice's, by value: place it at beat 0 for the file's
/// // full length. `voice` is a graph node: insert it into the graph that
/// // renders the track. The `stream` is the control thread's: keep it while
/// // the voice plays, drop it to end the stream.
/// let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
/// # drop((voice, stream));
/// # Ok(())
/// # }
/// ```
///
/// A streamer on an executor cannot be stepped by hand: the method belongs
/// to the other state, so this does not compile.
///
/// ```compile_fail
/// # fn run() -> tutti_sampler::Result<()> {
/// let (executor, bells) = (tutti_platform::Executor::threaded(), tutti_rt::Bells::new()?);
/// let mut streamer = tutti_sampler::DiskStreamer::<tutti_sampler::Running, f32>::start(
///     tutti_units::SampleRate::SR_48K,
///     Default::default(),
///     &executor,
///     &bells,
/// )?;
/// streamer.step(); // only a `DiskStreamer<Manual<f32>, f32>` steps
/// # Ok(())
/// # }
/// ```
///
/// Its compiling twin steps a hand-driven one:
///
/// ```
/// # fn run() -> tutti_sampler::Result<()> {
/// let mut streamer = tutti_sampler::DiskStreamer::<_, f32>::manual(tutti_units::SampleRate::SR_48K, Default::default())?;
/// let _ = streamer.step();
/// # Ok(())
/// # }
/// ```
pub struct DiskStreamer<D, F: RingFloat> {
    streamer_tx: StreamerSender<F>,
    shared: Handles<F>,
    /// The driver: the future's stop handle, which only its drop uses, or the caller's
    /// cycle.
    driver: D,
    /// The next stream's identity, minted here on the control side.
    next_stream: Mint,
}

// The driver holds a stop handle or a cycle that can't derive `Debug`; print
// the sample rate and note the live streamer.
impl<D, F: RingFloat> std::fmt::Debug for DiskStreamer<D, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskStreamer")
            .field("sample_rate", &self.shared.session_rate.get().get())
            .finish_non_exhaustive()
    }
}

impl<F: RingFloat> DiskStreamer<Running, F> {
    /// Creates the streaming engine at `sample_rate` (the engine's rate) and
    /// spawns the streamer future on `executor`. Its streams decode into `F` and
    /// its rings hold `F`. `bells` makes the bell the audio thread rings to wake
    /// it.
    ///
    /// Configure with [`DiskStreamerConfig`] (`Default` + struct-update); pass
    /// `Default::default()` for the tuned defaults. Allocates and spawns: control
    /// thread only. A threaded executor gives the streamer a thread of its own at a
    /// raised priority; a polled or host executor runs it at the caller's or the
    /// host's, and a polled one runs it only inside its `poll`.
    ///
    /// # Errors
    ///
    /// [`Error::Spawn`](crate::Error::Spawn) when the executor refuses the future
    /// (the OS refusing the thread). Every stream and every refill depends on it,
    /// so there is no streamer to return: a stream command with no consumer would
    /// stall silently.
    pub fn start(
        sample_rate: impl Into<SampleRate>,
        config: DiskStreamerConfig,
        executor: &Executor,
        bells: &Bells,
    ) -> Result<Self> {
        Self::build(
            sample_rate.into(),
            config,
            Seams::default(),
            executor,
            bells,
        )
    }

    /// [`start`](Self::start) with what a test replaces: the hooks where a file is
    /// read and decoded.
    fn build(
        sample_rate: SampleRate,
        config: DiskStreamerConfig,
        seams: Seams,
        executor: &Executor,
        bells: &Bells,
    ) -> Result<Self> {
        Self::from_built(
            Built::new(256, sample_rate, config.buffer_config, seams, bells),
            executor,
        )
    }

    /// The streamer over `built`, its future spawned.
    fn from_built(built: Built<F>, executor: &Executor) -> Result<Self> {
        let (streamer_tx, shared) = (built.command_sender(), built.shared());

        Ok(DiskStreamer {
            streamer_tx,
            shared,
            driver: built.start(executor)?,
            next_stream: Mint::new(),
        })
    }

    /// Rings the streamer's bell with no command, so it takes a cycle: the
    /// control side's tick, which sweeps every ring's retired controls. A session
    /// calls it once per `update`.
    pub fn wake(&self) {
        self.driver.wake();
    }
}

#[cfg(any(test, feature = "testing"))]
impl<F: RingFloat> DiskStreamer<Manual<F>, F> {
    /// Creates the streaming engine without spawning the streamer future;
    /// cycles are then run by hand with `step`.
    /// Requires the `testing` feature.
    ///
    /// The step is the same one the future runs, so this is the shipped path
    /// with its sleeping removed rather than a parallel implementation. What it
    /// buys is a streamer whose progress is *counted* instead of waited for: a
    /// test can say "after this many cycles the ring holds audio", where the
    /// threaded streamer only lets it say "after this long it probably does".
    ///
    /// # Errors
    ///
    /// None at present, as for [`start`](DiskStreamer::start).
    pub fn manual(sample_rate: impl Into<SampleRate>, config: DiskStreamerConfig) -> Result<Self> {
        Self::manual_with(sample_rate.into(), config, Seams::default())
    }

    /// [`manual`](Self::manual) with what a test replaces.
    fn manual_with(
        sample_rate: SampleRate,
        config: DiskStreamerConfig,
        seams: Seams,
    ) -> Result<Self> {
        let bells = Bells::new().map_err(crate::Error::Spawn)?;
        let built = Built::new(256, sample_rate, config.buffer_config, seams, &bells);
        let (streamer_tx, shared) = (built.command_sender(), built.shared());

        Ok(DiskStreamer {
            streamer_tx,
            shared,
            driver: built.manual(),
            next_stream: Mint::new(),
        })
    }

    /// Runs one streamer cycle on the calling thread, reporting whether the
    /// streamer still has urgent work ([`Busy`](crate::StepOutcome::Busy)) or has
    /// caught up ([`Healthy`](crate::StepOutcome::Healthy) /
    /// [`Idle`](crate::StepOutcome::Idle)).
    ///
    /// That verdict is what makes a step-driven test terminate on a *condition*
    /// rather than on a step budget: stepping until `Healthy` primes every ring
    /// exactly as far as the threaded streamer would before it parks.
    ///
    /// There is no thread to race: this method exists only on a
    /// `DiskStreamer<Manual<f32>, f32>`.
    pub fn step(&mut self) -> StepOutcome {
        self.driver.step()
    }

    /// Steps until the streamer stops making progress, or `budget` cycles have run
    /// — whichever comes first. Returns the number of cycles taken.
    ///
    /// "Stops making progress" is any outcome other than
    /// [`Busy`](StepOutcome::Busy): the rings are full enough
    /// ([`Healthy`](StepOutcome::Healthy)), nothing is streaming
    /// ([`Idle`](StepOutcome::Idle)), or a ring below threshold is one no
    /// further cycle can grow ([`Stalled`](StepOutcome::Stalled), the
    /// normal state near the end of a file). Waiting for `Healthy` alone would
    /// never return there.
    ///
    /// The budget is a liveness bound, not a pacing knob: reaching it means the
    /// streamer is refilling forever without ever catching up, and a caller should
    /// treat that as the failure it is rather than proceeding to measure.
    #[must_use = "an exhausted budget means the rings never primed"]
    pub fn step_until_idle(&mut self, budget: usize) -> usize {
        for taken in 0..budget {
            if self.step() != StepOutcome::Busy {
                return taken + 1;
            }
        }
        budget
    }
}

impl<D, F: RingFloat> DiskStreamer<D, F> {
    /// Opens `asset` as a new stream, its reader starting at `place`, and
    /// answers with an [`PendingOpen`]: poll it for the stream's [`Stream`] and its
    /// one [`StreamReader`]. The voice is as wide as the file
    /// ([`voice_width`](crate::voice_width)); [`open_into`](Self::open_into)
    /// asks for another width.
    ///
    /// Every open is a stream of its own, with its own ring: two opens of one
    /// file are two streams, and a reader belongs to the open it came from.
    ///
    /// Control thread. It waits while the streamer's queue is full, as every
    /// stream command does. A streamer that is gone answers
    /// [`OpenError::StreamerGone`] at the first poll.
    pub fn open(&self, asset: impl Into<Asset>, place: Placement) -> PendingOpen<F> {
        self.open_as(asset.into(), place, None)
    }

    /// [`open`](Self::open) for a voice `width` channels wide: the file's
    /// channels are placed into it by their
    /// [`ChannelTopology`](tutti_buffers::ChannelTopology), as
    /// [`memory_voice`](crate::memory_voice) places a wave. A file in another
    /// order than the engine's (a WAV 7.1 lists the rears before the sides) is
    /// permuted to it, one wider than `width` with an order is folded into it,
    /// and a mono one takes the centre of a wider frame. A placement that
    /// cannot be made (a channel no speaker names, or a narrowing with an
    /// unordered end: a 5.1 file into 5 channels) is
    /// [`OpenError::Placement`], and nothing is opened.
    pub fn open_into(
        &self,
        asset: impl Into<Asset>,
        place: Placement,
        width: tutti_buffers::ChannelLayout,
    ) -> PendingOpen<F> {
        self.open_as(asset.into(), place, Some(width))
    }

    fn open_as(
        &self,
        asset: Asset,
        place: Placement,
        width: Option<tutti_buffers::ChannelLayout>,
    ) -> PendingOpen<F> {
        let id = StreamId::mint(&self.next_stream);
        let (reply, answer) = reply::reply();
        // A refused send drops the command and with it `reply`, which is how
        // a gone streamer reaches the caller: the answer is disconnected.
        let _ = self.streamer_tx.send(StreamerCommand::Open {
            id,
            asset,
            start: place.frame(),
            width,
            reply,
        });
        PendingOpen::new(id, answer, StreamerPort::new(self.streamer_tx.clone()))
    }

    /// The session rate: the one the streamer was built with, or the one
    /// [`set_sample_rate`](Self::set_sample_rate) last moved it to.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        self.shared.session_rate.get()
    }

    /// Moves the session rate to `sample_rate`: what a device restart at a new
    /// rate does, between the stream's stop and its start.
    ///
    /// Every open stream keeps its file and its position (in file frames, which
    /// the rate does not touch) and has its conversion ratio re-derived against
    /// the new rate, so it plays on at the right pitch and speed; a stream
    /// opened later derives its ratio against the new rate too, and
    /// [`sample_rate`](Self::sample_rate) reports it at once. The
    /// ratios are the streamer's own to write, so the re-derivation happens on
    /// its thread when it takes the command this sends: at its next cycle (a
    /// hand-stepped streamer, at its next step). `&self`: the rate is one
    /// shared cell and the command a send, so a host holding the streamer
    /// behind a shared reference can call it.
    ///
    /// Waits while the streamer's bounded queue is full, as every stream
    /// command does; a streamer that is gone has no ratio to re-derive.
    ///
    /// Control thread only. Nothing changes for a disk voice already in the
    /// graph beyond its ratio: its placement gate converts beats to file frames
    /// at the file's own rate, which is not the session's.
    pub fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        self.shared.session_rate.set(sample_rate.into());
        // The rate is stored before the command is sent, so whichever the
        // streamer reads first (a stream it builds, or this) sees it. A gone
        // streamer has nothing to re-derive.
        let _ = self.streamer_tx.send(StreamerCommand::SessionRate);
    }

    /// The streamer's wave cache (tests).
    #[cfg(test)]
    pub(crate) fn cache(&self) -> std::sync::Arc<cache::LruCache<F>> {
        std::sync::Arc::clone(&self.shared.cache)
    }
}

// `driver` has its own `Drop` impl (a thread's stop); auto-drop handles cleanup.

/// Configuration for [`DiskStreamer::start`]. `Default` + struct-update.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiskStreamerConfig {
    /// Streamer buffer / cache configuration. Default is tuned for
    /// 64-channel streaming on a typical desktop.
    pub buffer_config: BufferConfig,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VoiceWindow;
    use crate::testing::loop_frames;
    use std::sync::Arc;

    /// A loop change on a streamed file reads only the frames it needs:
    /// its fade's lead-in and a short loop's body, through the stream's own
    /// decoder — never the whole file, which blocked every other stream while
    /// it decoded. Observed through
    /// the cache the whole-file read filled: a hard loop, a crossfaded one and
    /// a cleared one leave it empty, and the loop still plays (the ring holds
    /// the loop's frames past its end).
    #[test]
    fn a_loop_change_reads_only_the_frames_it_needs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loop.wav");
        write_tone(&path, 48_000, 2.0);
        let mut sampler =
            DiskStreamer::<_, f32>::manual(SampleRate::SR_48K, Default::default()).unwrap();
        let opening = sampler.open(&path, Placement::HEAD);
        let (stream, reader) =
            crate::testing::wait_for_open(&mut sampler, opening).expect("the file opens");
        for fade in [0usize, 500] {
            stream
                .looping_cell()
                .publish(Some(loop_frames(10_000, 20_000, fade)));
            let _ = sampler.step_until_idle(1_000);
        }
        let window = reader.ring.window();
        assert!(
            window.1 > 40_000,
            "the loop plays on past its end: {window:?}"
        );
        let key = Asset::from(path.as_path())
            .identity()
            .expect("a file has an identity");
        assert!(
            sampler.cache().get(&key).is_none(),
            "a loop change decoded the whole file"
        );
    }

    /// Write `seconds` of a stereo 440 Hz tone at `rate` to `path`.
    fn write_tone(path: &std::path::Path, rate: u32, seconds: f64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(f64::from(rate) * seconds) as usize {
            let s = (((std::f64::consts::TAU * 440.0 * i as f64 / rate as f64).sin()) as f32) * 0.4;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    /// A session-rate change re-rates every stream, open or opened later.
    /// A streamer built at 44.1 kHz streams a 48 kHz file (ratio 48/44.1);
    /// then the device restarts at 48 kHz (`set_sample_rate`):
    ///
    /// - the open stream's ratio is re-derived, to unity — it plays on at the
    ///   right pitch and speed rather than 8.8% sharp;
    /// - the streamer's `sample_rate` reports the new rate at once;
    /// - a disk voice taken after it still converts beats to file frames at
    ///   the file's 48 kHz (its seek targets stay where they were);
    /// - a 44.1 kHz file streamed after it gets 44.1/48, not unity.
    #[test]
    fn a_rate_change_re_derives_every_streams_ratio() {
        use tutti_units::{Beat, SrcRatio};

        let dir = tempfile::tempdir().unwrap();
        let (at_48, at_44) = (dir.path().join("48k.wav"), dir.path().join("44k.wav"));
        write_tone(&at_48, 48_000, 1.0);
        write_tone(&at_44, 44_100, 1.0);

        let mut sampler = DiskStreamer::manual(SampleRate::SR_44K1, Default::default()).unwrap();
        let stream = |sampler: &mut DiskStreamer<Manual<f32>, f32>, path: &std::path::Path| {
            let opening = sampler.open(path, Placement::HEAD);
            crate::testing::wait_for_open(sampler, opening).expect("the file opens")
        };

        let (_first_stream, first) = stream(&mut sampler, &at_48);
        let first_ratio = Arc::clone(&first.cells);
        assert_eq!(
            first_ratio.src_ratio(),
            SrcRatio::for_rates(SampleRate::SR_48K, SampleRate::SR_44K1)
        );
        assert!(
            first_ratio.src_ratio() != SrcRatio::UNITY,
            "setup: a converting stream"
        );

        sampler.set_sample_rate(SampleRate::SR_48K);
        assert_eq!(
            sampler.sample_rate(),
            SampleRate::SR_48K,
            "the rate is stored at once"
        );
        assert_eq!(
            first_ratio.src_ratio(),
            SrcRatio::for_rates(SampleRate::SR_48K, SampleRate::SR_44K1),
            "the ratio is the streamer's to write: it moves when the streamer takes the command"
        );
        let _ = sampler.step();
        assert_eq!(
            first_ratio.src_ratio(),
            SrcRatio::UNITY,
            "the open stream is re-rated"
        );

        let voice = crate::DiskVoice::from_reader(first, VoiceWindow::from(Beat::new(0.0)));
        assert_eq!(voice.file_sample_rate(), SampleRate::SR_48K);

        let (_second_stream, second) = stream(&mut sampler, &at_44);
        assert_eq!(
            second.cells.src_ratio(),
            SrcRatio::for_rates(SampleRate::SR_44K1, SampleRate::SR_48K)
        );
    }

    /// A dropped `Stream` ends its stream: the streamer frees its ring (and
    /// with it the writer's decoder and any resident wave) in the step that
    /// handles the drop, not before it. The reader is dropped first, so the
    /// streamer's writer is the ring's last holder.
    ///
    /// The region goes with the stream, and nothing but the `Drop` sends the
    /// close.
    #[test]
    fn a_dropped_stream_is_freed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_tone(&path, 48_000, 1.0);

        let mut sampler =
            DiskStreamer::<_, f32>::manual(SampleRate::SR_48K, Default::default()).unwrap();
        let opening = sampler.open(&path, Placement::HEAD);
        let (stream, reader) =
            crate::testing::wait_for_open(&mut sampler, opening).expect("the file opens");
        let ring = Arc::downgrade(&reader.ring);
        drop(reader);
        assert!(ring.upgrade().is_some(), "setup: the writer holds the ring");
        assert_eq!(sampler.driver.open_streams(), 1);

        drop(stream);
        assert!(
            ring.upgrade().is_some(),
            "dropping the handle only sends: the streamer frees on its own step"
        );
        let _ = sampler.step();
        assert!(
            ring.upgrade().is_none(),
            "the dropped stream's ring is still alive: its region leaked"
        );
        assert_eq!(sampler.driver.open_streams(), 0);
    }
}
