//! Streaming decode over symphonia.
//!
//! [`FileIn`] decodes an audio file sequentially, frame by frame, without
//! loading the whole file into RAM. It is a [`Source`]: [`pull`] fills a
//! caller's interleaved view of `S` **at the file's own channel width**
//! from the current cursor and reports how many **frames** it produced (a short
//! count, then [`Pull::Ended`] at end-of-stream, or [`Pull::Failed`] at a decode
//! fault). [`layout`](Source::layout) reports that
//! width, so a caller sizes its view from the trait and never has to ask the
//! concrete type.
//!
//! It is the read edge, the counterpart of [`FileOut`](crate::FileOut).
//!
//! **It does not downmix.** [`topology`](FileIn::topology) says which speaker
//! each channel feeds, and a caller wanting another width builds a
//! [`tutti_buffers::Fold`] from it, as every other engine edge does.
//!
//! To read from an
//! arbitrary position, call [`seek`] first — it hooks
//! `FormatReader::seek(SeekMode::Accurate, ...)` (which lands *before* the
//! requested frame) and decodes-and-discards the preroll to hit the exact frame,
//! so a following `pull` is a clean sequential read from there.
//!
//! [`pull`]: Source::pull
//! [`seek`]: FileIn::seek
//!
//! It shares [`read_wave`](crate::read_wave)'s probe head and its one-packet
//! helper. All decode/seek/file I/O runs on the streamer thread; the audio
//! thread never touches this type.

use symphonia::core::codecs::audio::well_known::CODEC_ID_FLAC;
use symphonia::core::errors::{Error, SeekErrorKind};
use symphonia::core::formats::{FormatReader, SeekMode, SeekTo};
use symphonia::core::units::Timestamp;
use tutti_buffers::{ChannelLayout, ChannelTopology, Placement};
use tutti_buffers::{Interleaved, InterleavedMut, Planes, Pull, Sample, Source, interleave};
use tutti_units::{SampleRate, Samples};

use super::error::convert;
use super::placement::topology_of;
use super::read::{PacketDecoder, first_audio_track, probe_asset, sample_rate_of, total_frames_of};
use crate::DecodeError;
use tutti_platform::Asset;

/// A frame count from the file's `u64`, saturating where `usize` is narrower.
fn frames(n: u64) -> Samples {
    Samples::new(usize::try_from(n).unwrap_or(usize::MAX))
}

/// What [`FileIn::error`] reports when the stream ends before the container's
/// frame count.
const TRUNCATED: &str = "stream ended before the container's frame count";

/// A streaming decoder over an audio file's first audio track, as an
/// [`Source`].
///
/// Decodes sequentially without loading the whole file into memory (compare
/// [`read_wave`](crate::read_wave)). [`pull`](Source::pull) fills an
/// interleaved view of `S` **at the file's own channel width** and
/// returns the frames produced; a short count and then [`Pull::Ended`] mark the
/// end of the file. It does not downmix: [`topology`](Self::topology) says which speaker each
/// channel feeds, and a caller wanting another width builds a
/// [`tutti_buffers::Fold`] from it.
///
/// [`seek`](Self::seek) positions it at an exact frame. Decoding, seeking and
/// file I/O all block, so use it from a loader or streaming thread, never the
/// audio thread. Sequential reads reuse their decode buffers across calls.
/// Requires a decode feature.
///
/// # Examples
///
/// ```no_run
/// use tutti_codec::FileIn;
/// use tutti_buffers::{InterleavedMut, Pull, Source};
/// use tutti_units::Samples;
///
/// let mut file = FileIn::<f32>::open("take.flac")?;
/// let width = usize::from(file.layout().count());
/// let mut buf = vec![0.0f32; 1024 * width];
/// let out = InterleavedMut::new(&mut buf, file.layout())?;
/// file.seek(Samples::new(48_000))?; // start one second in, at 48 kHz
/// if let Pull::Frames(frames) = file.pull(out) {
///     println!("read {} frames of {width} channels", frames.get());
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct FileIn<S: Sample = f32> {
    reader: Box<dyn FormatReader>,
    decoder: PacketDecoder<S>,
    track_id: u32,
    /// Which speaker each channel feeds; its length is the interleave stride.
    topology: ChannelTopology,
    /// Whether the container stated `topology` or only its channel count.
    placement: Placement,
    sample_rate: SampleRate,
    total_frames: Option<u64>,
    /// Decoded-but-unconsumed frames from the last packet, interleaved at the
    /// file's own width.
    leftover: Vec<S>,
    /// Read offset into `leftover` in frames; `[leftover_pos..]` is unconsumed.
    leftover_pos: usize,
    /// Next file sample-frame a sequential read produces.
    cursor: u64,
    /// Whether the container reports a frame count and supports accurate seek.
    seekable: bool,
    /// Timestamp of the first zero-frame packet the latest
    /// [`decode_next_packet`](Self::decode_next_packet) call skipped, if any —
    /// the decoder primer. [`seek`](Self::seek) steps back past it when the
    /// frames it swallowed include the one asked for.
    skipped_primer: Option<i64>,
    /// The container's timestamp of the first packet of the track, once one has
    /// been read: the earliest a seek can land.
    first_ts: Option<i64>,
    /// The container's timestamp of the first frame the stream produces, once
    /// one has: file frame 0. A packet's timestamp is that of its first decoded
    /// frame, so the frame a packet starts at is its timestamp less this.
    origin_ts: Option<i64>,
    /// The decode error that ended a [`pull`](Source::pull) stream
    /// early, kept for [`error`](Self::error). Sticky until a
    /// [`seek`](Self::seek) succeeds.
    fault: Option<DecodeError>,
    /// Whether `total_frames` is exact, so a stream that ends short of it is
    /// damaged. Only FLAC's is (its STREAMINFO count); MP3 delay and padding,
    /// Vorbis priming and a WAV header's data length can all disagree with
    /// the frames a file really holds.
    exact_length: bool,
}

