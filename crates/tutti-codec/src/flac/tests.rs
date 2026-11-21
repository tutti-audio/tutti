use super::*;

fn configs() -> Configs {
    Configs {
        full: encoder_config(FlacLevel::Standard).into_verified().unwrap(),
        short: short_block_config(FlacLevel::Standard)
            .into_verified()
            .unwrap(),
    }
}

/// An empty export states valid block sizes (the stream's, 4 096 both)
/// and "unknown" frame sizes, not flacenc's unset 65 535 / 0 and
/// `u32::MAX` / 0.
#[test]
fn an_empty_stream_states_valid_sizes() {
    let info = StreamInfo::new(48_000, 2, 16).unwrap();
    let info = FrameWriter::start(configs(), info, 2, 16)
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(
        (info.min_block_size(), info.max_block_size()),
        (BLOCK_SIZE, BLOCK_SIZE)
    );
    assert_eq!((info.min_frame_size(), info.max_frame_size()), (0, 0));
    assert_eq!(info.total_samples(), 0);
    assert_eq!(header_bytes(&info).unwrap().len(), 42);
}

/// After a frame is lost to a sink failure the writer encodes no more: `write`
/// and `finish` return errors.
#[test]
fn a_writer_that_lost_a_frame_fails_every_later_call() {
    use crate::test_sink::Flaky;
    use crate::{FormatWriter, Spec};
    use tutti_buffers::ChannelLayout;
    use tutti_buffers::Interleaved;
    use tutti_units::SampleRate;

    let spec = Spec::of_width(
        crate::Format::Flac {
            depth: IntDepth::Int16,
            level: FlacLevel::Standard,
        },
        ChannelLayout::STEREO,
        SampleRate::SR_44K1,
    );
    let mut w = FlacWriter::create(&spec, Flaky::failing_write(3)).unwrap();
    let samples: Vec<f32> = (0..BLOCK_SIZE * 2)
        .map(|i| ((i * 31) % 255) as f32 / 255.0 - 0.5)
        .collect();
    let block = || Interleaved::new(&samples, ChannelLayout::STEREO).unwrap();
    let mut failed = false;
    for _ in 0..200 {
        if w.write(block()).is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed, "the sink's failure reaches a write");
    assert!(w.write(block()).is_err());
    assert!(w.finish().is_err());
}
