use super::*;
use crate::fold_uneven;

/// A layout wider than a bus is metered whole: every channel of a 24-channel buffer has
/// its blocks, indexed by its position.
#[test]
fn a_layout_wider_than_a_bus_meters_every_channel() {
    let wide = ChannelLayout::from(24u16);
    let samples: Vec<f64> = (0..4 * 24).map(|i| (i % 24) as f64 / 24.0).collect();
    let blocks = summarize(&mono(4), Interleaved::new(&samples, wide).unwrap());
    for c in 0..24 {
        assert_eq!(ch(&blocks, c).len(), 1, "channel {c}");
        assert_eq!(ch(&blocks, c)[0].max, c as f64 / 24.0, "channel {c}");
    }
}

/// `summarize` of a buffer that is valid for its config.
fn summarize(cfg: &PeakConfig, buffer: Interleaved<'_, f64>) -> PeakBlocks {
    super::summarize(cfg, buffer).unwrap()
}

fn mono(block: usize) -> PeakConfig {
    PeakConfig::new(Samples::new(block))
}

/// Channel `ch`'s blocks, for tests that know the channel exists.
fn ch(blocks: &PeakBlocks, ch: usize) -> &[PeakBlock] {
    blocks.channel(ch).expect("channel is within the layout")
}

/// Stream `samples` in `chunk`-frame pieces at `layout`. The streaming
/// path, wrapped so a test states only what it varies.
fn stream(cfg: &PeakConfig, layout: ChannelLayout, samples: &[f64], chunk: usize) -> PeakBlocks {
    let mut meter = cfg.measure(layout).unwrap();
    for part in samples.chunks(chunk * usize::from(layout.count())) {
        meter
            .push(Interleaved::new(part, layout).unwrap(), &mut Vec::new())
            .unwrap();
    }
    meter.finish()
}

#[test]
fn block_values_are_exact_for_a_ramp() {
    let samples: Vec<f64> = (0..500).map(|i| i as f64).collect();
    let blocks = summarize(
        &mono(100),
        Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
    );

    assert_eq!(blocks.blocks_per_channel(), 5);
    for (i, block) in ch(&blocks, 0).iter().enumerate() {
        let lo = (i * 100) as f64;
        assert_eq!(block.min, lo);
        assert_eq!(block.max, lo + 99.0);
        assert_eq!(block.peak(), Amplitude::new(lo + 99.0));
    }
}

/// The law: streaming in arbitrary chunks equals one batch call.
///
/// Swept across **every layout**, not just mono: at `ChannelLayout::MONO`
/// a frame is one sample, so a bug that discards audio on non-frame-aligned
/// multichannel chunks would go unseen.
///
/// Chunk sizes are mostly coprime with both the block size and the channel
/// counts, so nearly every chunk ends mid-frame *and* mid-block.
#[test]
fn streaming_matches_batch_at_every_chunk_size_and_layout() {
    for layout in [
        ChannelLayout::MONO,
        ChannelLayout::STEREO,
        ChannelLayout::QUAD,
        ChannelLayout::from(6u16),
    ] {
        let channels = layout.count() as usize;
        // 500 whole frames, so batch and streaming see the same input.
        let samples: Vec<f64> = (0..500 * channels).map(|i| i as f64).collect();

        for block in [1usize, 3, 100] {
            let cfg = PeakConfig::new(Samples::new(block));
            let batch = summarize(&cfg, Interleaved::new(&samples, layout).unwrap());

            for chunk in [1usize, 3, 7, 33, 100, 250, 333, 501, 1024] {
                let streamed = stream(&cfg, layout, &samples, chunk);

                assert_eq!(
                    streamed, batch,
                    "{layout:?}, block {block}, chunk {chunk} disagrees with batch"
                );
            }
        }
    }
}