impl<S: Sample> std::fmt::Debug for FileIn<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileIn")
            .field("track_id", &self.track_id)
            .field("topology", &self.topology)
            .field("sample_rate", &self.sample_rate)
            .field("total_frames", &self.total_frames)
            .field("leftover_frames", &self.leftover_frames())
            .field("leftover_pos", &self.leftover_pos)
            .field("cursor", &self.cursor)
            .field("seekable", &self.seekable)
            .field("skipped_primer", &self.skipped_primer)
            .field("origin_ts", &self.origin_ts)
            .field("faulted", &self.fault.is_some())
            .finish_non_exhaustive()
    }
}

impl<S: Sample> FileIn<S> {
    /// Returns the file's length in frames, if the container reports it.
    pub fn total_frames(&self) -> Option<Samples> {
        self.total_frames.map(frames)
    }

    /// Returns the file's sample rate, as the container states it; a file
    /// that states none is not opened ([`DecodeError::NoSampleRate`]).
    /// Nothing is resampled.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Which speaker each channel of the interleaved frames feeds, in channel
    /// order: what the container names, or [`ChannelTopology::default_for`] for
    /// the count it states (see [`read_wave`](crate::read_wave)). Its width is
    /// [`layout`](Source::layout)'s.
    pub fn topology(&self) -> &ChannelTopology {
        &self.topology
    }

    /// Whether [`topology`](Self::topology) was stated by the container or
    /// the default layout for a channel count.
    pub fn placement(&self) -> Placement {
        self.placement
    }

    /// Frames held in `leftover`, consumed or not.
    fn leftover_frames(&self) -> usize {
        self.topology
            .width()
            .from_interleaved_len(self.leftover.len())
            .get()
    }

    /// Returns whether [`seek`](Self::seek) can serve arbitrary positions:
    /// `true` when the container reports a frame count. When `false`, load the
    /// whole file with [`read_wave`](crate::read_wave) instead.
    pub fn seekable(&self) -> bool {
        self.seekable
    }

