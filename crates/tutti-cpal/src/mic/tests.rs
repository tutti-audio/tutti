use super::*;
use tutti_buffers::Planes;

use tutti_platform::Instant;

use tutti_buffers::{Interleaved, InterleavedMut, Sink};
use tutti_engine::{BackendErrorKind, STALL_GRACE};

/// A device that supports `[min, max]` at `format`.
fn range(min: u32, max: u32, format: cpal::SampleFormat) -> cpal::SupportedStreamConfigRange {
    cpal::SupportedStreamConfigRange::new(2, min, max, cpal::SupportedBufferSize::Unknown, format)
}

/// One pull into `buf`, viewed at the mic's own width.
fn pull_into<F: Float>(mic: &mut MicIn<F>, buf: &mut [F]) -> Pull {
    let out = InterleavedMut::new(buf, Source::layout(mic)).unwrap();
    mic.pull(out)
}

/// What a pull of an empty ring answers, whatever width the mic has.
fn verdict(mic: &mut MicIn) -> Pull {
    pull_into(mic, &mut [0.0; 16])
}

fn fallback(rate: u32) -> cpal::SupportedStreamConfig {
    range(rate, rate, cpal::SampleFormat::F32).with_sample_rate(rate)
}

/// The input pick with `wanted`, at 48 kHz over `ranges`.
fn choose_wanting(
    ranges: &[cpal::SupportedStreamConfigRange],
    wanted: Option<SampleFormat>,
) -> Result<cpal::SupportedStreamConfig> {
    choose_input_config(
        ranges.iter().cloned(),
        fallback(44_100),
        SampleRate::SR_48K,
        wanted,
        "Format Mic",
    )
}

/// **With no format chosen, F32 is taken over an integer format** the device
/// lists first.
#[test]
fn an_unset_input_format_prefers_f32() {
    let chosen = choose_wanting(
        &[
            range(44_100, 48_000, cpal::SampleFormat::I16),
            range(44_100, 48_000, cpal::SampleFormat::F32),
        ],
        None,
    )
    .unwrap();
    assert_eq!(chosen.sample_format(), cpal::SampleFormat::F32);
}

/// **A chosen I16 is taken over the F32 the default would prefer.**
#[test]
fn a_chosen_i16_input_is_taken_over_f32() {
    let chosen = choose_wanting(
        &[
            range(44_100, 48_000, cpal::SampleFormat::F32),
            range(44_100, 48_000, cpal::SampleFormat::I16),
        ],
        Some(SampleFormat::I16),
    )
    .unwrap();
    assert_eq!(chosen.sample_format(), cpal::SampleFormat::I16);
}

/// **A chosen I16 on a device that offers only U16 opens the U16 type**, which
/// the callback decodes as I16 with the sign bit flipped; with both offered the
/// signed one wins.
#[test]
fn a_chosen_i16_input_on_a_u16_only_device_opens_u16() {
    let only = choose_wanting(
        &[range(44_100, 48_000, cpal::SampleFormat::U16)],
        Some(SampleFormat::I16),
    )
    .unwrap();
    assert_eq!(only.sample_format(), cpal::SampleFormat::U16);
    let both = choose_wanting(
        &[
            range(44_100, 48_000, cpal::SampleFormat::U16),
            range(44_100, 48_000, cpal::SampleFormat::I16),
        ],
        Some(SampleFormat::I16),
    )
    .unwrap();
    assert_eq!(both.sample_format(), cpal::SampleFormat::I16);
}

/// **A chosen format the device lacks at the rate is the named error**, with
/// the device, the format and the rate; it is not another format, and it is not
/// a rate mismatch (the rate is covered).
#[test]
fn a_chosen_input_format_the_device_lacks_is_a_named_error() {
    let err = choose_wanting(
        &[range(44_100, 48_000, cpal::SampleFormat::F32)],
        Some(SampleFormat::I16),
    )
    .expect_err("only F32 is offered");
    assert!(
        matches!(
            &err,
            Error::InputFormatUnavailable { device_name, format: SampleFormat::I16, rate }
                if device_name == "Format Mic" && *rate == SampleRate::SR_48K
        ),
        "{err:?}"
    );
}

