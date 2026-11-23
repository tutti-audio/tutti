//! Decoding a file into a [`Wave`], and reading its header without decoding.
//!
//! The entry points are [`read_wave`] and [`read_wave_metadata`], each over an
//! [`Asset`]. Decoded samples are held to the generator of the committed fixtures
//! by `tests/decode_fixtures.rs`.

use std::any::Any;
use std::io::{Read, Seek, SeekFrom};
use tutti_buffers::{I24, Planes, Sample, SampleFormat, Wave};
use tutti_platform::Asset;
use tutti_rt::Exclusive;
use tutti_units::{SampleRate, Samples};

use symphonia::core::audio::sample::{SampleFormat as FileFormat, i24};
use symphonia::core::audio::{AudioSpec, GenericAudioBufferRef};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_PCM_ALAW, CODEC_ID_PCM_F32BE, CODEC_ID_PCM_F32LE, CODEC_ID_PCM_F64BE,
    CODEC_ID_PCM_F64LE, CODEC_ID_PCM_MULAW, CODEC_ID_VORBIS,
};
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, CODEC_ID_NULL_AUDIO,
};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, Track};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::packet::Packet;

use super::error::convert;
use super::placement::{empty_wave, topology_of};
use super::riff_guard;
use crate::DecodeError;
use tutti_buffers::{ChannelLayout, ChannelTopology, Placement};

/// Container metadata read from an audio file without decoding any audio.
///
/// Returned by [`read_wave_metadata`]. The fields are the container's raw
/// answers (the rate it states), and the topology it names, the one a full read would
/// give the [`Wave`]. It carries a [`ChannelTopology`], which owns its list of
/// speakers, so this is `Clone` and not `Copy`.
#[derive(Debug, Clone, PartialEq)]
pub struct WaveMetadata {
    /// Total sample frames, if the container reports it (`None` for some
    /// streamed/VBR formats that don't store a frame count).
    pub total_frames: Option<u64>,
    /// The file's sample rate. A container that states none, or 0, is
    /// [`DecodeError::NoSampleRate`].
    pub sample_rate: SampleRate,
    /// Which speaker each channel feeds, in the order the samples are in (as
    /// [`Wave::topology`] and [`FileIn::topology`](crate::FileIn::topology)
    /// give it): a lone channel is the centre, a speaker the vocabulary has no
    /// name for is [`Speaker::Unknown`](tutti_buffers::Speaker::Unknown), and a
    /// container that states a count and no placement is
    /// [`ChannelTopology::default_for`] ([`placement`](Self::placement)). A caller deciding whether a file
    /// can be placed asks this before it loads or opens the file.
    pub topology: Option<ChannelTopology>,
    /// The file's width, which its [`topology`](Self::topology) has when it is at
    /// most [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) channels.
    pub layout: ChannelLayout,
    /// Whether the container stated [`topology`](Self::topology) or only a
    /// channel count it took the default layout for.
    pub placement: Placement,
    /// The format the file's samples are held in: what [`read_wave`] and
    /// [`FileIn`](crate::FileIn) give back exactly, with no conversion, when
    /// asked for it. A caller picks its sample type from this once, at open.
    /// A lossy codec decodes to floats, so it is [`SampleFormat::F32`].
    pub format: SampleFormat,
}

/// The format a track's decoded samples are held in: a float PCM codec's own
/// width, an integer codec's smallest format holding its bits, a G.711
/// companding codec's, which expands its 8 bits to 16-bit integers, and a lossy
/// codec's, which has no integer to keep, `f32`.
fn native_format(params: &AudioCodecParameters) -> SampleFormat {
    let float = |wide: bool| {
        if wide {
            SampleFormat::F64
        } else {
            SampleFormat::F32
        }
    };
    match (params.codec, params.sample_format, params.bits_per_sample) {
        (CODEC_ID_PCM_F32LE | CODEC_ID_PCM_F32BE, ..) => float(false),
        (CODEC_ID_PCM_F64LE | CODEC_ID_PCM_F64BE, ..) => float(true),
        (CODEC_ID_PCM_ALAW | CODEC_ID_PCM_MULAW, ..) => SampleFormat::I16,
        (_, Some(FileFormat::F32), _) => float(false),
        (_, Some(FileFormat::F64), _) => float(true),
        (_, _, Some(..=8)) => SampleFormat::I8,
        (_, _, Some(9..=16)) => SampleFormat::I16,
        (_, _, Some(17..=24)) => SampleFormat::I24,
        (_, _, Some(_)) => SampleFormat::I32,
        (_, _, None) => SampleFormat::F32,
    }
}

