//! The reverbs', the phase vocoder's and the drift corrector's audio-thread paths allocate nothing:
//! the banks' `retarget`, `process` and `tail_of`, the network design in place, the vocoder's
//! `retarget`, `input_needed` and `process`, and the variable resampler's `input_needed` and
//! `process` with its fill controller's `observe`.
//!
//! The crate carries no `assert_no_alloc` dev-dependency,
//! so this binary counts allocations with its own global allocator, per thread.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

use tutti_fixtures::{ThreadCounting, allocations};
use tutti_units::{Mix, SampleRate, Seconds};

#[global_allocator]
static A: ThreadCounting = ThreadCounting;

/// Freeverb's `retarget`, `process` and `tail_of` allocate nothing, with a different room every
/// block (hostile controls among them), after the one `FreeverbBank::new` that sizes it.
#[test]
fn freeverb_retargets_and_renders_without_allocating() {
    use tutti_buffers::{InputOutput, Planes, PlanesMut};
    use tutti_dsp::{FreeverbBank, FreeverbControls};

    for rate in [44_100.0, 48_000.0, 192_000.0] {
        let mut bank = FreeverbBank::<f32>::new(SampleRate::of(rate));
        let mut ramps = bank.ramps();
        let input = vec![0.25f32; 64];
        let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
        for block in 0..64u32 {
            let c = FreeverbControls {
                room: Mix::new([f64::from(block % 8) / 7.0, f64::NAN, 9.0][(block % 3) as usize]),
                damp: Mix::new(
                    [f64::from((block / 8) % 8) / 7.0, f64::INFINITY, -2.0]
                        [(block % 5 % 3) as usize],
                ),
            };
            let n = allocations(|| {
                bank.retarget(&mut ramps, &c);
                bank.process(
                    &mut ramps,
                    &mut InputOutput::paired(
                        Planes::new(&[&input]).unwrap(),
                        PlanesMut::new(&mut [&mut left, &mut right]).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
                std::hint::black_box(bank.tail_of(&c));
                std::hint::black_box(bank.tail());
            });
            assert_eq!(n, 0, "the block allocated at {rate} Hz");
        }
    }
}

/// The FDN bank's `retarget` and `process` allocate nothing, with a different network every block
/// (a decay, a size, a diffusion, a pre-delay, a width, a wet gain and a freeze all move),
/// after the one `FdnBank::new` that sizes it. The ramps the ramps start are the work of the
/// following blocks, which allocate nothing either.
#[test]
fn the_fdn_retargets_and_renders_without_allocating() {
    use tutti_buffers::{InputOutput, Planes, PlanesMut};
    use tutti_dsp::{FdnBank, FdnControls, FdnLayout, FreezeControls, MaxDecay};
    use tutti_units::{Bipolar, Db, Hz, StereoWidth};

    let rate = SampleRate::SR_48K;
    let layout = FdnLayout::new(8, MaxDecay::DEFAULT).unwrap();
    let mut bank = FdnBank::<f32, FdnLayout>::new(&layout, rate);
    let mut ramps = bank.ramps();
    let input = vec![0.25f32; 64];
    let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
    for block in 0..200u32 {
        let k = f64::from(block % 7) / 6.0;
        let c = FdnControls {
            decay: [
                Seconds::new(0.2 + 7.0 * k),
                Seconds::new(1.0 + k),
                Seconds::new(0.5 + 3.0 * k),
            ],
            size: Mix::new(k),
            diffusion: Mix::new(1.0 - k),
            pre_delay: Seconds::new(0.05 * k),
            modulation: Bipolar::of(k),
            rate: Hz::new(0.1 + k),
            width: StereoWidth::new(2.0 * k),
            wet: Db::new(-12.0 * k),
            ..FdnControls::DEFAULT
        };
        let freeze = FreezeControls {
            freeze: block % 11 == 0,
        };
        let n = allocations(|| {
            bank.retarget(&mut ramps, &c, &freeze);
            bank.process(
                &mut ramps,
                &mut InputOutput::paired(
                    Planes::new(&[&input]).unwrap(),
                    PlanesMut::new(&mut [&mut left, &mut right]).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
            std::hint::black_box(bank.tail_of(&ramps, &c, &freeze));
        });
        assert_eq!(n, 0, "the block allocated");
    }
}

/// The FDN's `design_into` runs on a 140 KiB stack: a network is 113 KiB (`MAX_STAGES` of the
/// largest stage), with 27 KiB spare for the frames, and building one on an
/// audio thread's callback must not copy it. An unoptimised build copies a network about
/// three times, so building one needs well over that.
#[test]
fn the_fdns_design_into_needs_no_network_sized_stack() {
    use tutti_dsp::{FdnControls, FdnLayout, FreezeControls};
    let rate = SampleRate::SR_44K1;
    let mut network = Box::new(tutti_dsp::Network::new(rate));
    let handle = std::thread::Builder::new()
        .stack_size(140 * 1024)
        .spawn(move || {
            FdnLayout::default().design_into(
                &FdnControls::DEFAULT,
                &FreezeControls::DEFAULT,
                rate,
                &mut network,
            );
            network.stages.len()
        })
        .unwrap();
    assert_eq!(handle.join().unwrap(), 1);
}

/// The phase vocoder's `retarget`, `input_needed`, `process`, `seat` and `discard` allocate
/// nothing, with every control moving each block (hostile ones among them) and every block fed
/// what it asks, after the one `PhaseVocoder::new` that sizes it.
#[test]
fn the_phase_vocoder_retargets_and_renders_without_allocating() {
    use tutti_buffers::{ChannelLayout, Planes, PlanesMut};
    use tutti_dsp::{
        ColaGeometry, HopSize, PhaseVocoder, PhaseVocoderControls, StftGeometry, WindowLength,
    };
    use tutti_units::{Cents, Samples, StretchFactor};

    let geometry = ColaGeometry::try_from(StftGeometry::new(
        SampleRate::SR_48K,
        WindowLength::new(Samples::new(1024)).unwrap(),
        HopSize::new(Samples::new(256)).unwrap(),
    ))
    .unwrap();
    let mut vocoder = PhaseVocoder::<f32>::new(ChannelLayout::STEREO, geometry);
    let source: Vec<f32> = (0..8192).map(|n| (n as f32 * 0.05).sin()).collect();
    let (mut left, mut right) = (vec![0.0f32; 300], vec![0.0f32; 300]);
    for block in 0..96u32 {
        let c = PhaseVocoderControls {
            stretch: StretchFactor::new([1.0, 0.3, 3.7, f64::NAN][(block % 4) as usize]),
            pitch: Cents::new([0.0, -2400.0, 1200.0, f64::INFINITY][(block / 4 % 4) as usize]),
        };
        let n = allocations(|| {
            vocoder.retarget(&c);
            if block % 24 == 0 {
                // A seat, and its first frame taken without synthesis.
                vocoder.seat();
                let history = &source[..1024];
                std::hint::black_box(
                    vocoder.discard(Planes::new(&[history, history]).unwrap(), Samples::new(1)),
                );
            }
            let needed = vocoder
                .input_needed(Samples::new(300))
                .get()
                .min(source.len());
            let counts = vocoder.process(
                Planes::new(&[&source[..needed], &source[..needed]]).unwrap(),
                PlanesMut::new(&mut [&mut left, &mut right]).unwrap(),
            );
            std::hint::black_box(counts);
        });
        assert_eq!(n, 0, "block {block} allocated");
    }
}

/// The drift corrector's audio-thread path allocates nothing: the controller's `observe`,
/// the resampler's `input_needed` and `process`, with a different ratio every block and
/// windows that straddle the history, after the one `VariableResampler::new` that builds the
/// table.
#[test]
fn the_variable_resampler_and_its_controller_allocate_nothing() {
    use tutti_buffers::{ChannelLayout, Planes, PlanesMut};
    use tutti_dsp::{FillController, VariableResampler};
    use tutti_units::Samples;

    let mut resampler = VariableResampler::<f32>::new(ChannelLayout::STEREO);
    let mut controller = FillController::new(Samples::new(512), SampleRate::SR_48K);
    let input = vec![0.25f32; 700];
    let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
    for block in 0..64usize {
        let n = allocations(|| {
            let ratio = controller.observe(Samples::new(500 + block), Samples::new(64));
            let needed = resampler.input_needed(Samples::new(64), ratio).get();
            let planes = [&input[..needed], &input[..needed]];
            let consumed = resampler
                .process(
                    Planes::new(&planes).unwrap(),
                    PlanesMut::new(&mut [&mut left, &mut right]).unwrap(),
                    ratio,
                )
                .unwrap();
            std::hint::black_box(consumed);
        });
        assert_eq!(n, 0, "block {block} allocated");
    }
}