    /// Opens `src` (a path, or bytes in memory) at its first audio track,
    /// positioned at frame 0.
    ///
    /// Probes the container and creates a decoder; no audio is decoded yet.
    /// It reads the same track [`read_wave`](crate::read_wave) decodes. The
    /// reader the asset opens is owned for the life of the `FileIn`, and must
    /// seek (a seek is how the file's frames are found).
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] if the source cannot be opened, its container is not one
    /// this build reads, it has no audio track this build can decode, or the
    /// track states no sample rate ([`DecodeError::NoSampleRate`]).
    pub fn open(src: impl Into<Asset>) -> Result<Self, DecodeError> {
        Self::start(probe_asset(&src.into()).map_err(convert)?)
    }

    /// The decoder over a probed container's first audio track.
    fn start(reader: Box<dyn FormatReader>) -> Result<Self, DecodeError> {
        let (track, params) = first_audio_track(&*reader)?;
        let track_id = track.id;

        let total_frames = total_frames_of(track, params);
        let sample_rate = sample_rate_of(params)?;
        // At least one channel, so the interleave stride is never zero. Untested:
        // no container this build reads yields a zero-channel track, and a
        // fixture cannot be built or stubbed (symphonia's `Channels` come from
        // the demuxer).
        let (topology, placement) = topology_of(params.channels.as_ref());

        let decoder = PacketDecoder::<S>::new(params).map_err(convert)?;

        // Seekable only when the container reports a frame count. Formats
        // without n_frames (some VBR/streamed) fall back to whole-file load.
        let seekable = total_frames.is_some();
        let exact_length = params.codec == CODEC_ID_FLAC;

        // Reserve leftover for a typical packet's worth of frames so
        // sequential reads don't reallocate after warmup. WAV packets are
        // small (~a few thousand frames); FLAC blocks up to ~4608.
        let leftover = Vec::with_capacity(1 << 14);

        Ok(Self {
            reader,
            decoder,
            track_id,
            topology,
            placement,
            sample_rate,
            total_frames,
            leftover,
            leftover_pos: 0,
            cursor: 0,
            seekable,
            skipped_primer: None,
            first_ts: None,
            origin_ts: None,
            fault: None,
            exact_length,
        })
    }

    /// Unconsumed retained frames.
    #[inline]
    fn leftover_len(&self) -> usize {
        self.leftover_frames().saturating_sub(self.leftover_pos)
    }

    /// Refill `leftover` from the next packet of the selected track that
    /// decodes to audio. Returns the number of frames decoded, which is 0 only
    /// at EOF, and the container's timestamp (`packet.pts`, in frames) of the
    /// packet that produced them. Reuses the decoder's buffers to stay
    /// allocation-free after warmup.
    ///
    /// A packet can decode to **zero** frames without the stream ending —
    /// Vorbis's first packet, and its first after a `reset`, only prime the
    /// decoder's overlap — and both callers read a 0 as end-of-stream, so such
    /// a packet is skipped here rather than returned (returning it would make
    /// every Ogg file stream as empty). The skip is also why the timestamp comes back:
    /// the frames in `leftover` start at the returned packet's `ts`, not at
    /// the first packet read, and [`seek`](Self::seek) must count its preroll
    /// from there.
    fn decode_next_packet(&mut self) -> Result<(usize, i64), DecodeError> {
        self.skipped_primer = None;
        loop {
            let packet = match self.reader.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok((0, 0)),
                // A file cut short inside a packet ends there: the frames
                // before the cut are the file. FLAC's exact count
                // (see `truncation`) is what tells a cut from an end.
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok((0, 0));
                }
                Err(e) => return Err(convert(e)),
            };
            if packet.track_id != self.track_id {
                continue;
            }

            let ts = packet.pts.get();
            self.first_ts.get_or_insert(ts);
            let frames = self.decoder.decode(&packet).map_err(convert)?;
            if frames == 0 {
                self.skipped_primer.get_or_insert(ts);
                continue;
            }
            self.origin_ts.get_or_insert(ts);
            // A packet whose channel count disagrees with `open`'s is refused:
            // the read's stride is the file's width.
            let layout = self.topology.width();
            let packet_width = self.decoder.planes().len();
            if packet_width != usize::from(layout.count()) {
                return Err(DecodeError::ChannelsChanged {
                    file: usize::from(layout.count()),
                    packet: packet_width,
                });
            }
            let planes = self.decoder.planes();
            self.leftover.clear();
            self.leftover_pos = 0;
            self.leftover
                .resize(layout.interleaved_len(Samples::new(frames)), S::SILENCE);
            let planes = Planes::from_vecs(planes).expect("every plane holds the packet's frames");
            let out = InterleavedMut::new(&mut self.leftover, layout)
                .expect("the buffer holds whole frames");
            interleave(planes, out).expect("both are the file's width and the packet's frames");
            return Ok((frames, ts));
        }
    }

    /// The fault that cut a sequential read short, if one did: a packet that
    /// failed to decode, or (FLAC only, whose frame count is exact) a stream
    /// that ended before that count, where a damaged frame can read as a
    /// clean end of file.
    ///
    /// [`pull`](Source::pull) answers [`Pull::Failed`] after the frames decoded
    /// before the fault, where a finished file answers [`Pull::Ended`]; this is
    /// the fault's detail. The fault is sticky (later reads return it
    /// without decoding on) and cleared by a successful [`seek`](Self::seek).
    pub fn error(&self) -> Option<&DecodeError> {
        self.fault.as_ref()
    }

    /// Returns the frame the next sequential read will produce.
    pub fn cursor(&self) -> Samples {
        frames(self.cursor)
    }

    /// Fills `out` with the next sequential frames at the file's **own**
    /// channel width, interleaved, and returns how many frames were produced
    /// (fewer than fit only at the end of the file).
    ///
    /// This is the fallible form of [`pull`](Source::pull).
    ///
    /// # Errors
    ///
    /// [`DecodeError::Width`] if `out` is not the file's width. Otherwise a
    /// [`DecodeError`] if reading or decoding a packet fails. Frames filled
    /// before the failure are returned as `Ok` first, and the failure comes
    /// with the next call (and stays, see [`error`](Self::error)). The same
    /// goes for a FLAC stream that ends before its frame count.
    pub fn fill_sequential_interleaved(
        &mut self,
        out: InterleavedMut<'_, S>,
    ) -> Result<Samples, DecodeError> {
        if out.layout() != self.topology.width() {
            return Err(DecodeError::Width {
                file: self.topology.width(),
                view: out.layout(),
            });
        }
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }
        let (filled, fault) = self.fill(out);
        self.fault = fault;
        match &self.fault {
            // The frames before the fault are the caller's; the fault is
            // reported by the next call, which finds nothing more to read.
            Some(fault) if filled == 0 => Err(fault.clone()),
            _ => Ok(Samples::new(filled)),
        }
    }

    /// Whether the stream has ended short of the container's own frame count.
    ///
    /// A damaged frame can read as a clean end of file: the demuxer reports
    /// end-of-stream where it lost sync, so the only evidence is the count.
    fn truncation(&self) -> bool {
        self.exact_length && self.total_frames.is_some_and(|total| self.cursor < total)
    }

    /// [`fill_sequential_interleaved`](Self::fill_sequential_interleaved),
    /// keeping the frames filled before a decode error.
    fn fill(&mut self, mut out: InterleavedMut<'_, S>) -> (usize, Option<DecodeError>) {
        let layout = self.topology.width();
        let capacity_frames = out.frames().get();
        let mut filled = 0usize;

        while filled < capacity_frames {
            // Drain any retained remainder first (seek-free, alloc-free).
            if self.leftover_len() > 0 {
                let avail = self.leftover_len();
                let want = (capacity_frames - filled).min(avail);
                let held = Interleaved::new(&self.leftover, layout)
                    .expect("the buffer holds whole frames");
                let src = held
                    .slice(Samples::new(self.leftover_pos)..Samples::new(self.leftover_pos + want));
                let mut dst = out.slice_mut(Samples::new(filled)..Samples::new(filled + want));
                dst.samples_mut().copy_from_slice(src.samples());
                self.leftover_pos += want;
                filled += want;
                self.cursor += want as u64;
                continue;
            }

            // Need another packet.
            match self.decode_next_packet() {
                // End-of-stream: leave the untouched tail for the caller.
                Ok((0, _)) => {
                    let fault = self
                        .truncation()
                        .then_some(DecodeError::Malformed(TRUNCATED.into()));
                    return (filled, fault);
                }
                Ok(_) => {}
                Err(error) => return (filled, Some(error)),
            }
        }

        (filled, None)
    }

    /// Seeks the container to the packet holding `target`, or, when the
    /// container's seekable range ends before it (a container refuses a seek
    /// into the padding frames its decoder still returns), to the last
    /// timestamp it accepts, found by bisection from `first_ts`. Decoding on
    /// from there reaches `target`.
    fn seek_reader(&mut self, target: i64, first_ts: i64) -> Result<(), DecodeError> {
        let track_id = self.track_id;
        let reader = &mut self.reader;
        let mut seek = |ts: i64| {
            reader.seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts: Timestamp::new(ts),
                    track_id,
                },
            )
        };
        match seek(target) {
            Err(Error::SeekError(SeekErrorKind::OutOfRange)) if target > first_ts => {}
            other => return other.map(drop).map_err(convert),
        }
        let (mut accepted, mut refused) = (first_ts, target);
        while refused - accepted > 1 {
            let mid = accepted + (refused - accepted) / 2;
            match seek(mid) {
                Ok(_) => accepted = mid,
                Err(Error::SeekError(SeekErrorKind::OutOfRange)) => refused = mid,
                Err(error) => return Err(convert(error)),
            }
        }
        seek(accepted).map(drop).map_err(convert)
    }

    /// Positions the stream so the next read produces frame `start` exactly.
    ///
    /// Seeks the container to the packet holding `start`, then decodes and
    /// discards the frames before it. Seeking past the end leaves the stream
    /// at its end. Only meaningful when [`seekable`](Self::seekable).
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] if the container cannot seek or a packet fails to
    /// decode.
    pub fn seek(&mut self, start: Samples) -> Result<(), DecodeError> {
        let start = start.get() as u64;
        // File frame 0 is the first frame the stream produces, which the
        // container timestamps. A stream not read yet learns it from its first
        // packet.
        if self.origin_ts.is_none() {
            self.decode_next_packet()?;
        }
        let first_ts = self.first_ts.unwrap_or(0);
        let origin = self.origin_ts.unwrap_or(first_ts);
        let start_ts = origin.saturating_add(i64::try_from(start).unwrap_or(i64::MAX));
        // Accurate seek lands on the packet containing `target`; decoding then
        // discards the preroll up to `start_ts`. Two things make that subtler
        // than `start_ts - actual_ts`:
        //
        // 1. **The first packet after `reset` may decode to nothing.** A
        //    Vorbis decoder needs one packet to prime its overlap, and
        //    `decode_next_packet` skips it. The frames that come out then start
        //    at the *next* packet's timestamp, so the preroll is counted from
        //    the timestamp of the first packet that produced frames — never from
        //    the one the seek landed on; counting from that would land every Ogg
        //    seek about 1024 frames late, with no error.
        // 2. **That packet can start after `start_ts`** — or not exist, when the
        //    primer was the file's last packet. The primer was then the packet
        //    containing `start_ts`, and the frames there cannot be discarded
        //    back into existence. So the seek is retried from one frame before
        //    the primer, which makes the packet before it the primer instead.
        //    Each retry targets strictly earlier, and the track's first packet
        //    ends it.
        //
        // For codecs whose first packet decodes (PCM, FLAC, MP3 here) the
        // first attempt succeeds and this is one seek.
        let mut target = start_ts;
        loop {
            self.leftover.clear();
            self.leftover_pos = 0;
            self.seek_reader(target, first_ts)?;
            self.decoder.reset();

            let (decoded, ts) = self.decode_next_packet()?;
            if (decoded == 0 || ts > start_ts)
                && let Some(primer) = self
                    .skipped_primer
                    .filter(|&p| target > first_ts && p <= target)
            {
                target = primer.saturating_sub(1);
                continue;
            }
            if decoded == 0 {
                // Seeked past EOF; nothing to discard.
                break;
            }

            // The frames in `leftover` start at `ts`; discard up to `start_ts`.
            let mut discard = u64::try_from(start_ts.saturating_sub(ts)).unwrap_or(0);
            loop {
                let avail = self.leftover_len() as u64;
                let skip = avail.min(discard);
                self.leftover_pos += skip as usize;
                discard -= skip;
                if discard == 0 {
                    break;
                }
                if self.decode_next_packet()?.0 == 0 {
                    break; // EOF inside the preroll
                }
            }
            break;
        }

        self.cursor = start;
        self.fault = None;
        Ok(())
    }
}

