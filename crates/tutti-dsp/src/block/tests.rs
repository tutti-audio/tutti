use super::*;
use crate::{
    BitCrusher, BitCrusherControls, BrickwallBank, BrickwallSignals, CombBank, CombForm,
    CompressorBank, CompressorSignals, CrossoverBands, CrossoverBank, CrossoverControls,
    CrossoverModel, DcBlocker, GateBank, GateSignals, LadderBank, LadderBlend, LadderControls,
    LadderModel, MidSide, OscillatorWave, Polarity, Ramp, RingModulator, SvfBank, SvfCoefficients,
    SvfControls, SvfModel, Tremolo,
};
use crate::{
    DelayBank, DelayLayout, DelayRamps, DelaySignals, EarlyReflections, FreeverbBank,
    FrequencyShifterBank, GAIN_RAMP, KarplusStrongBank, LowestPitch, MaxDelay, PhaserChain,
    SpringLayout, SpringReverb, Stages, TapSet,
};
use core::num::NonZeroUsize;
use tutti_buffers::{
    BlockSide, BufferError, BusTopology, ChannelLayout, InputOutput, Planes, PlanesMut,
};
use tutti_units::{Feedback, Hz, SampleRate, Seconds};

const RATE: SampleRate = SampleRate::SR_48K;
const FRAMES: usize = 8;
/// What the output holds before a call: a processor that wrote nothing leaves it.
const UNTOUCHED: f32 = 7.0;

