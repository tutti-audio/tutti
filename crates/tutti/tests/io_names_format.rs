//! `FileOut` writes a `Format`: a build with `io` (or `audio-io`) and no format
//! feature must still name one, and build a WAV sink from it. `FileOut` is
//! `tutti-codec`'s, a dev-dependency of this crate.
//!
//! Compiled only with `io`.
#![cfg(feature = "io")]

use tutti::codec::Spec;
use tutti::types::Sink;
use tutti::{ChannelLayout, SampleRate};
use tutti_codec::{FileOut, Format, Pcm};

#[test]
fn io_reaches_a_wav_sink_without_a_format_feature() {
    let dir = tempfile::tempdir().expect("temp dir");
    let spec = Spec::of_width(
        Format::Wav(Pcm::Int16),
        ChannelLayout::STEREO,
        SampleRate::SR_48K,
    );
    let sink = FileOut::create(&spec, dir.path().join("take.wav"));
    sink.expect("a WAV sink opens").finalize().expect("closes");
}
