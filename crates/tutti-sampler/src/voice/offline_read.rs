//! A disk voice's file, read on demand for an offline render.
//!
//! A fork of a [`DiskVoice`](super::disk_voice::DiskVoice) cannot read the
//! streamer's ring (one consumer, the live audio thread; a seek asked of the
//! streamer moves the live stream). It reads the file itself: the file the
//! streamer's record names ([`StreamFile`]), opened again through its asset and
//! decoded a page at a time on the render's own thread, or read in place when
//! the streamer's cache already holds it decoded.
//!
//! Offline has no deadline, so nothing prefetches: a second streamer stream
//! would put the render on the streamer's schedule (an underrun would be
//! silence written as a success). The export is a function of the file and the
//! timeline alone, and its resources (a decoder and two pages) drop with the
//! fork, or at [`OfflineRead::close`] once the voice is past its file.
//!
//! A file that cannot be opened, sought or decoded renders silence from that
//! point, and the first such failure is latched ([`FirstError`]); the fork
//! hands the latch to the graph as its `ForkHealth`, and the export fails
//! naming the node and the asset.
//!
//! A position is read exactly as `MemorySource` reads one: the four taps
//! [`tap_indices`] names (a loop's, `LoopSpan::taps`, on a loop), through
//! [`interpolate_taps`], silent at and past the end, mirrored the same way in
//! reverse. A clip exported from disk is the samples the same clip in memory
//! renders, bit for bit.

use std::sync::Arc;

use super::fault::FirstError;
use tutti_buffers::ChannelLayout;
#[cfg(codec)]
use tutti_buffers::Source;
use tutti_buffers::{Float, Wave};
use tutti_platform::Asset;
use tutti_units::{SamplePosition, Samples};

use super::Loop;
use super::interp::{
    MAX_WAVE_CHANNELS, Place, interpolate_taps, mirrored, read_frame, read_looped_frame,
    tap_indices,
};
use super::loop_span::LoopSpan;
use super::types::PlayDirection;
use crate::streamer::StreamFile;

// A read is placed by the `Place` the voice built from its file's topology
// (`DiskVoice::new`), the same one its live read uses: the caller
// passes it, so a fork plays the channels where the live voice does.

/// Frames per page. Two pages are resident; see [`Pages`].
const PAGE_FRAMES: usize = 1 << 15;

/// How far a page reaches past the frame that missed it, on the side the read
/// came from, so the taps around that frame (one behind, two ahead) are in the
/// same page.
const PAGE_LEAD: usize = 4;

/// Why an offline disk voice renders silence where its file should be: the
/// file, and what went wrong with it.
#[derive(Debug, thiserror::Error)]
#[error("cannot read {name}: {what}")]
pub(crate) struct OfflineReadError {
    /// The asset's [`name`](Asset::name).
    name: String,
    #[source]
    what: ReadFault,
}

/// What went wrong reading a file for an offline voice.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadFault {
    #[cfg(not(codec))]
    #[error("no audio codec feature is enabled")]
    NoCodec,
    #[cfg(codec)]
    #[error("seek to frame {frame}: {source}")]
    Seek {
        frame: usize,
        source: tutti_codec::DecodeError,
    },
    #[cfg(codec)]
    #[error("decode at frame {frame}: {message}")]
    Decode {
        frame: usize,
        // The decoder keeps its error and lends it out, so only its text is ours.
        message: String,
    },
    #[cfg(codec)]
    #[error("page at frame {frame}: {message}")]
    Page { frame: usize, message: String },
    #[cfg(codec)]
    #[error(transparent)]
    Open(#[from] tutti_codec::DecodeError),
}

/// The file a forked disk voice plays: the streamer's description of it, and
/// the file itself, opened on the first read.
pub(crate) struct OfflineRead<F: Float> {
    /// What to play. Shared by clones: it is a description, never written.
    file: Arc<StreamFile<F>>,
    /// The loop the stream ran when the fork was taken, as the streamer
    /// applied it (`RingControl`); `Off` for none.
    loop_: Option<Loop>,
    /// The open file. `None` until the first read and after a
    /// [`close`](Self::close); never cloned: a clone opens its own, so no two
    /// renders share a decoder's cursor.
    open: Option<Open<F>>,
    /// The file's length in frames, once it has been opened: what lets a
    /// closed reader know a position past the end without opening again.
    len: Option<usize>,
    /// Where a failure goes. Shared with the fork's probe.
    fault: Arc<FirstError>,
    /// Opening failed once. Kept so a missing file costs one attempt, not one
    /// per frame.
    failed: bool,
}