/// A chunk whose own width disagrees with the config's is refused: the
/// pending blocks are `cfg.layout` wide.
#[test]
fn a_chunk_of_the_wrong_width_is_refused() {
    let cfg = PeakConfig::new(Samples::new(1));
    let mut meter = cfg.measure(ChannelLayout::STEREO).unwrap();

    assert_eq!(
        meter.push(
            Interleaved::new(&[1.0f64, 2.0, 3.0, 4.0], ChannelLayout::QUAD).unwrap(),
            &mut Vec::new()
        ),
        Err(MeterError::ChunkLayoutMismatch {
            chunk: ChannelLayout::QUAD,
            config: ChannelLayout::STEREO,
        })
    );
    assert!(
        meter.finish().is_empty(),
        "a quad chunk must not be blocked as stereo"
    );
}

#[test]
fn a_trailing_partial_block_is_kept() {
    let samples: Vec<f64> = (0..250).map(|i| i as f64).collect();
    let blocks = summarize(
        &mono(100),
        Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
    );

    assert_eq!(blocks.blocks_per_channel(), 3);
    assert_eq!(ch(&blocks, 0)[2].min, 200.0);
    assert_eq!(ch(&blocks, 0)[2].max, 249.0, "only the 50 samples present");
}

/// A ragged final block must be kept for **every** channel, not just the
/// first — the per-channel form of the trailing-block rule.
#[test]
fn a_trailing_partial_block_is_kept_on_every_channel() {
    // 250 stereo frames at block 100: two whole blocks and a 50-frame tail.
    let samples: Vec<f64> = (0..250).flat_map(|i| [i as f64, -(i as f64)]).collect();
    let blocks = summarize(
        &PeakConfig::new(Samples::new(100)),
        Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
    );

    assert_eq!(blocks.blocks_per_channel(), 3);
    assert_eq!(ch(&blocks, 0)[2].max, 249.0);
    assert_eq!(ch(&blocks, 1)[2].min, -249.0);
}

/// **Why blocking is per channel.**
///
/// A phase-inverted pair is the worst case: folding the samples before
/// blocking cancels it to a flat line, so the clip renders as silence and
/// is not. Per channel, both channels are a full-scale ramp.
#[test]
fn a_phase_inverted_pair_does_not_cancel() {
    // L ramps up, R is its exact negation.
    let samples: Vec<f64> = (0..200).flat_map(|i| [i as f64, -(i as f64)]).collect();
    let blocks = summarize(
        &PeakConfig::new(Samples::new(100)),
        Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
    );

    assert_eq!(blocks.blocks_per_channel(), 2, "frames, not samples");
    assert_eq!(blocks.layout(), ChannelLayout::STEREO);

    // Neither channel is silent, and they are opposite.
    assert_eq!(ch(&blocks, 0)[0].min, 0.0);
    assert_eq!(ch(&blocks, 0)[0].max, 99.0);
    assert_eq!(ch(&blocks, 1)[0].min, -99.0);
    assert_eq!(ch(&blocks, 1)[0].max, 0.0);

    // And the reduction keeps the envelope rather than cancelling it —
    // this is what distinguishes `to_mono` from folding first.
    let merged = blocks.to_mono();
    assert_eq!(merged[0].min, -99.0);
    assert_eq!(merged[0].max, 99.0);
}

/// A hard-panned pair: one channel silent, the other full-scale.
///
/// Folding halved the amplitude, so the clip drew at half height. Per
/// channel the loud side is untouched and the silent side is visibly
/// silent — which is the information a panned double-track needs to show.
#[test]
fn a_hard_panned_pair_keeps_the_loud_channel_at_full_scale() {
    // L silent, R full-scale square.
    let samples: Vec<f64> = (0..200)
        .flat_map(|i| [0.0, if i % 2 == 0 { 1.0 } else { -1.0 }])
        .collect();
    let blocks = summarize(
        &PeakConfig::new(Samples::new(100)),
        Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
    );

    assert_eq!(ch(&blocks, 0)[0].min, 0.0, "silent channel is silent");
    assert_eq!(ch(&blocks, 0)[0].max, 0.0);
    assert_eq!(ch(&blocks, 1)[0].peak(), Amplitude::new(1.0), "not halved");
}