impl WaveMetadata {
    /// The file's channel count: the width of its [`topology`](Self::topology).
    #[must_use]
    pub fn channels(&self) -> usize {
        usize::from(self.layout.count())
    }
}

/// A decoder for one audio track, producing planar `S` a packet at a time.
///
/// Gapless trimming is off, so it hands back every frame its codec decodes,
/// delay and padding included. A Vorbis stream's first packet after a reset
/// only primes the codec's overlap: it decodes to no frames.
pub(super) struct PacketDecoder<S: Sample> {
    decoder: Box<dyn AudioDecoder>,
    /// Whether the codec's first packet after a reset decodes to no frames.
    primes: bool,
    /// Whether a packet has decoded since the last reset.
    primed: bool,
    /// The last packet's frames, one vector per channel.
    planes: Vec<Vec<S>>,
    /// The last packet's frames as the decoder's 24-bit integers, for `S = I24`.
    wide: Vec<Vec<i24>>,
}

impl<S: Sample> PacketDecoder<S> {
    /// A decoder for the track whose parameters are `params`.
    pub(super) fn new(params: &AudioCodecParameters) -> Result<Self, Error> {
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default().gapless(false))?;
        Ok(Self {
            decoder,
            primes: params.codec == CODEC_ID_VORBIS,
            primed: false,
            planes: Vec::new(),
            wide: Vec::new(),
        })
    }

    /// Forgets the codec's overlap: the next packet is not continuous with the
    /// last one, as after a seek.
    pub(super) fn reset(&mut self) {
        self.decoder.reset();
        self.primed = false;
    }

    /// Decodes one packet into [`planes`](Self::planes), reusing the vectors
    /// across calls to avoid per-packet allocation. Returns the frame count,
    /// which every plane holds. This is the single decode-a-packet primitive
    /// shared by the whole-file load and [`FileIn`](crate::FileIn): given a
    /// packet already known to belong to the selected track.
    pub(super) fn decode(&mut self, packet: &Packet) -> Result<usize, Error> {
        let decoded = self.decoder.decode(packet)?;
        let priming = self.primes && !self.primed;
        self.primed = true;
        if priming {
            self.planes.clear();
            return Ok(0);
        }
        lift(&decoded, &mut self.planes, &mut self.wide);
        Ok(decoded.frames())
    }

    /// The frames of the last packet, one `Vec` per channel.
    pub(super) fn planes(&self) -> &[Vec<S>] {
        &self.planes
    }

    /// The format of the last decoded packet.
    pub(super) fn spec(&self) -> AudioSpec {
        self.decoder.last_decoded().spec().clone()
    }
}

/// Copies `decoded` into `planes`, a `Vec<Vec<S>>` for a sample format `S`: the decoder converts from the codec's
/// own format, which is no conversion when they agree. `wide` is the 24-bit
/// staging for `S = I24`.
fn lift(decoded: &GenericAudioBufferRef<'_>, planes: &mut dyn Any, wide: &mut Vec<Vec<i24>>) {
    macro_rules! direct {
        ($($t:ty),*) => {$(
            if let Some(p) = planes.downcast_mut::<Vec<Vec<$t>>>() {
                decoded.copy_to_vecs_planar::<$t>(p);
                return;
            }
        )*};
    }
    direct!(i8, i16, i32, f32, f64);
    // `Sample` is sealed to those five and `I24`.
    let Some(out) = planes.downcast_mut::<Vec<Vec<I24>>>() else {
        unreachable!("a Sample is i8, i16, I24, i32, f32 or f64")
    };
    decoded.copy_to_vecs_planar::<i24>(wide);
    out.resize_with(wide.len(), Vec::new);
    for (dst, src) in out.iter_mut().zip(wide.iter()) {
        dst.clear();
        dst.extend(
            src.iter()
                .map(|v| I24::new(v.0).expect("a decoder's 24-bit integer is within I24's range")),
        );
    }
}