impl<F: Float> Clone for OfflineRead<F> {
    fn clone(&self) -> Self {
        Self {
            file: Arc::clone(&self.file),
            loop_: self.loop_,
            open: None,
            len: self.len,
            fault: Arc::clone(&self.fault),
            failed: false,
        }
    }
}

impl<F: Float> std::fmt::Debug for OfflineRead<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineRead")
            .field("file", &self.file)
            .field("open", &self.open.is_some())
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

impl<F: Float> OfflineRead<F> {
    /// A reader for `file` looped as `loop_`, latching its failures into
    /// `fault`. Opens nothing yet.
    pub(crate) fn new(
        file: Arc<StreamFile<F>>,
        loop_: Option<Loop>,
        fault: Arc<FirstError>,
    ) -> Self {
        Self {
            file,
            loop_,
            open: None,
            len: None,
            fault,
            failed: false,
        }
    }

    /// The loop this reader plays (tests).
    #[cfg(test)]
    pub(crate) fn loop_setting(&self) -> Option<Loop> {
        self.loop_
    }

    /// This reader, closed, latching into `fault` from now on: what a fork
    /// of a fork keeps, with the new fork's latch.
    pub(crate) fn relatched(mut self, fault: Arc<FirstError>) -> Self {
        self.open = None;
        self.failed = false;
        self.fault = fault;
        self
    }

    /// Read file position `pos` (fractional file frames), gain-free, into
    /// `out`, writing every element.
    ///
    /// Forward, on a loop, the position is placed on it and read as
    /// `LoopSpan` has a loop (the fade leads into the loop's start; taps near
    /// the end wrap through it), as `MemorySource::read_placed_into` reads the
    /// same loop. Reverse mirrors the position about the file's last frame, as
    /// the memory tier does, and ignores the loop, as the streamer's reverse
    /// refill does. Silence where the file has nothing to play — forward at or
    /// past the end, reverse at a position whose mirror is before the first
    /// frame (`pos >= len`, as forward) — and the file is closed there (there
    /// is nothing more to read in that direction).
    ///
    /// Blocks on file I/O when the position leaves the resident pages. That
    /// is the point of this type, and why it is only ever built for an
    /// offline render.
    pub(crate) fn read_into(
        &mut self,
        pos: SamplePosition,
        direction: PlayDirection,
        place: &Place<F>,
        out: &mut [F],
    ) {
        // A position that is not a number is nowhere in the file.
        if !pos.get().is_finite() {
            out.fill(F::ZERO);
            return;
        }
        // Past what this direction can play, known without opening the file.
        let played_out = match direction {
            PlayDirection::Reverse => true,
            PlayDirection::Forward => self.loop_.is_none(),
        };
        if played_out
            && let Some(len) = self.len
            && pos.get() >= len as f64
        {
            self.close();
            out.fill(F::ZERO);
            return;
        }
        let setting = self.loop_;
        let Some(open) = self.open() else {
            out.fill(F::ZERO);
            return;
        };
        let span = LoopSpan::from_setting(setting, open.len());
        let len = open.len() as f64;
        match (direction, span) {
            (PlayDirection::Reverse, _) => {
                if pos.get() >= len {
                    out.fill(F::ZERO);
                    return;
                }
                open.read_into(mirrored(Samples::new(open.len()), pos), place, out);
            }
            (PlayDirection::Forward, Some(span)) => {
                let (p, looped) = span.place(pos.get());
                open.read_looped_into(&span, SamplePosition::new(p), looped, place, out);
            }
            (PlayDirection::Forward, None) => open.read_into(pos, place, out),
        }
    }

    /// Close the file: drop the decoder and its pages. The next read opens it
    /// again through its asset. A voice closes it once the playhead has left its
    /// window for good, so a render holding many voices keeps one file open
    /// per voice sounding, not per voice in the graph.
    pub(crate) fn close(&mut self) {
        self.open = None;
    }