/// Sequential reads at the file's **own** channel width. A decode error ends
/// the stream, is kept for [`FileIn::error`] and reads as
/// [`Pull::Failed`], where a clean end reads as [`Pull::Ended`]; use
/// [`fill_sequential_interleaved`](FileIn::fill_sequential_interleaved) to get
/// the error back as a `Result`.
///
/// No downmix happens here: `layout()` describes exactly what `pull`
/// produces. A caller wanting another width builds a [`tutti_buffers::Fold`] from
/// [`topology`](FileIn::topology).
///
/// A file is synchronous, so it never answers [`Pull::Starved`].
impl<S: Sample> Source for FileIn<S> {
    type Sample = S;

    /// The file's own width, which is what [`pull`](Self::pull) produces.
    fn layout(&self) -> ChannelLayout {
        self.topology.width()
    }

    /// `out` holds `out.len()` frames, and the count is that many FRAMES at
    /// most, never samples. A view of another width than
    /// [`layout`](Self::layout) is [`Failed`](Pull::Failed).
    ///
    /// Frames and a verdict never arrive together: the frames read before a
    /// decode fault come first, and the fault is the next pull's
    /// [`Failed`](Pull::Failed). A pull that finds nothing left and no fault is
    /// [`Ended`](Pull::Ended).
    fn pull(&mut self, out: InterleavedMut<'_, S>) -> Pull {
        if out.layout() != self.layout() {
            return Pull::Failed;
        }
        // Not through `fill_sequential_interleaved`, whose `Err` is a copy of
        // the fault: here it would be dropped.
        if self.fault.is_some() {
            return Pull::Failed;
        }
        let (filled, fault) = self.fill(out);
        self.fault = fault;
        if filled > 0 {
            Pull::Frames(Samples::new(filled))
        } else if self.fault.is_some() {
            Pull::Failed
        } else {
            Pull::Ended
        }
    }
}

#[cfg(all(test, feature = "decode-wav"))]
mod tests;