/// **A format offered only at another rate is the format error's neighbour,
/// not it**: a device with no range at the graph's rate is a rate mismatch
/// whatever the format asked.
#[test]
fn a_chosen_format_on_a_device_missing_the_rate_is_a_rate_mismatch() {
    let err = choose_wanting(
        &[range(44_100, 44_100, cpal::SampleFormat::I16)],
        Some(SampleFormat::I16),
    )
    .expect_err("44.1k cannot serve a 48k graph");
    assert!(matches!(err, Error::SampleRateMismatch { .. }), "{err:?}");
}

/// **A device that can run at the graph's rate is opened at it**, not at
/// whatever its default happens to be.
///
/// Before this, `open_inner` took `default_input_config()` unconditionally.
/// A mic defaulting to 44.1 kHz on a 48 kHz graph opened at 44.1 and
/// drifted, because an inlet does not resample and nothing
/// compared the two rates.
#[test]
fn a_device_covering_the_graph_rate_is_opened_at_it() {
    let chosen = choose_input_config(
        [range(8_000, 96_000, cpal::SampleFormat::F32)].into_iter(),
        fallback(44_100),
        SampleRate::SR_48K,
        None,
        "Wide Range Mic",
    )
    .expect("48k is inside [8k, 96k]");
    assert_eq!(chosen.sample_rate(), 48_000);
}

/// **F32 wins when several supported ranges cover the rate.**
///
/// Not cosmetic: `build_input` converts every other format per sample, and
/// F32 is a float device format and needs no integer scaling.
#[test]
fn f32_is_preferred_over_an_integer_format_at_the_same_rate() {
    let chosen = choose_input_config(
        [
            range(44_100, 48_000, cpal::SampleFormat::I16),
            range(44_100, 48_000, cpal::SampleFormat::F32),
        ]
        .into_iter(),
        fallback(44_100),
        SampleRate::SR_48K,
        None,
        "Dual Format Mic",
    )
    .expect("both ranges cover 48k");
    assert_eq!(chosen.sample_format(), cpal::SampleFormat::F32);
    assert_eq!(chosen.sample_rate(), 48_000);
}

/// An integer-only device is still accepted — preference is not a
/// requirement.
#[test]
fn an_integer_only_device_is_accepted_at_the_graph_rate() {
    let chosen = choose_input_config(
        [range(44_100, 48_000, cpal::SampleFormat::I16)].into_iter(),
        fallback(44_100),
        SampleRate::SR_48K,
        None,
        "I16 Mic",
    )
    .expect("the range covers 48k");
    assert_eq!(chosen.sample_format(), cpal::SampleFormat::I16);
    assert_eq!(chosen.sample_rate(), 48_000);
}

/// **A device that cannot reach the graph's rate is an error, naming both
/// rates.**
///
/// The whole point: silent drift becomes a refusal a host can show.
#[test]
fn a_device_that_cannot_reach_the_graph_rate_is_refused() {
    let err = choose_input_config(
        [range(44_100, 44_100, cpal::SampleFormat::F32)].into_iter(),
        fallback(44_100),
        SampleRate::SR_48K,
        None,
        "Fixed 44k1 Mic",
    )
    .expect_err("44.1k-only cannot serve a 48k graph");

    match err {
        Error::SampleRateMismatch {
            device_name,
            device,
            graph,
        } => {
            assert_eq!(device_name, "Fixed 44k1 Mic");
            assert_eq!(device.get(), 44_100.0);
            assert_eq!(graph.get(), 48_000.0);
        }
        other => panic!("expected SampleRateMismatch, got {other:?}"),
    }
}

/// A device that advertises nothing but whose default already matches is
/// accepted — some backends report an empty supported-config list.
#[test]
fn a_default_that_already_matches_is_accepted_with_no_ranges() {
    let chosen = choose_input_config(
        std::iter::empty(),
        fallback(48_000),
        SampleRate::SR_48K,
        None,
        "Silent About Its Configs",
    )
    .expect("the default already runs at the graph rate");
    assert_eq!(chosen.sample_rate(), 48_000);
}

/// A `MicIn` over an empty ring and a fresh fault record, with no device.
fn mic_without_a_device() -> MicIn {
    mic_with_ring(2, 8).0
}