    /// Whether the file is open.
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Pages decoded since the file was opened.
    #[cfg(test)]
    pub(crate) fn page_loads(&self) -> usize {
        match &self.open {
            Some(Open::Paged(pages)) => pages.loads,
            _ => 0,
        }
    }

    /// The open file, opening it on first use. `None` when it cannot be opened
    /// (moved, deleted, no codec for it): the voice renders silence, and the
    /// failure is latched.
    fn open(&mut self) -> Option<&mut Open<F>> {
        if self.open.is_none() && !self.failed {
            match Open::from_file(&self.file, Arc::clone(&self.fault)) {
                Ok(open) => {
                    self.len = Some(open.len());
                    self.open = Some(open);
                }
                Err(what) => {
                    self.failed = true;
                    self.fault.latch(OfflineReadError {
                        name: self.file.asset().name(),
                        what,
                    });
                }
            }
        }
        self.open.as_mut()
    }
}

/// An opened file: decoded whole already (the streamer's cached wave, or a
/// file that cannot seek), or paged in through a decoder.
enum Open<F: Float> {
    Resident(Arc<Wave<F>>),
    /// Boxed: a decoder and two pages' bookkeeping, against an `Arc`. With
    /// no codec compiled in nothing builds one (`open_path` fails first).
    #[cfg_attr(not(codec), allow(dead_code))]
    Paged(Box<Pages<F>>),
}

impl<F: Float> Open<F> {
    /// Open `file`: the streamer's decoded copy when its cache holds one, else
    /// the asset opened again — header only when it can seek, a whole decode
    /// when it cannot.
    fn from_file(file: &StreamFile<F>, fault: Arc<FirstError>) -> Result<Self, ReadFault> {
        if let Some(wave) = file.resident() {
            return Ok(Self::Resident(Arc::clone(wave)));
        }
        Self::open_path(file, fault)
    }

    #[cfg(codec)]
    fn open_path(file: &StreamFile<F>, fault: Arc<FirstError>) -> Result<Self, ReadFault> {
        let decoder = tutti_codec::FileIn::<F>::open(file.asset().clone())?;
        let decoder_layout = decoder.layout();
        let channels = usize::from(decoder_layout.count());
        match decoder.total_frames().filter(|_| decoder.seekable()) {
            Some(len) => Ok(Self::Paged(Box::new(Pages {
                decoder,
                asset: file.asset().clone(),
                fault,
                channels,
                layout: decoder_layout,
                len: len.get(),
                slots: Default::default(),
                recent: 0,
                loads: 0,
            }))),
            // Decoded once, and read in place as a memory voice reads its
            // wave: no second, interleaved copy.
            None => {
                drop(decoder);
                tutti_codec::read_wave::<F>(file.asset().clone())
                    .map(|wave| Self::Resident(Arc::new(wave)))
                    .map_err(ReadFault::from)
            }
        }
    }

    /// No codec compiled in: nothing can be decoded, here or by the streamer.
    #[cfg(not(codec))]
    fn open_path(_file: &StreamFile<F>, _fault: Arc<FirstError>) -> Result<Self, ReadFault> {
        Err(ReadFault::NoCodec)
    }

    fn len(&self) -> usize {
        match self {
            Self::Resident(wave) => wave.len().get(),
            Self::Paged(pages) => pages.len,
        }
    }

    /// Interpolate position `p` into `out` as `read_frame` would from the
    /// whole file resident: silence at and past the end.
    fn read_into(&mut self, p: SamplePosition, place: &Place<F>, out: &mut [F]) {
        if p >= SamplePosition::new(0.0).advanced_by(Samples::new(self.len())) {
            out.fill(F::ZERO);
            return;
        }
        match self {
            // The memory tier's own read, on the same kind of wave.
            Self::Resident(wave) => read_frame(wave, place, p, out),
            Self::Paged(pages) => pages.read_into(p, place, out),
        }
    }