/// The frame count of a track's whole decode: the container's count of valid
/// frames with the delay and padding frames the decoder also returns. A codec
/// whose first packet only primes (Vorbis) returns no frames for the delay,
/// which is that packet.
pub(super) fn total_frames_of(track: &Track, params: &AudioCodecParameters) -> Option<u64> {
    let delay = if params.codec == CODEC_ID_VORBIS {
        0
    } else {
        track.delay.unwrap_or(0)
    };
    track
        .num_frames
        .map(|valid| valid + u64::from(delay) + u64::from(track.padding.unwrap_or(0)))
}

/// Opens `asset` and probes its container, hinting the format from the asset's
/// extension.
///
/// The head every entry point shares; `FileIn::open` and
/// [`read_wave_metadata`] stop here, [`read_wave`] decodes on from it.
///
/// symphonia's [`MediaSource`] is `Sync`; a reader that is only `Send` is made
/// so by an [`Exclusive`], which is reached only through `&mut`.
pub(super) fn probe_asset(asset: &Asset) -> Result<Box<dyn FormatReader>, Error> {
    let mut hint = Hint::new();
    if let Some(extension) = asset.extension() {
        hint.with_extension(extension);
    }
    let mut reader = asset.open().map_err(Error::IoError)?;
    // The length is what lets a container (Ogg) find its last page, and so its
    // frame count.
    let start = reader.stream_position().map_err(Error::IoError)?;
    let len = reader.seek(SeekFrom::End(0)).map_err(Error::IoError)?;
    reader
        .seek(SeekFrom::Start(start))
        .map_err(Error::IoError)?;
    let source = Reader {
        reader: Exclusive::new(reader),
        len,
    };
    probe_source(Box::new(source), &hint)
}

/// A `Read + Seek + Send` source as a [`MediaSource`].
struct Reader<R> {
    reader: Exclusive<R>,
    /// The source's length in bytes.
    len: u64,
}

impl<R> Reader<R> {
    fn get(&mut self) -> &mut R {
        self.reader.get_mut()
    }
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.get().read(buf)
    }
}

impl<R: Seek> Seek for Reader<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.get().seek(pos)
    }
}