/// Every channel is blocked, including ones the downmix matrices weight to
/// zero — a surround waveform shows what is in each channel, not what a
/// stereo listener would hear.
#[test]
fn surround_keeps_every_channel_separately() {
    // 5.1 with only the centre non-zero.
    let layout = ChannelLayout::from(6u16);
    let samples: Vec<f64> = (0..100)
        .flat_map(|_| [0.0, 0.0, 1.0, 0.0, 0.0, 0.0])
        .collect();
    let blocks = summarize(
        &PeakConfig::new(Samples::new(50)),
        Interleaved::new(&samples, layout).unwrap(),
    );

    assert_eq!(blocks.blocks_per_channel(), 2);
    assert_eq!(blocks.layout(), layout);
    assert_eq!(ch(&blocks, 2)[0].max, 1.0, "centre, at full scale");
    for silent in [0, 1, 3, 4, 5] {
        assert_eq!(
            ch(&blocks, silent)[0].max,
            0.0,
            "channel {silent} must stay silent, not borrow the centre"
        );
    }
    assert!(blocks.channel(6).is_none(), "no seventh channel");
}

/// On mono input the two semantics must coincide — the one case where
/// `to_mono` reproduces the per-channel output exactly.
#[test]
fn to_mono_on_mono_input_is_the_channel_itself() {
    let samples: Vec<f64> = (0..500).map(|i| i as f64).collect();
    let blocks = summarize(
        &mono(100),
        Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
    );

    assert_eq!(blocks.to_mono(), ch(&blocks, 0));
}

/// `as_flat` is channel-major and contiguous — the property the GPU upload
/// path depends on, so it is asserted rather than assumed.
#[test]
fn as_flat_is_channel_major() {
    let samples: Vec<f64> = (0..200).flat_map(|i| [i as f64, -(i as f64)]).collect();
    let blocks = summarize(
        &PeakConfig::new(Samples::new(100)),
        Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
    );

    let flat = blocks.as_flat();
    let per = blocks.blocks_per_channel();
    assert_eq!(flat.len(), per * 2);
    assert_eq!(&flat[..per], ch(&blocks, 0));
    assert_eq!(&flat[per..], ch(&blocks, 1));
}

#[test]
fn degenerate_inputs_are_safe() {
    assert!(
        summarize(
            &mono(100),
            Interleaved::new(&[], ChannelLayout::MONO).unwrap()
        )
        .is_empty()
    );
    // A block of zero samples is named, not silently empty.
    assert_eq!(
        super::summarize(
            &mono(0),
            Interleaved::new(&[1.0f64, 2.0], ChannelLayout::MONO).unwrap()
        ),
        Err(MeterError::ZeroBlock)
    );
    assert_eq!(
        summarize_block(Mono::new(&[] as &[f64])),
        PeakBlock::default()
    );
    let one = summarize_block(Mono::new(&[-0.5f64, 0.25]));
    assert_eq!((one.min, one.max), (-0.5, 0.25));

    // `to_mono` and `channel` on an empty summary answer rather than panic.
    let empty = summarize(
        &mono(100),
        Interleaved::new(&[], ChannelLayout::MONO).unwrap(),
    );
    assert!(empty.to_mono().is_empty());
    assert!(empty.as_flat().is_empty());

    let blocks = Measure::<f64>::measure(&mono(100), ChannelLayout::MONO)
        .unwrap()
        .finish();
    assert!(blocks.is_empty(), "finish on an empty meter emits nothing");
}