    /// Interpolate position `p`, placed on `span` (`looped` once round it),
    /// into `out` as `read_looped_frame` would from the whole file resident:
    /// silence at and past the end.
    fn read_looped_into(
        &mut self,
        span: &LoopSpan,
        p: SamplePosition,
        looped: bool,
        place: &Place<F>,
        out: &mut [F],
    ) {
        if p >= SamplePosition::new(0.0).advanced_by(Samples::new(self.len())) {
            out.fill(F::ZERO);
            return;
        }
        match self {
            // The memory tier's own looped read, on the same kind of wave.
            Self::Resident(wave) => read_looped_frame(wave, place, span, p, looped, out),
            Self::Paged(pages) => pages.read_looped_into(span, p, looped, place, out),
        }
    }
}

/// A file, decoded into two resident pages of [`PAGE_FRAMES`] as it is read.
///
/// Two, not one, because two places are read near each other in time: the
/// loop seam (the taps before a loop's end and after its start) and a loop
/// crossfade (the tail and the head at once). With one page each would evict
/// the other every frame.
///
/// A page is laid out ahead of the read: after the frame that missed going
/// forward, before it going backward ([`load`](Self::load)), so a reversed
/// read decodes a page per page of frames, not one per few frames.
// With no codec compiled in nothing builds one (`open_path` fails first).
#[cfg_attr(not(codec), allow(dead_code))]
struct Pages<F: Float> {
    #[cfg(codec)]
    decoder: tutti_codec::FileIn<F>,
    asset: Asset,
    fault: Arc<FirstError>,
    /// The file's width. Every page is interleaved at it.
    channels: usize,
    /// `channels` as the layout a page's frames are counted in.
    layout: ChannelLayout,
    /// The file's length in frames.
    len: usize,
    slots: [Page<F>; 2],
    /// The slot read last; a miss replaces the other one.
    recent: usize,
    /// Pages decoded so far. What a test bounds the paging by.
    loads: usize,
}

/// A run of decoded frames, `data` interleaved at the file's width.
#[derive(Default)]
struct Page<F> {
    start: usize,
    frames: usize,
    data: Vec<F>,
}

impl<F> Page<F> {
    fn contains(&self, frame: usize) -> bool {
        frame >= self.start && frame < self.start + self.frames
    }
}

impl<F: Float> Pages<F> {
    /// Interpolate position `p` (`< len`) into `out`.
    fn read_into(&mut self, p: SamplePosition, place: &Place<F>, out: &mut [F]) {
        // The file's width: the placement reads all of it (a file wider than
        // a frame is folded, or refused when the voice is built).
        let ch = self.channels.min(MAX_WAVE_CHANNELS);
        let (taps, frac) = tap_indices(self.len, p);
        let mut frames = [[F::ZERO; MAX_WAVE_CHANNELS]; 4];
        for (frame, &at) in frames.iter_mut().zip(taps.iter()) {
            frame[..ch].copy_from_slice(&self.frame(at)[..ch]);
        }
        interpolate_taps(ch, place, frac, out, |c, t| frames[t][c]);
    }

    /// Interpolate position `p` (`< len`), placed on `span`, into `out`: the
    /// loop's four taps, each blended toward its lead-in inside the fade, as
    /// `read_looped_frame` blends them.
    fn read_looped_into(
        &mut self,
        span: &LoopSpan,
        p: SamplePosition,
        looped: bool,
        place: &Place<F>,
        out: &mut [F],
    ) {
        let ch = self.channels.min(MAX_WAVE_CHANNELS);
        let (taps, frac) = span.taps(self.len, p, looped);
        let mut frames = [[F::ZERO; MAX_WAVE_CHANNELS]; 4];
        for (frame, tap) in frames.iter_mut().zip(taps.iter()) {
            frame[..ch].copy_from_slice(&self.frame(tap.frame)[..ch]);
            if let Some((lead, out, inn)) = tap.fade {
                let lead = self.frame(lead);
                for (s, &l) in frame[..ch].iter_mut().zip(lead.iter()) {
                    *s = *s * out + l * inn;
                }
            }
        }
        interpolate_taps(ch, place, frac, out, |c, t| frames[t][c]);
    }

