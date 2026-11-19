// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_buffers::Float;

#[non_exhaustive]
pub(crate) struct ArrayMath {}

impl ArrayMath {
    pub(crate) fn multiply_add<F: Float>(a: F, x: &[F], destination: &mut [F]) {
        for (x, destination) in x.iter().zip(destination.iter_mut()) {
            *destination += a * *x;
        }
    }

    pub(crate) fn multiply_add_slope<F: Float>(a: F, step: F, x: &[F], destination: &mut [F]) {
        let mut a = a;
        for (x, destination) in x.iter().zip(destination.iter_mut()) {
            *destination += a * *x;
            a += step;
        }
    }
}