impl<R: Read + Seek + Send> MediaSource for Reader<R> {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

fn probe_source(
    mut source: Box<dyn MediaSource>,
    hint: &Hint,
) -> Result<Box<dyn FormatReader>, Error> {
    riff_guard::check(&mut *source)?;
    let stream = MediaSourceStream::new(source, Default::default());
    symphonia::default::get_probe().probe(
        hint,
        stream,
        FormatOptions::default(),
        MetadataOptions::default(),
    )
}

/// The first track with a known audio codec, with its audio parameters: the
/// one every loader here decodes.
pub(super) fn first_audio_track(
    reader: &dyn FormatReader,
) -> Result<(&Track, &AudioCodecParameters), DecodeError> {
    reader
        .tracks()
        .iter()
        .find_map(|track| match &track.codec_params {
            Some(CodecParameters::Audio(params)) if params.codec != CODEC_ID_NULL_AUDIO => {
                Some((track, params))
            }
            _ => None,
        })
        .ok_or(DecodeError::NoAudioTrack)
}

/// The track's sample rate, as the container states it. A container that
/// states none is refused, never given a rate of ours.
///
/// A rate of 0 states none.
pub(super) fn sample_rate_of(params: &AudioCodecParameters) -> Result<SampleRate, DecodeError> {
    params
        .sample_rate
        .and_then(SampleRate::from_hz)
        .ok_or(DecodeError::NoSampleRate)
}

/// Decodes the first audio track of `src` (a path, or bytes in memory) into
/// memory, as `S`.
///
/// Requires a decode feature (`decode-wav`, `decode-aiff`, `decode-flac`, `decode-mp3` or
/// `decode-ogg`). Supported formats are whatever those enable; see
/// [`decodable_extensions`](crate::decodable_extensions). The wave keeps
/// the file's own sample rate and channel count; nothing is resampled. Its
/// [`topology`](Wave::topology) is what the container names, in the order
/// the samples are in: a mono file is the centre, a channel that names no
/// position is [`Speaker::Unassigned`](tutti_buffers::Speaker::Unassigned), and
/// a position the vocabulary has no name for is
/// [`Speaker::Unknown`](tutti_buffers::Speaker::Unknown). Reads and
/// decodes the whole file on the calling thread, so do not call it on the
/// audio thread.
///
/// `S` is the format the samples arrive in. Asking for the file's own
/// ([`WaveMetadata::format`]) keeps every sample exactly; another is
/// converted by the decoder.
///
/// # Errors
///
/// A [`DecodeError`] if the source cannot be opened, its container is not one
/// this build reads, it has no audio track, the track states no sample
/// rate ([`DecodeError::NoSampleRate`]), or a packet fails to decode.
///
/// # Examples
///
/// ```no_run
/// let wave = tutti_codec::read_wave::<f32>("kick.wav")?; // or bytes: `Vec<u8>`, `Arc<[u8]>`
/// println!("{} channels, {} s", wave.channels(), wave.duration());
/// # Ok::<(), tutti_codec::DecodeError>(())
/// ```
pub fn read_wave<S: Sample>(src: impl Into<Asset>) -> Result<Wave<S>, DecodeError> {
    decode(probe_asset(&src.into()).map_err(convert)?)
}

/// Reads an audio source's frame count, sample rate, channel count and
/// sample format without decoding any audio.
///
/// Works for every format [`read_wave`] can read, and is fast enough for
/// duration and size decisions before a full load.
///
/// # Errors
///
/// A [`DecodeError`] if the source cannot be opened, its container is not one
/// this build reads, it has no audio track, or the track states no sample
/// rate ([`DecodeError::NoSampleRate`]).
///
pub fn read_wave_metadata(src: impl Into<Asset>) -> Result<WaveMetadata, DecodeError> {
    metadata_of(probe_asset(&src.into()).map_err(convert)?)
}

/// The metadata of a probed container's first audio track.
fn metadata_of(reader: Box<dyn FormatReader>) -> Result<WaveMetadata, DecodeError> {
    let (track, params) = first_audio_track(&*reader)?;
    let (layout, topology, placement) = topology_of(params.channels.as_ref());
    Ok(WaveMetadata {
        total_frames: total_frames_of(track, params),
        sample_rate: sample_rate_of(params)?,
        topology,
        layout,
        placement,
        format: native_format(params),
    })
}

/// Decode the first audio track of a probed container to its end.
fn decode<S: Sample>(reader: Box<dyn FormatReader>) -> Result<Wave<S>, DecodeError> {
    let mut decode = WaveDecode::start(reader)?;
    loop {
        if let Some(wave) = decode.step(Samples::new(usize::MAX))? {
            return Ok(wave);
        }
    }
}

/// A whole-file decode in steps: the work of [`read_wave`], one bounded piece at a
/// time, for a caller that must not hold its thread for the length of the file.
///
/// [`step`](Self::step) decodes packets until it has at least a number of frames,
/// and answers the [`Wave`] once the file ends. Decoding and file I/O block, so a
/// step is a blocking call as long as its frames take.
///
/// ```no_run
/// use tutti_codec::WaveDecode;
/// use tutti_units::Samples;
///
/// let mut decode = WaveDecode::<f32>::open("long.mp3")?;
/// let wave = loop {
///     if let Some(wave) = decode.step(Samples::new(65_536))? {
///         break wave;
///     }
/// };
/// println!("{} s", wave.duration());
/// # Ok::<(), tutti_codec::DecodeError>(())
/// ```
pub struct WaveDecode<S: Sample> {
    reader: Box<dyn FormatReader>,
    decoder: PacketDecoder<S>,
    track_id: u32,
    wave: Option<Wave<S>>,
}

impl<S: Sample> std::fmt::Debug for WaveDecode<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaveDecode")
            .field("track_id", &self.track_id)
            .field("frames", &self.wave.as_ref().map(|wave| wave.len().get()))
            .finish_non_exhaustive()
    }
}