/// [`mic_without_a_device`] `width` wide, and the writer its callback would
/// hold.
fn mic_with_ring(width: u16, frames: usize) -> (MicIn, TapWriter) {
    let (reader, writer) = TapReader::pair(usize::from(width), Samples::new(frames));
    let mic = MicIn {
        tap: reader,
        sample_rate: SampleRate::SR_48K,
        device_inputs: ChannelLayout::from(width),
        faults: Arc::new(StreamFaults::new()),
        latency: Arc::new(StreamLatency::new()),
        _stream: StreamHandle(Owned::spawn("test-input", || Ok(())).expect("spawn")),
    };
    (mic, writer)
}

/// A full ring drops the callback's frames and the source counts them; the
/// count clears when read. (The stream that fills the ring needs a device; the
/// writer's refusal is the same one `Capture::block` hits.)
#[test]
fn a_full_ring_counts_the_frames_the_callback_dropped() {
    let (mut mic, mut writer) = mic_with_ring(2, 2);
    for i in 0..5 {
        writer.push_interleaved(Interleaved::new(&[i as f32, 0.0], writer.layout()).unwrap());
    }
    assert_eq!(Source::take_lost(&mut mic), Samples::new(3));
    assert_eq!(
        Source::take_lost(&mut mic),
        Samples::new(0),
        "reading clears"
    );
}

/// A capture of `device` inputs, selecting `inputs`, over rings of room for
/// `frames` frames; the mic end and the monitor's.
fn capture<F: Float>(
    device: u16,
    inputs: &[usize],
    frames: usize,
) -> (Capture<F>, MicIn<F>, TapReader<F>) {
    let select = Fold::select(ChannelLayout::from(device), inputs).expect("a selection");
    let width = select.dst_layout();
    let (reader, rec) = TapReader::pair(usize::from(width.count()), Samples::new(frames));
    let (ring, mon) = TapReader::pair(usize::from(width.count()), Samples::new(frames));
    let capture = Capture::new(select, ChannelLayout::from(device), rec, Some(mon));
    let mic = MicIn {
        tap: reader,
        sample_rate: SampleRate::SR_48K,
        device_inputs: ChannelLayout::from(device),
        faults: Arc::new(StreamFaults::new()),
        latency: Arc::new(StreamLatency::new()),
        _stream: StreamHandle(Owned::spawn("test-input", || Ok(())).expect("spawn")),
    };
    (capture, mic, ring)
}

/// Inputs 3 and 4 of 16 reach the recording as its two channels, in that
/// order, and the monitor ring carries the same two; inputs beyond the first
/// two are reachable, which the stereo fold never allowed.
///
/// The device frames are `100 * frame + input`, so every sample says where it
/// came from.
#[test]
fn inputs_three_and_four_of_sixteen_are_the_recording() {
    let (mut cap, mut mic, ring) = capture(16, &[2, 3], 64);
    let data: Vec<f32> = (0..3)
        .flat_map(|f| (0..16).map(move |c| (100 * f + c) as f32))
        .collect();
    cap.block(&data, edge::from_native::<f32, f32>);
    assert_eq!(Source::layout(&mic), ChannelLayout::STEREO);
    assert_eq!(
        mic.topology().positions().unwrap(),
        [tutti_buffers::Speaker::Unassigned; 2]
    );
    assert_eq!(mic.device_inputs(), ChannelLayout::from(16u16));
    let mut out = [0.0f32; 8];
    assert_eq!(pull_into(&mut mic, &mut out), Pull::Frames(Samples::new(3)));
    assert_eq!(out[..6], [2.0, 3.0, 102.0, 103.0, 202.0, 203.0]);

    let mut node =
        tutti_graph::contract::prepared(tutti_graph::InletNode::new(ring), SampleRate::SR_48K, 64);
    let env = tutti_graph::Env {
        frame: tutti_units::Frame::new(0),
        sample_rate: SampleRate::SR_48K,
        block_len: Samples::new(3),
        transport: tutti_graph::TransportPosition::default(),
        changes: tutti_graph::TransportChanges::NONE,
        play: tutti_graph::PlayClock::ZERO,
    };
    let heard =
        tutti_graph::contract::drive_input(&mut node, &env, Planes::default(), &[], &[]).audio;
    assert_eq!(heard[0], [2.0, 102.0, 202.0]);
    assert_eq!(heard[1], [3.0, 103.0, 203.0]);
}

