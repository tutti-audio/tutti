//! The reverbs' audio-thread paths allocate nothing: the banks' `aim`, `process` and
//! `tail_of`, and the network design in place.
//!
//! The crate carries no `assert_no_alloc` dev-dependency,
//! so this binary counts allocations with its own global allocator, per thread.

use tutti_fixtures::{ThreadCounting, allocations};
use tutti_units::{Mix, SampleRate, Seconds};

#[global_allocator]
static A: ThreadCounting = ThreadCounting;

/// Freeverb's `aim`, `process` and `tail_of` allocate nothing, with a different room every
/// block (hostile controls among them), after the one `FreeverbBank::new` that sizes it.
#[test]
fn freeverb_aims_and_renders_without_allocating() {
    use tutti_buffers::{Planes, PlanesMut};
    use tutti_dsp::{FreeverbBank, FreeverbControls};

    for rate in [44_100.0, 48_000.0, 192_000.0] {
        let mut bank = FreeverbBank::<f32>::new(SampleRate::of(rate));
        let mut glides = bank.glides();
        let input = vec![0.25f32; 64];
        let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
        for block in 0..64u32 {
            let c = FreeverbControls {
                room: Mix([(block % 8) as f32 / 7.0, f32::NAN, 9.0][(block % 3) as usize]),
                damp: Mix(
                    [((block / 8) % 8) as f32 / 7.0, f32::INFINITY, -2.0][(block % 5 % 3) as usize]
                ),
            };
            let n = allocations(|| {
                bank.aim(&mut glides, &c);
                bank.process(
                    &mut glides,
                    Planes::new(&[&input]).unwrap(),
                    PlanesMut::new(&mut [&mut left, &mut right]).unwrap(),
                );
                std::hint::black_box(bank.tail_of(&c));
                std::hint::black_box(bank.tail());
            });
            assert_eq!(n, 0, "the block allocated at {rate} Hz");
        }
    }
}

/// The FDN bank's `aim` and `process` allocate nothing, with a different network every block
/// (a decay, a size, a diffusion, a pre-delay, a width, a wet gain and a freeze all move),
/// after the one `FdnBank::new` that sizes it. The ramps the glides start are the work of the
/// following blocks, which allocate nothing either.
#[test]
fn the_fdn_aims_and_renders_without_allocating() {
    use tutti_buffers::{Planes, PlanesMut};
    use tutti_dsp::{FdnBank, FdnControls, FdnLayout, MaxDecay};
    use tutti_units::{Db, Depth, Hz, StereoWidth};

    let rate = SampleRate::SR_48K;
    let layout = FdnLayout::new(8, MaxDecay::DEFAULT, true).unwrap();
    let mut bank = FdnBank::<f32>::new(&layout, rate);
    let mut glides = bank.glides();
    let input = vec![0.25f32; 64];
    let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
    for block in 0..200u32 {
        let k = (block % 7) as f32 / 6.0;
        let c = FdnControls {
            decay: [
                Seconds::from_f32(0.2 + 7.0 * k),
                Seconds::from_f32(1.0 + k),
                Seconds::from_f32(0.5 + 3.0 * k),
            ],
            size: Mix(k),
            diffusion: Mix(1.0 - k),
            pre_delay: Seconds::from_f32(0.05 * k),
            modulation: Depth::of(k),
            rate: Hz(f64::from(0.1 + k)),
            width: StereoWidth(2.0 * k),
            wet: Db(-12.0 * k),
            freeze: block % 11 == 0,
        };
        let n = allocations(|| {
            bank.aim(&mut glides, &c);
            bank.process(
                &mut glides,
                Planes::new(&[&input]).unwrap(),
                PlanesMut::new(&mut [&mut left, &mut right]).unwrap(),
            );
            std::hint::black_box(bank.tail_of(&glides, &c));
        });
        assert_eq!(n, 0, "the block allocated");
    }
}

/// The FDN's `design_into` runs on a 96 KiB stack: a network is 67 KB, and building one on an
/// audio thread's callback must not copy it. An unoptimised build copies a network about
/// three times, so building one needs well over that.
#[test]
fn the_fdns_design_into_needs_no_network_sized_stack() {
    use tutti_dsp::{FdnControls, FdnLayout};
    let rate = SampleRate::SR_44K1;
    let mut network = Box::new(tutti_dsp::Network::new(rate));
    let handle = std::thread::Builder::new()
        .stack_size(96 * 1024)
        .spawn(move || {
            FdnLayout::default().design_into(&FdnControls::DEFAULT, rate, &mut network);
            network.stages.len()
        })
        .unwrap();
    assert_eq!(handle.join().unwrap(), 1);
}