impl<S: Sample> WaveDecode<S> {
    /// Opens `src` at its first audio track; no audio is decoded yet.
    ///
    /// # Errors
    ///
    /// The errors of [`read_wave`] that precede decoding: the source cannot be
    /// opened, its container is not one this build reads, it has no audio track,
    /// or the track states no sample rate.
    pub fn open(src: impl Into<Asset>) -> Result<Self, DecodeError> {
        Self::start(probe_asset(&src.into()).map_err(convert)?)
    }

    fn start(reader: Box<dyn FormatReader>) -> Result<Self, DecodeError> {
        let (track, params) = first_audio_track(&*reader)?;
        let track_id = track.id;
        let total_frames = total_frames_of(track, params);
        let sample_rate = sample_rate_of(params)?;
        let decoder = PacketDecoder::<S>::new(params).map_err(convert)?;
        let (layout, topology, placement) = topology_of(params.channels.as_ref());

        // Pre-allocate using the metadata frame count when available (WAV, FLAC).
        let wave = total_frames.map(|total| {
            let mut wave = empty_wave(layout, topology, placement, sample_rate);
            wave.reserve(Samples::new(reserved_frames(total, wave.channels())));
            wave
        });
        Ok(Self {
            reader,
            decoder,
            track_id,
            wave,
        })
    }

    /// Decodes packets until at least `frames` more frames are in, or the file
    /// ends. `Some` is the finished wave (after which the decode is spent);
    /// `None` means call again.
    ///
    /// # Errors
    ///
    /// The errors of [`read_wave`]: a packet that does not decode, or a stream with
    /// no audio at all. A read error after audio was decoded ends the stream and
    /// answers the wave so far.
    pub fn step(&mut self, frames: Samples) -> Result<Option<Wave<S>>, DecodeError> {
        let before = self.wave.as_ref().map_or(0, |wave| wave.len().get());
        loop {
            let packet = match self.reader.next_packet() {
                Ok(Some(packet)) => packet,
                // A clean end of stream. A file with no packet at all is empty, not a
                // wave.
                Ok(None) => return self.finish(None),
                // Any read error ends the stream.
                Err(err) => return self.finish(Some(err)),
            };

            // A packet of another track is not ours to decode.
            if packet.track_id != self.track_id {
                continue;
            }

            let buffer_len = self.decoder.decode(&packet).map_err(convert)?;
            let wave_output = match &mut self.wave {
                Some(wave) => wave,
                none => {
                    let spec = self.decoder.spec();
                    let rate = SampleRate::from_hz(spec.rate()).ok_or(DecodeError::NoSampleRate)?;
                    let (layout, topology, placement) = topology_of(Some(spec.channels()));
                    none.insert(empty_wave(layout, topology, placement, rate))
                }
            };

            // A primed-over packet holds no frames and so no planes.
            if buffer_len > 0 {
                Planes::from_vecs(self.decoder.planes())
                    .and_then(|planes| wave_output.append_planes(planes))
                    .map_err(|e| DecodeError::Malformed(e.to_string().into()))?;
            }
            if wave_output.len().get().saturating_sub(before) >= frames.get() {
                return Ok(None);
            }
        }
    }

    /// The end of the stream: the wave, or why there is none.
    fn finish(&mut self, read_error: Option<Error>) -> Result<Option<Wave<S>>, DecodeError> {
        match self.wave.take() {
            Some(wave) => Ok(Some(wave)),
            None => Err(read_error.map_or_else(end_of_stream, convert)),
        }
    }
}

/// The most samples, across all channels, reserved before the first packet.
const RESERVE_SAMPLES: u64 = 1 << 24;

/// The frames per channel to reserve for a header stating `total`: the header's
/// count is untrusted, so a larger one is reserved only up to
/// [`RESERVE_SAMPLES`] and the vectors grow with the packets actually decoded.
fn reserved_frames(total: u64, channels: usize) -> usize {
    let cap = RESERVE_SAMPLES / channels.max(1) as u64;
    total.min(cap) as usize
}

/// What a stream with no packet at all is refused with.
pub(super) fn end_of_stream() -> DecodeError {
    DecodeError::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "end of stream",
    ))
}

#[cfg(all(test, feature = "decode-wav"))]
mod tests;