/// An empty summary reports **no** channels, not a guessed stereo pair;
/// one a meter finishes with nothing fed keeps the width it measured.
#[test]
fn an_empty_summary_has_no_channels_until_a_width_is_measured() {
    assert_eq!(PeakBlocks::default().layout(), ChannelLayout::EMPTY);
    assert_eq!(PeakBlocks::default().channel(0), None);
    let blocks = Measure::<f64>::measure(&mono(64), ChannelLayout::STEREO)
        .unwrap()
        .finish();
    assert_eq!(blocks.layout(), ChannelLayout::STEREO);
    assert!(blocks.is_empty());
}

/// Every block is exact, including blocks past the first chunk a meter
/// cuts and the short final one.
///
/// A stereo ramp (left `i`, right `-i` at frame `i`) over 20 000 frames
/// with 3 000-frame blocks: 7 blocks, the last a partial one. Block `k` of
/// the left channel spans exactly `[3000k, min(3000k + 2999, 19999)]`, the
/// right its negation.
#[test]
fn summarize_is_exact_across_its_chunks() {
    let frames = 20_000;
    let samples: Vec<f64> = (0..frames).flat_map(|i| [i as f64, -(i as f64)]).collect();
    let cfg = PeakConfig::new(Samples::new(3_000));
    let blocks = summarize(
        &cfg,
        Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
    );
    assert_eq!(blocks.blocks_per_channel(), 7);
    for (k, (l, r)) in ch(&blocks, 0).iter().zip(ch(&blocks, 1)).enumerate() {
        let lo = (k * 3_000) as f64;
        let hi = ((k * 3_000 + 2_999).min(frames - 1)) as f64;
        assert_eq!((l.min, l.max), (lo, hi), "left block {k}");
        assert_eq!((r.min, r.max), (-hi, -lo), "right block {k}");
    }
}

/// An empty buffer still has the view's width.
#[test]
fn summarizing_nothing_keeps_the_layout() {
    let cfg = PeakConfig::new(Samples::new(64));
    let blocks = summarize(&cfg, Interleaved::new(&[], ChannelLayout::STEREO).unwrap());
    assert_eq!(blocks.layout(), ChannelLayout::STEREO);
    assert!(blocks.is_empty());
}

#[test]
fn reset_returns_to_a_fresh_meter() {
    let mut meter = mono(100).measure(ChannelLayout::MONO).unwrap();
    meter
        .push(
            Interleaved::new(&[1.0f64; 150], ChannelLayout::MONO).unwrap(),
            &mut Vec::new(),
        )
        .unwrap();
    meter.reset();
    meter
        .push(
            Interleaved::new(&[2.0f64; 100], ChannelLayout::MONO).unwrap(),
            &mut Vec::new(),
        )
        .unwrap();
    let blocks = meter.finish();

    assert_eq!(
        blocks.blocks_per_channel(),
        1,
        "the carried 50 samples were discarded"
    );
    assert_eq!(ch(&blocks, 0)[0].min, 2.0);
}

/// `reset` must clear every channel's carry, not just the first: a stereo
/// stream reset mid-block would otherwise splice the stale right channel
/// onto the new one.
#[test]
fn reset_clears_every_channel_carry() {
    let mut meter = PeakConfig::new(Samples::new(100))
        .measure(ChannelLayout::STEREO)
        .unwrap();

    // 50 frames: half a block on both channels, nothing emitted.
    let partial: Vec<f64> = (0..50).flat_map(|_| [1.0f64, -1.0]).collect();
    meter
        .push(
            Interleaved::new(&partial, ChannelLayout::STEREO).unwrap(),
            &mut Vec::new(),
        )
        .unwrap();
    meter.reset();

    // A full block of a different value. If either carry survived, its
    // channel would report the earlier value's excursion too.
    let full: Vec<f64> = (0..100).flat_map(|_| [2.0f64, -2.0]).collect();
    meter
        .push(
            Interleaved::new(&full, ChannelLayout::STEREO).unwrap(),
            &mut Vec::new(),
        )
        .unwrap();
    let blocks = meter.finish();

    assert_eq!(blocks.blocks_per_channel(), 1);
    assert_eq!(ch(&blocks, 0)[0].min, 2.0, "left carry survived reset");
    assert_eq!(ch(&blocks, 1)[0].max, -2.0, "right carry survived reset");
}