/// A six-input interface delivers six unassigned channels when asked, and is no
/// longer folded as 5.1: the sixth input arrives untouched beside the first.
/// One input may feed both sides (`&[0, 0]`), which is what `open` does for a
/// mono device.
#[test]
fn a_six_input_interface_is_six_inputs_not_five_point_one() {
    let (mut cap, mut mic, _ring) = capture(6, &[0, 1, 2, 3, 4, 5], 64);
    cap.block(
        &[0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6],
        edge::from_native::<f32, f32>,
    );
    let mut out = [0.0f32; 6];
    assert_eq!(pull_into(&mut mic, &mut out), Pull::Frames(Samples::new(1)));
    assert_eq!(out, [0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);

    let (mut cap, mut mic, _ring) = capture(1, &[0, 0], 64);
    cap.block(&[0.25f32, -0.5], edge::from_native::<f32, f32>);
    let mut out = [0.0f32; 4];
    assert_eq!(pull_into(&mut mic, &mut out), Pull::Frames(Samples::new(2)));
    assert_eq!(out, [0.25, 0.25, -0.5, -0.5]);
}

/// A callback larger than the chunk runs in chunks, in order, and an integer
/// device converts per sample.
///
/// 150 frames of `i16` input 1 (of 2) are 64 + 64 + 22: the frames come out
/// whole and in order.
#[test]
fn a_big_callback_runs_in_chunks_and_integers_convert() {
    let (mut cap, mut mic, _ring) = capture(2, &[1], 512);
    let data: Vec<i16> = (0..150).flat_map(|f| [0, f as i16 * 100]).collect();
    cap.block(&data, edge::from_native::<i16, f32>);
    let mut out = vec![0.0f32; 200];
    let n = pull_into(&mut mic, &mut out);
    assert_eq!(n, Pull::Frames(Samples::new(150)));
    for (f, &s) in out[..150].iter().enumerate() {
        // An `i16` over 2^15 is exact in `f32`.
        let want = f32::from(f as i16 * 100) / 32768.0;
        assert_eq!(s, want, "frame {f}");
    }
}

/// The default selection is the first two inputs, or a lone input twice.
#[test]
fn the_default_is_the_first_two_inputs_or_the_lone_one_twice() {
    assert_eq!(default_inputs(ChannelLayout::MONO), [0, 0]);
    assert_eq!(default_inputs(ChannelLayout::STEREO), [0, 1]);
    assert_eq!(default_inputs(ChannelLayout::from(16u16)), [0, 1]);
}

/// An input the device does not have is an `Error::Inputs`, with `FoldError`'s
/// reason; so is an empty selection.
#[test]
fn an_input_past_the_devices_last_is_an_error() {
    let err = Error::from(Fold::<f32>::select(ChannelLayout::from(4u16), &[2, 4]).unwrap_err());
    assert!(
        matches!(
            err,
            Error::Inputs(tutti_buffers::FoldError::NoSuchChannel { channel: 4, .. })
        ),
        "{err}"
    );
    assert!(matches!(
        Error::from(Fold::<f32>::select(ChannelLayout::STEREO, &[]).unwrap_err()),
        Error::Inputs(tutti_buffers::FoldError::Buffer(
            tutti_buffers::BufferError::ZeroWidth
        ))
    ));
}

/// A disconnect reported through the shared fault record fails the source, and
/// `faults()` is that same record. Another backend error does not: it may be
/// transient. cpal's error callback is `move |err| edge::record(&faults, &err)`; the
/// registration itself needs a device and is not exercised here.
#[test]
fn a_disconnect_fails_the_mic_and_a_transient_error_does_not() {
    let mut mic = mic_without_a_device();
    assert_eq!(
        verdict(&mut mic),
        Pull::Starved,
        "a live mic with an empty ring is late, not finished"
    );

    mic.faults()
        .record(BackendErrorKind::Other, &"xrun", Instant::now());
    assert!(mic.faults().is_faulted());
    assert_eq!(
        verdict(&mut mic),
        Pull::Starved,
        "a backend error is not a death"
    );

    mic.faults().record(
        BackendErrorKind::DeviceGone,
        &"device not available",
        Instant::now(),
    );
    assert_eq!(verdict(&mut mic), Pull::Failed);
}

/// A mic whose device disconnects still hands over the frames its ring held,
/// and then reads as failed, so a pump that drains before it checks
/// (`Source::pull`'s contract: the fault flag is read before the ring and
/// `Failed` needs an empty ring; `tutti-session`'s `PumpLoop` is the pump that
/// relies on it, and cannot be a dependency here) writes them and ends the take
/// as failed.
#[test]
fn a_disconnected_mic_hands_over_its_buffered_frames_then_reads_failed() {
    let (mut mic, mut prod) = mic_with_ring(2, 8);
    let faults = mic.faults();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mic-fault.wav");
    let mut sink = mic
        .matching_sink(&path, Format::Wav(tutti_codec::Pcm::Float32))
        .unwrap();

    prod.push_interleaved(Interleaved::new(&[0.5, -0.5], prod.layout()).unwrap());
    faults.record(
        BackendErrorKind::DeviceGone,
        &"device not available",
        Instant::now(),
    );
    // The pump's pass: the frame the ring held is written first.
    let mut scratch = [0.0f32; 8];
    let Pull::Frames(n) = pull_into(&mut mic, &mut scratch) else {
        panic!("the buffered frame is delivered before the failure");
    };
    assert_eq!(n, Samples::new(1));
    let layout = Source::layout(&mic);
    let written = Interleaved::new(&scratch[..layout.interleaved_len(n)], layout).unwrap();
    sink.write(written).expect("the file takes it");
    // Then nothing more comes, and the source says why.
    assert_eq!(
        pull_into(&mut mic, &mut scratch),
        Pull::Failed,
        "the take is over: the device is gone"
    );
    Sink::finalize(sink).expect("the prefix closes cleanly");
    assert_eq!(hound::WavReader::open(&path).unwrap().len(), 2);
}

/// The ALSA shape of an unplugged mic, a backend error repeated with no audio,
/// fails the source.
#[test]
fn a_mic_whose_backend_keeps_failing_without_audio_is_failed() {
    let mut mic = mic_without_a_device();
    let faults = mic.faults();
    let err = "`alsa::poll()` returned POLLERR";
    let run_start = Instant::now();
    faults.record(BackendErrorKind::Other, &err, run_start);
    assert_eq!(verdict(&mut mic), Pull::Starved);
    // The run of errors becomes a disconnect once the grace has passed with no
    // audio delivered.
    faults.record(BackendErrorKind::Other, &err, run_start + STALL_GRACE);
    assert_eq!(verdict(&mut mic), Pull::Failed);
}

// `Capture` is crate-private, so its allocation gate lives in the library's own
// test binary rather than in `tests/` (where the output gates are). Nothing
// else in this binary allocates inside an `assert_no_alloc` section.
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// The capture callback does not allocate: blocks of `f32` and `i16`, wider than
/// a chunk, with the recording ring and the monitor ring both filling and
/// then refusing (the full-ring branch), run under a disabled allocator.
#[test]
fn the_capture_callback_does_not_allocate() {
    let (mut cap, mut mic, _ring) = capture(8, &[2, 3], 256);
    let f32s = vec![0.25f32; 8 * 150];
    let i16s = vec![1000i16; 8 * 150];
    // Warm both conversions, outside the gate.
    cap.block(&f32s, edge::from_native::<f32, f32>);
    cap.block(&i16s, edge::from_native::<i16, f32>);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..8 {
            cap.block(&f32s, edge::from_native::<f32, f32>);
            cap.block(&i16s, edge::from_native::<i16, f32>);
        }
    });
    // The rings filled and refused whole frames; nothing was lost silently.
    assert!(Source::take_lost(&mut mic).get() > 0);
}