/// `process` over a block of `channels_in` planes of ones in and `channels_out` planes of
/// [`UNTOUCHED`] out: its result, and the output it left behind.
fn run(
    channels_in: usize,
    channels_out: usize,
    process: impl FnOnce(&mut InputOutput<'_, f32>) -> Result<(), BlockMismatch>,
) -> (Result<(), BlockMismatch>, Vec<Vec<f32>>) {
    let input = vec![vec![1.0f32; FRAMES]; channels_in];
    let mut output = vec![vec![UNTOUCHED; FRAMES]; channels_out];
    let result = {
        let mut block = InputOutput::paired(
            Planes::from_vecs(&input).unwrap(),
            PlanesMut::from_vecs(&mut output).unwrap(),
        )
        .unwrap();
        process(&mut block)
    };
    (result, output)
}

/// A bank of `width` channels refuses a block of another width on either side before it
/// writes a sample, and takes one of its width. `process` builds the bank afresh for each call.
fn refuses_another_width(
    width: usize,
    process: impl Fn(&mut InputOutput<'_, f32>) -> Result<(), BlockMismatch>,
) {
    let untouched = |out: &[Vec<f32>]| out.iter().flatten().all(|&y| y == UNTOUCHED);
    let (result, out) = run(width - 1, width, &process);
    assert_eq!(
        result,
        Err(BlockMismatch::Width {
            side: BlockSide::Input,
            expected: width,
            found: width - 1
        })
    );
    assert!(untouched(&out), "a refused block is not processed in part");
    let (result, out) = run(width, width + 1, &process);
    assert_eq!(
        result,
        Err(BlockMismatch::Width {
            side: BlockSide::Output,
            expected: width,
            found: width + 1
        })
    );
    assert!(untouched(&out), "a refused block is not processed in part");
    let (result, out) = run(width, width, &process);
    assert_eq!(result, Ok(()));
    assert!(
        out.iter().flatten().all(|&y| y != UNTOUCHED),
        "a block of the bank's width is written whole"
    );
}

/// A pair of views at two frame counts is no block: the input and the output of every bank
/// agree in frames because [`InputOutput`] refuses the pair.
fn frames_cannot_differ() {
    let input = vec![vec![0.0f32; FRAMES]];
    let mut output = vec![vec![0.0f32; FRAMES + 1]];
    let err = InputOutput::paired(
        Planes::from_vecs(&input).unwrap(),
        PlanesMut::from_vecs(&mut output).unwrap(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            BufferError::BlockLengthMismatch {
                side: BlockSide::Output,
                ..
            }
        ),
        "{err:?}"
    );
}

/// Every filter bank is built for a width; its block is exactly that wide, and a block of
/// another width is refused whole, the crossover's output counting its bands.
#[test]
fn filter_banks_refuse_a_block_of_another_width() {
    frames_cannot_differ();
    refuses_another_width(2, |block| {
        let model = SvfModel::new(SvfControls::DEFAULT, RATE);
        let mut coefficients = Ramp::new(SvfCoefficients::from(&model));
        SvfBank::<f32>::new(ChannelLayout::STEREO).process(&mut coefficients, block)
    });
    refuses_another_width(2, |block| {
        let model = LadderModel::new(LadderControls::DEFAULT, RATE);
        let mut blend = Ramp::new(LadderBlend::from(&model));
        LadderBank::<f32>::new(ChannelLayout::STEREO).process(&mut blend, block)
    });
    // Two channels in, three bands of two out.
    let splits = [Hz::new(300.0), Hz::new(3_000.0)];
    let controls = CrossoverControls::try_from((&splits[..], RATE)).unwrap();
    let model = CrossoverModel::designed(CrossoverBands::new(3).unwrap(), &controls, RATE);
    let process = |block: &mut InputOutput<'_, f32>| {
        let mut blend = Ramp::new(model.blend());
        CrossoverBank::<f32>::new(ChannelLayout::STEREO).process(&mut blend, block)
    };
    let (result, out) = run(2, 5, process);
    assert_eq!(
        result,
        Err(BlockMismatch::Width {
            side: BlockSide::Output,
            expected: 6,
            found: 5
        })
    );
    assert!(out.iter().flatten().all(|&y| y == UNTOUCHED));
    let (result, out) = run(2, 6, process);
    assert_eq!(result, Ok(()));
    assert!(out.iter().flatten().all(|&y| y != UNTOUCHED));
}

/// A dynamics bank of any width reads and writes the same channels; a block with more or
/// fewer out than in is refused whole, and a key at another frame count is refused.
#[test]
fn dynamics_banks_refuse_a_block_of_unequal_width_and_a_key_of_other_frames() {
    frames_cannot_differ();
    let (result, out) = run(2, 1, |block| {
        BrickwallBank::<f32>::new().process(BrickwallSignals::default(), block)
    });
    assert_eq!(
        result,
        Err(BlockMismatch::Width {
            side: BlockSide::Output,
            expected: 2,
            found: 1
        })
    );
    assert!(out.iter().flatten().all(|&y| y == UNTOUCHED));
    let (result, _) = run(2, 2, |block| {
        BrickwallBank::<f32>::new().process(BrickwallSignals::default(), block)
    });
    assert_eq!(result, Ok(()));

    // The key is a third view: it must hold the block's frames.
    let key = vec![vec![0.0f32; FRAMES + 1]];
    let (result, out) = run(1, 1, |block| {
        GateBank::<f32>::new(RATE).process(
            GateSignals::default(),
            Planes::from_vecs(&key).unwrap(),
            block,
        )
    });
    assert_eq!(
        result,
        Err(BlockMismatch::KeyFrames {
            block: tutti_units::Samples::new(FRAMES),
            found: tutti_units::Samples::new(FRAMES + 1),
        })
    );
    assert!(out.iter().flatten().all(|&y| y == UNTOUCHED));
    let (result, _) = run(1, 1, |block| {
        let own = block.input();
        CompressorBank::<f32>::new(RATE).process(CompressorSignals::default(), own, block)
    });
    assert_eq!(result, Ok(()));
}

/// The utility banks fixed at a width refuse a block of another one; a stereo pair is
/// exactly two channels in and out.
#[test]
fn utility_banks_refuse_a_block_of_another_width() {
    frames_cannot_differ();
    refuses_another_width(2, |block| {
        DcBlocker::<f32>::new(ChannelLayout::STEREO, RATE).process(block)
    });
    refuses_another_width(2, |block| {
        CombBank::<f32>::new(
            CombForm::Feedforward,
            NonZeroUsize::new(3).unwrap(),
            Feedback::new(0.5),
            ChannelLayout::STEREO,
        )
        .process(block)
    });
    refuses_another_width(2, |block| {
        BitCrusher::<f32>::new(ChannelLayout::STEREO, BitCrusherControls::DEFAULT).process(block)
    });
    refuses_another_width(2, |block| {
        Polarity::new(ChannelLayout::STEREO, [])
            .unwrap()
            .process(block)
    });
    refuses_another_width(2, |block| MidSide::Encode.process(block));
    // A tremolo or ring modulator has no width of its own: the output is as wide as the input.
    let (result, out) = run(2, 1, |block| {
        Tremolo::new(OscillatorWave::Sine, RATE).process(block)
    });
    assert_eq!(
        result,
        Err(BlockMismatch::Width {
            side: BlockSide::Output,
            expected: 2,
            found: 1
        })
    );
    assert!(out.iter().flatten().all(|&y| y == UNTOUCHED));
    let (result, _) = run(2, 3, |block| RingModulator::new(RATE).process(block));
    assert!(result.is_err());
}

/// The delay-line banks (a delay, an early-reflection stage) are built for their widths: the
/// delay takes its lines' channels on both sides, and the stage the channels its taps feed.
#[test]
fn delay_banks_refuse_a_block_of_another_width() {
    frames_cannot_differ();
    refuses_another_width(2, |block| {
        let layout = DelayLayout::new(BusTopology::STEREO, MaxDelay::of(Seconds::new(1.0)));
        let steps = u32::try_from(GAIN_RAMP.to_samples_ceil(RATE).get()).unwrap();
        DelayBank::<f32>::new(&layout, RATE).process(
            &mut DelayRamps::new(steps),
            DelaySignals::default(),
            block,
        )
    });
    // A stage with no taps outputs one channel, whatever the input's width.
    let stage = EarlyReflections::<f32>::try_from(&TapSet::default()).unwrap();
    assert_eq!(stage.channels(), 1);
    let (result, out) = run(2, 2, |block| stage.clone().process(block));
    assert_eq!(
        result,
        Err(BlockMismatch::Width {
            side: BlockSide::Output,
            expected: 1,
            found: 2
        })
    );
    assert!(out.iter().flatten().all(|&y| y == UNTOUCHED));
    assert_eq!(run(2, 1, |block| stage.clone().process(block)).0, Ok(()));
}

/// The modulation banks refuse a block of another width: a frequency shifter and a phaser
/// chain are built for one.
#[test]
fn modulation_banks_refuse_a_block_of_another_width() {
    frames_cannot_differ();
    refuses_another_width(2, |block| {
        FrequencyShifterBank::<f32>::new(ChannelLayout::STEREO, RATE).process(block)
    });
    refuses_another_width(2, |block| {
        let mut complements = [Ramp::new(0.5f32), Ramp::new(0.5f32)];
        PhaserChain::<f32>::new(Stages::of(2), tutti_buffers::BusTopology::STEREO).process(
            &mut complements,
            &mut Ramp::new(0.0),
            block,
        )
    });
}

/// A reverb's wet signal is a pair, or as wide as the spring's channels: another output width
/// is refused, whatever the input's.
#[test]
fn reverb_banks_refuse_a_block_of_another_width() {
    frames_cannot_differ();
    for input in [1, 2, 3] {
        let (result, out) = run(input, 1, |block| {
            let mut bank = FreeverbBank::<f32>::new(RATE);
            let mut ramps = bank.ramps();
            bank.process(&mut ramps, block)
        });
        assert_eq!(
            result,
            Err(BlockMismatch::Width {
                side: BlockSide::Output,
                expected: 2,
                found: 1
            })
        );
        assert!(out.iter().flatten().all(|&y| y == UNTOUCHED));
    }
    assert_eq!(
        run(3, 2, |block| {
            let mut bank = FreeverbBank::<f32>::new(RATE);
            let mut ramps = bank.ramps();
            bank.process(&mut ramps, block)
        })
        .0,
        Ok(())
    );
    refuses_another_width(2, |block| {
        SpringReverb::<f32>::new(ChannelLayout::STEREO, RATE, SpringLayout::default())
            .process(block)
    });
}

/// The string and mode banks are built for a width and refuse a block of another.
#[test]
fn synthesis_banks_refuse_a_block_of_another_width() {
    frames_cannot_differ();
    refuses_another_width(2, |block| {
        KarplusStrongBank::<f32>::new(
            LowestPitch::new(Hz::new(50.0)).unwrap(),
            RATE,
            ChannelLayout::STEREO,
        )
        .process(block)
    });
}
