//! Asking a file whether this sampler can stream it, before committing to play it.
//!
//! A caller choosing a playback tier (a resident wave or a disk stream) needs
//! two facts about a file: how long it is,
//! and whether the streamer can serve arbitrary ranges from it. Both come from the
//! container header, and neither needs a single audio packet decoded.
//!
//! "Can it stream" is a fact about this crate's machinery: the streamer's
//! incremental decoder can seek in the file, given the codecs compiled into
//! this build. The streamer applies the same rule when it registers a stream,
//! and a host reads the same function. Nothing is decoded: the container head
//! is read and no more; how much of a file to keep resident is the caller's
//! policy.

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_platform::Asset;
use tutti_units::{SampleRate, Samples};

/// What a header says about a file, and what this sampler can do with it.
///
/// Returned by [`probe`]. Every field is header-derived — no audio was decoded.
///
/// `PartialEq` but not `Eq`: `SampleRate` is float-backed, like every measure
/// unit in the engine's vocabulary. A host caching these compares them with
/// `==` as usual; it just cannot use one as a `HashMap` key.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleFacts {
    /// Total length in frames, not interleaved samples, if the container
    /// reports one.
    ///
    /// `None` for some streamed/VBR formats that do not store a length. A
    /// caller sizing a preload window against this must treat `None` as
    /// "unknown", not as "zero" — and note that [`streamable`](Self::streamable)
    /// is already `false` whenever this is `None`, because the streamer cannot
    /// stream a file whose end it cannot find.
    pub frames: Option<Samples>,
    /// The file's own rate, which need not be the engine's.
    ///
    /// A voice reading this file converts through it, not through the session
    /// rate; the ratio between the two is the [`SrcRatio`](tutti_units::SrcRatio)
    /// the streaming path carries.
    pub sample_rate: SampleRate,
    /// The file's channel count. Multiply by [`frames`](Self::frames) to size an
    /// interleaved buffer.
    pub layout: ChannelLayout,
    /// Which speaker each channel of the file feeds, in sample order; `None` for a
    /// file wider than a topology holds.
    ///
    /// What a voice over this file would place by: a channel no speaker names
    /// is [`Speaker::Unknown`](tutti_buffers::Speaker::Unknown), and a
    /// memory voice over such a file is refused
    /// ([`PlaceError`](crate::PlaceError)). A host that wants to say so before
    /// it loads the file reads it here.
    pub topology: Option<ChannelTopology>,
    /// Whether this build's streamer can stream this file.
    ///
    /// The sampler's verdict, not a property of the file alone. False when the
    /// container reports no frame count, when the decoder declines to seek, or
    /// when no codec feature covering this format was compiled in.
    ///
    /// A caller that sees `false` has no tier choice left: the file must be
    /// played wholly from memory, because the streaming path does not exist for
    /// it. That is the same fallback the streamer takes internally
    /// (`load_wave` + `LruCache`).
    pub streamable: bool,
}

/// Errors a probe can report. Terminal: none of these fix themselves on retry.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SampleProbeError {
    /// The file could not be opened, or has no track this build can decode.
    #[error("could not read {name}: {source}")]
    Unreadable {
        /// The probed asset's [`name`](Asset::name).
        name: String,
        /// The decoder's error.
        #[source]
        source: tutti_codec::DecodeError,
    },
}

/// Reads `asset`'s header and reports what this sampler can do with it.
///
/// Blocking file I/O. A host on a frame budget runs this off the main thread;
/// this crate deliberately does not choose a task system for it.
///
/// # Errors
///
/// [`SampleProbeError::Unreadable`] when the file cannot be opened or holds no track
/// this build can decode. A file that decodes but cannot be streamed is not an
/// error: it reports `streamable: false`.
///
/// # Examples
///
/// ```no_run
/// # use tutti_sampler::probe;
/// let facts = probe("kick.wav")?;
/// if facts.streamable {
///     // free to pick either tier
/// } else {
///     // must be resident — the streaming path does not exist for this file
/// }
/// # Ok::<(), tutti_sampler::SampleProbeError>(())
/// ```
pub fn probe(asset: impl Into<Asset>) -> Result<SampleFacts, SampleProbeError> {
    let asset = asset.into();
    let meta = tutti_codec::read_wave_metadata(asset.clone()).map_err(|source| {
        SampleProbeError::Unreadable {
            name: asset.name(),
            source,
        }
    })?;
    Ok(SampleFacts {
        frames: meta.total_frames.map(|f| Samples::new(f as usize)),
        sample_rate: meta.sample_rate,
        layout: meta.layout,
        streamable: streamable(&asset, &meta),
        topology: meta.topology,
    })
}