/// The monitor ring is 10 ms and the capture ring one second, in frames at the
/// stream's rate: 480 and 48 000 at 48 kHz, 441 and 44 100 at 44.1 kHz.
#[test]
fn ring_depths_follow_the_stream_rate() {
    for (rate, monitor, capture) in [
        (SampleRate::SR_48K, 480, 48_000),
        (SampleRate::SR_44K1, 441, 44_100),
    ] {
        assert_eq!(MONITOR_RING.to_samples_ceil(rate), Samples::new(monitor));
        assert_eq!(CAPTURE_RING.to_samples_ceil(rate), Samples::new(capture));
    }
}

/// An offset-binary device reads by the rule in `Sample` on the signed format
/// of its width, after the sign bit flips back: mid-scale is silence, `3 * 2^14`
/// is 0.5 and 0 is -1.
#[test]
fn an_offset_binary_input_reads_mid_scale_as_silence() {
    let (mut cap, mut mic, _ring) = capture(1, &[0], 64);
    cap.block(&[32768u16, 49152, 0], edge::from_offset::<u16, f32>);
    let mut out = vec![9.0f32; 8];
    assert_eq!(pull_into(&mut mic, &mut out), Pull::Frames(Samples::new(3)));
    assert_eq!(out[..3], [0.0, 0.5, -1.0]);
    assert_eq!(edge::from_offset::<u8, f32>(128), 0.0);
    assert_eq!(edge::from_offset::<u32, f32>(2_147_483_648), 0.0);
}