/// `merge` is the tier-building and channel-reducing primitive, so its
/// range union is exact and its `rms` is the quadratic mean.
#[test]
fn merge_unions_the_range_and_takes_the_quadratic_mean() {
    let a = PeakBlock {
        min: -1.0,
        max: 0.5,
        rms: Amplitude::new(0.0),
    };
    let b = PeakBlock {
        min: -0.25,
        max: 2.0,
        rms: Amplitude::new(1.0),
    };
    let m = a.merge(b);

    assert_eq!(m.min, -1.0, "the lower of the two mins");
    assert_eq!(m.max, 2.0, "the upper of the two maxes");
    // sqrt((0² + 1²) / 2), not (0 + 1) / 2.
    assert!(
        (m.rms.get() - 0.5f64.sqrt()).abs() < 1e-6,
        "got {}",
        m.rms.get()
    );
}

/// The trait fold over uneven chunks (1, 7, 64 frames and the rest) yields
/// every whole block of each channel as an item, in order, and its output
/// is `summarize`: the items are the output less the short final block.
#[test]
fn the_trait_fold_over_uneven_chunks_equals_summarize_with_whole_blocks_as_items() {
    let layout = ChannelLayout::STEREO;
    let samples: Vec<f64> = (0..2 * 250).map(|i| ((i * 37) % 101) as f64).collect();
    let cfg = mono(16);
    let batch = summarize(&cfg, Interleaved::new(&samples, layout).unwrap());
    let (items, blocks) = fold_uneven(&cfg, layout, 250, |meter, at, out| {
        meter.push(
            Interleaved::new(&samples[at.start * 2..at.end * 2], layout).unwrap(),
            out,
        )
    })
    .unwrap();
    assert_eq!(blocks, batch);
    assert_eq!(
        batch.blocks_per_channel(),
        16,
        "15 whole blocks and a short one"
    );
    for channel in 0..2 {
        let streamed: Vec<PeakBlock> = items
            .iter()
            .filter(|item| item.channel.get() == channel)
            .map(|item| item.block)
            .collect();
        assert_eq!(streamed, ch(&batch, channel)[..15], "channel {channel}");
        let frames: Vec<Samples> = items
            .iter()
            .filter(|item| item.channel.get() == channel)
            .map(|item| item.frame)
            .collect();
        let starts: Vec<Samples> = (0..15).map(|i| Samples::new(i * 16)).collect();
        assert_eq!(frames, starts, "each block names the frame it starts at");
    }
}

/// With 3 and 6 channels over 5000 frames (runs that do not divide the
/// conversion block), channel `c` holding the constant `c + 1` summarizes to
/// blocks of exactly `c + 1` in channel `c`: a run cut off a frame boundary
/// moves a sample to another channel and breaks it.
#[test]
fn an_odd_and_a_wide_layout_keep_every_sample_on_its_channel() {
    for channels in [3u16, 6] {
        let layout = ChannelLayout::from(channels);
        let width = usize::from(channels);
        let samples: Vec<f64> = (0..5000 * width).map(|i| (i % width + 1) as f64).collect();
        let blocks = summarize(&mono(100), Interleaved::new(&samples, layout).unwrap());
        assert_eq!(blocks.blocks_per_channel(), 50);
        for c in 0..width {
            let want = (c + 1) as f64;
            assert!(
                ch(&blocks, c)
                    .iter()
                    .all(|b| b.min == want && b.max == want && b.rms.get() == want),
                "channel {c} of {channels}"
            );
        }
    }
}