/// The streamer's conditions for streaming a file, in the order it applies them.
///
/// The one place these conditions live: `streamer::handlers::open_stream` asks
/// [`probe`] for its verdict instead of re-deriving it, so a host picking a
/// playback tier and the streamer cannot disagree.
fn streamable(asset: &Asset, meta: &tutti_codec::WaveMetadata) -> bool {
    // 1. No frame count means no seekable end — the streamer falls back whole-file.
    if meta.total_frames.is_none() {
        return false;
    }
    // 2. The incremental decoder must actually open.
    let Ok(decoder) = tutti_codec::FileIn::<f32>::open(asset.clone()) else {
        return false;
    };
    // 3. Defensive, and inherited from `open_stream`: a decoder that reports
    //    itself non-seekable despite a frame count is still not streamable.
    decoder.seekable()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A minimal real PCM wav, `frames` frames of stereo silence, written
    /// inside a caller-owned [`TempDir`](tempfile::TempDir).
    ///
    /// The directory is the caller's, not this helper's: a fixed shared path
    /// would let two test processes (nextest runs each test in its own)
    /// create, read and remove the same bytes, and a probe could see a
    /// half-written or already-deleted file. Each test owning its `TempDir`
    /// removes the sharing, and the directory is cleaned up on drop.
    fn write_wav(dir: &tempfile::TempDir, name: &str, frames: usize) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).expect("create");
        let data = (frames * 2 * 2) as u32;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&48_000u32.to_le_bytes()).unwrap();
        f.write_all(&192_000u32.to_le_bytes()).unwrap();
        f.write_all(&4u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data.to_le_bytes()).unwrap();
        f.write_all(&vec![0u8; data as usize]).unwrap();
        path
    }

    #[test]
    fn a_wav_reports_its_header_without_decoding() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_wav(&dir, "probe_ok.wav", 4800);
        let facts = probe(&path).expect("readable");
        assert_eq!(facts.frames, Some(Samples::new(4800)));
        assert_eq!(facts.sample_rate, SampleRate::SR_48K);
        assert_eq!(facts.layout.count(), 2);
        assert!(
            facts.streamable,
            "a local PCM wav has a frame count and seeks"
        );
    }

    #[test]
    fn a_bytes_asset_reports_the_header_its_bytes_hold() {
        let dir = tempfile::tempdir().expect("temp dir");
        let bytes = std::fs::read(write_wav(&dir, "probe_bytes.wav", 4800)).expect("read");
        drop(dir);
        let facts = probe(bytes).expect("readable");
        assert_eq!(facts.frames, Some(Samples::new(4800)));
        assert_eq!(facts.sample_rate, SampleRate::SR_48K);
        assert!(facts.streamable, "the bytes have a frame count and seek");
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_default() {
        // The alternative — returning zeroed facts — would let a caller build a
        // voice on a file that is not there and hear nothing, with no error
        // anywhere.
        let err = probe("/nonexistent/nowhere.wav").unwrap_err();
        assert!(matches!(err, SampleProbeError::Unreadable { .. }));
    }

    /// An unstreamable file is not an error — it is a legitimate verdict that
    /// removes the caller's tier *choice* rather than its ability to play.
    #[test]
    fn a_zero_length_file_still_probes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_wav(&dir, "probe_empty.wav", 0);
        let facts = probe(&path).expect("an empty wav is still a valid wav");
        assert_eq!(facts.frames, Some(Samples::new(0)));
    }
}