/// An `i32` device keeps its 31 bits in an `f64` graph: `0x4000_0001` is
/// `(2^30 + 1) / 2^31` exactly, a value an `f32` cannot hold (24 bits).
#[test]
fn an_i32_capture_in_an_f64_graph_keeps_more_than_24_bits() {
    let want = f64::from((1u32 << 30) + 1) / f64::from(1u32 << 31);
    assert_ne!(
        f64::from(edge::from_native::<i32, f32>(0x4000_0001)),
        want,
        "setup: an f32 loses the last bit"
    );
    let (mut cap, mut mic, _ring) = capture::<f64>(1, &[0], 8);
    cap.block(
        &[0x4000_0001i32, i32::MIN, 0],
        edge::from_native::<i32, f64>,
    );
    let mut out = [9.0f64; 4];
    assert_eq!(pull_into(&mut mic, &mut out), Pull::Frames(Samples::new(3)));
    assert_eq!(out[..3], [want, -1.0, 0.0]);
}

/// Offset-binary `u8`, `u16` and `u32` read by the rule in `Sample` on the
/// signed format of their width, in either float: mid-scale is silence, the
/// top code is `(2^(n-1) - 1) / 2^(n-1)` and zero is -1.
fn offset_binary_reads_by_the_sample_rule<F: Float + PartialEq + std::fmt::Debug>() {
    let f = |x: f64| F::from_f64(x);
    assert_eq!(edge::from_offset::<u8, F>(128), f(0.0));
    assert_eq!(edge::from_offset::<u8, F>(255), f(127.0 / 128.0));
    assert_eq!(edge::from_offset::<u8, F>(0), f(-1.0));
    assert_eq!(edge::from_offset::<u16, F>(32768), f(0.0));
    assert_eq!(edge::from_offset::<u16, F>(49152), f(0.5));
    assert_eq!(edge::from_offset::<u16, F>(65535), f(32767.0 / 32768.0));
    assert_eq!(edge::from_offset::<u32, F>(2_147_483_648), f(0.0));
    assert_eq!(edge::from_offset::<u32, F>(0), f(-1.0));
}

#[test]
fn offset_binary_reads_by_the_sample_rule_in_f32() {
    offset_binary_reads_by_the_sample_rule::<f32>();
}

/// In `f64` the `u32` code `0xC000_0001` (signed `0x4000_0001`) keeps its last
/// bit, which `f32` rounds away.
#[test]
fn offset_binary_reads_by_the_sample_rule_in_f64() {
    offset_binary_reads_by_the_sample_rule::<f64>();
    let want = f64::from((1u32 << 30) + 1) / f64::from(1u32 << 31);
    assert_eq!(edge::from_offset::<u32, f64>(0xC000_0001), want);
    assert_ne!(f64::from(edge::from_offset::<u32, f32>(0xC000_0001)), want);
}

/// The capture callback of an `f64` graph does not allocate either: `i32` and
/// `f64` blocks wider than a chunk fill both rings and then overrun them.
#[test]
fn the_f64_capture_callback_does_not_allocate() {
    let (mut cap, mut mic, _ring) = capture::<f64>(8, &[2, 3], 256);
    let i32s = vec![1 << 20; 8 * 150];
    let f64s = vec![0.25f64; 8 * 150];
    cap.block(&i32s, edge::from_native::<i32, f64>);
    cap.block(&f64s, edge::from_native::<f64, f64>);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..8 {
            cap.block(&i32s, edge::from_native::<i32, f64>);
            cap.block(&f64s, edge::from_native::<f64, f64>);
        }
    });
    assert!(Source::take_lost(&mut mic).get() > 0);
}