    /// Frame `at` (`< len`), interleaved at the file's width, paging it in if
    /// neither resident page holds it.
    fn frame(&mut self, at: usize) -> &[F] {
        let slot = match self.slots.iter().position(|page| page.contains(at)) {
            Some(slot) => slot,
            None => {
                // Behind the page read last: the read is going backward.
                let recent = &self.slots[self.recent];
                let backward = recent.frames > 0 && at < recent.start;
                let slot = 1 - self.recent;
                self.load(slot, at, backward);
                slot
            }
        };
        self.recent = slot;
        let ch = self.channels;
        let page = &self.slots[slot];
        let from = (at - page.start) * ch;
        &page.data[from..from + ch]
    }

    /// Decode the page around `at` into `slot`: `[at - LEAD, at - LEAD +
    /// PAGE)` going forward, `(at + LEAD - PAGE, at + LEAD]` going backward,
    /// clamped to the file. A seek or decode that fails leaves the page
    /// silent and latches the failure; a decode that comes up short (a
    /// truncated file) leaves the rest silent, as the streamer's refill does.
    fn load(&mut self, slot: usize, at: usize, backward: bool) {
        let start = if backward {
            (at + PAGE_LEAD + 1).saturating_sub(PAGE_FRAMES)
        } else {
            at.saturating_sub(PAGE_LEAD)
        };
        let frames = PAGE_FRAMES.min(self.len - start);
        self.loads += 1;
        let page = &mut self.slots[slot];
        page.start = start;
        page.frames = frames;
        page.data.clear();
        page.data
            .resize(self.layout.interleaved_len(Samples::new(frames)), F::ZERO);
        #[cfg(codec)]
        {
            let fail = |what: ReadFault| OfflineReadError {
                name: self.asset.name(),
                what,
            };
            if self.decoder.cursor() != Samples::new(start)
                && let Err(source) = self.decoder.seek(Samples::new(start))
            {
                self.fault.latch(fail(ReadFault::Seek {
                    frame: start,
                    source,
                }));
                return;
            }
            // Frames read before a fault stay in the page.
            let view = match tutti_buffers::InterleavedMut::new(&mut page.data, self.layout) {
                Ok(view) => view,
                Err(e) => {
                    self.fault.latch(fail(ReadFault::Page {
                        frame: start,
                        message: e.to_string(),
                    }));
                    return;
                }
            };
            let _ = self.decoder.fill_sequential_interleaved(view);
            if let Some(e) = self.decoder.error() {
                self.fault.latch(fail(ReadFault::Decode {
                    frame: start,
                    message: e.to_string(),
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::loop_frames;
    use tutti_units::SampleRate;

    const LEN: usize = 100_000;

    fn value(i: usize) -> f32 {
        (i as f32 + 1.0) * 1e-5
    }

    /// A mono f32 ramp of [`LEN`] frames, three pages long.
    fn ramp_file(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("ramp.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&path, spec).expect("writes");
        for i in 0..LEN {
            w.write_sample(value(i)).expect("writes");
        }
        w.finalize().expect("writes");
        path
    }

    fn reader(path: std::path::PathBuf) -> OfflineRead<f32> {
        OfflineRead::new(
            Arc::new(StreamFile::new(path, SampleRate::SR_48K, None)),
            None,
            Arc::default(),
        )
    }

    /// A reversed read decodes a page per page of frames, as a forward
    /// one does: reading the whole file backwards loads about `LEN / PAGE`
    /// pages, not one every few frames (a page laid out ahead of a forward
    /// read leaves a backward one below its start almost at once). Counted,
    /// not timed. Both directions read the right frames.
    #[test]
    fn a_reversed_read_decodes_a_page_per_page_of_frames() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = ramp_file(dir.path());
        let bound = LEN / (PAGE_FRAMES - 2 * PAGE_LEAD) + 2;
        for direction in [PlayDirection::Forward, PlayDirection::Reverse] {
            let mut read = reader(path.clone());
            let mut out = [0.0f32; 1];
            for k in 0..LEN {
                read.read_into(
                    SamplePosition::new(k as f64),
                    direction,
                    &Place::FAN,
                    &mut out,
                );
                let want = match direction {
                    PlayDirection::Forward => value(k),
                    PlayDirection::Reverse => value(LEN - 1 - k),
                };
                assert_eq!(out[0], want, "{direction:?}: frame {k}");
            }
            assert!(
                read.page_loads() <= bound,
                "{direction:?}: {} page loads for {LEN} frames (at most {bound})",
                read.page_loads()
            );
        }
    }

    /// The taps wrap through the loop (the disk fork): on a
    /// hard loop `[10, 20)`, half a frame before the end interpolates frames
    /// 18, 19, then 10, 11 — what the loop plays next, and what the streamer's
    /// ring holds there — not 20, 21 from past it; half a frame into a later
    /// pass the frame behind is 19; the first pass reaches 10 from 9. Paged,
    /// through the loop's own tap layout, as the memory tier reads it.
    #[test]
    fn a_loops_taps_wrap_through_its_seam() {
        use tutti_dsp::cubic_hermite;
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut read = OfflineRead::new(
            Arc::new(StreamFile::new(
                ramp_file(dir.path()),
                SampleRate::SR_48K,
                None,
            )),
            Some(loop_frames(10, 20, 0)),
            Arc::default(),
        );
        let mut out = [0.0f32; 1];
        let mut at = |pos: f64| {
            read.read_into(
                SamplePosition::new(pos),
                PlayDirection::Forward,
                &Place::FAN,
                &mut out,
            );
            out[0]
        };
        let v = value;
        assert_eq!(at(19.5), cubic_hermite(v(18), v(19), v(10), v(11), 0.5));
        assert_eq!(at(30.5), cubic_hermite(v(19), v(10), v(11), v(12), 0.5));
        assert_eq!(at(10.5), cubic_hermite(v(9), v(10), v(11), v(12), 0.5));
        assert!(read.page_loads() > 0, "read through the pages");
    }

    /// A forward read past the end of an unlooped file closes it: there
    /// is nothing more to read, so the render stops holding the file, and a
    /// later read past the end does not open it again.
    #[test]
    fn a_read_past_the_end_closes_the_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut read = reader(ramp_file(dir.path()));
        let mut out = [0.0f32; 1];
        read.read_into(
            SamplePosition::new(10.0),
            PlayDirection::Forward,
            &Place::FAN,
            &mut out,
        );
        assert!(read.is_open());
        read.read_into(
            SamplePosition::new((LEN + 10) as f64),
            PlayDirection::Forward,
            &Place::FAN,
            &mut out,
        );
        assert_eq!(out[0], 0.0);
        assert!(!read.is_open(), "the file is still open past its end");
        read.read_into(
            SamplePosition::new((LEN + 20) as f64),
            PlayDirection::Forward,
            &Place::FAN,
            &mut out,
        );
        assert!(!read.is_open(), "a read past the end opened it again");
    }

    /// `OfflineRead::read_into` latches a damaged file's fault. The FLAC
    /// fixture cut to 90% ends short of its frame count; a read before the
    /// cut returns frames, and the `FirstError` holds the decode failure.
    /// Called on the reader directly, not through a voice render.
    #[test]
    fn a_damaged_file_latches_a_fault_and_keeps_its_frames() {
        let source = tutti_fixtures::asset("audio/stereo.flac");
        let bytes = std::fs::read(source).expect("fixture");
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("cut.flac");
        std::fs::write(&path, &bytes[..bytes.len() * 9 / 10]).expect("write");

        let fault = Arc::<FirstError>::default();
        let mut read = OfflineRead::new(
            Arc::new(StreamFile::new(path, SampleRate::SR_44K1, None)),
            None,
            Arc::clone(&fault),
        );
        let mut out = [0.0f32; 2];
        read.read_into(
            SamplePosition::new(100.0),
            PlayDirection::Forward,
            &Place::FAN,
            &mut out,
        );
        assert!(
            out.iter().any(|&s| s != 0.0),
            "frames before the cut are kept"
        );
        let latched = fault.fault().expect("the cut is latched").to_string();
        assert!(latched.contains("frame count"), "{latched}");
    }

    /// A position that is NaN or infinite reads silence, in both directions
    /// and on a loop: it was NaN (the fraction of a NaN position) or, on a
    /// loop, a wrapped read of garbage frames.
    #[test]
    fn a_non_finite_position_reads_silence() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = ramp_file(dir.path());
        let on = Some(loop_frames(10, 20, 0));
        for loop_ in [None, on] {
            for direction in [PlayDirection::Forward, PlayDirection::Reverse] {
                for pos in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                    let mut read = OfflineRead::new(
                        Arc::new(StreamFile::new(path.clone(), SampleRate::SR_48K, None)),
                        loop_,
                        Arc::default(),
                    );
                    let mut out = [1.0f32; 2];
                    read.read_into(SamplePosition::new(pos), direction, &Place::FAN, &mut out);
                    assert_eq!(out, [0.0; 2], "{loop_:?} {direction:?} at {pos}");
                }
            }
        }
    }

    /// A stereo WAV of `frames` frames at 48 kHz in `spec`'s format, frame `i`
    /// holding `sample(i)` (left, right).
    fn stereo_file<T: hound::Sample + Copy>(
        dir: &std::path::Path,
        spec: hound::WavSpec,
        frames: usize,
        sample: impl Fn(usize) -> (T, T),
    ) -> std::path::PathBuf {
        let path = dir.join("stereo.wav");
        let mut w = hound::WavWriter::create(&path, spec).expect("writes");
        for i in 0..frames {
            let (l, r) = sample(i);
            w.write_sample(l).expect("writes");
            w.write_sample(r).expect("writes");
        }
        w.finalize().expect("writes");
        path
    }

    /// Every frame of a file of more than a page reads back as the value the
    /// file holds, in a graph of format `F`: an I24 file holds frame `i` as the
    /// integer `i - 500` (left) and `-i` (right), which is that over 2^23 as a
    /// float, exactly, so the pages are exact in either float format.
    fn assert_i24_pages_exact<F: Float>() {
        const FRAMES: usize = 3 * PAGE_FRAMES / 2;
        const SCALE: f64 = 8_388_608.0;
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = stereo_file(dir.path(), spec, FRAMES, |i| (i as i32 - 500, -(i as i32)));
        let mut read = OfflineRead::<F>::new(
            Arc::new(StreamFile::new(path, SampleRate::SR_48K, None)),
            None,
            Arc::default(),
        );
        let mut out = [F::ZERO; 2];
        for k in (0..FRAMES).step_by(7).chain([FRAMES - 1]) {
            read.read_into(
                SamplePosition::new(k as f64),
                PlayDirection::Forward,
                &Place::FAN,
                &mut out,
            );
            let want = [(k as f64 - 500.0) / SCALE, -(k as f64) / SCALE];
            assert_eq!(
                [out[0].to_f64(), out[1].to_f64()],
                want,
                "frame {k} of an I24 file"
            );
        }
        assert!(read.page_loads() >= 2, "the file spans more than one page");
    }

    #[test]
    fn offline_pages_of_an_i24_file_read_back_the_exact_values_in_f32() {
        assert_i24_pages_exact::<f32>();
    }

    #[test]
    fn offline_pages_of_an_i24_file_read_back_the_exact_values_in_f64() {
        assert_i24_pages_exact::<f64>();
    }

    /// A graph of `f64` reading an `f32` file: the frames arrive converted,
    /// the left and right where the file has them (they differ), both when
    /// the file is paged and when the streamer holds it resident.
    #[test]
    fn an_f64_read_of_an_f32_file_keeps_each_channel_where_the_file_has_it() {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = stereo_file(dir.path(), spec, 16, |i| (i as f32 / 16.0, -0.25f32));
        let resident = Arc::new(tutti_codec::read_wave::<f64>(&path).expect("decodes"));
        for wave in [None, Some(resident)] {
            let mut read = OfflineRead::<f64>::new(
                Arc::new(StreamFile::new(path.clone(), SampleRate::SR_48K, wave)),
                None,
                Arc::default(),
            );
            let mut out = [0.0f64; 2];
            for k in 0..16 {
                read.read_into(
                    SamplePosition::new(k as f64),
                    PlayDirection::Forward,
                    &Place::FAN,
                    &mut out,
                );
                assert_eq!(out, [k as f64 / 16.0, -0.25], "frame {k}");
            }
        }
    }
}
