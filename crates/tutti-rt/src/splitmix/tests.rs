use super::SplitMix64;

/// The first five outputs of the published generator from seed 0 (Vigna's
/// reference `splitmix64.c`).
const SEED_0: [u64; 5] = [
    0xE220_A839_7B1D_CDAF,
    0x6E78_9E6A_A1B9_65F4,
    0x06C4_5D18_8009_454F,
    0xF88B_B8A8_724C_81EC,
    0x1B39_896A_51A8_749B,
];

/// **The stream from seed 0 is the published one.**
#[test]
fn the_stream_from_seed_zero_is_the_published_outputs() {
    let mut g = SplitMix64::new(0);
    for (n, want) in SEED_0.into_iter().enumerate() {
        assert_eq!(g.next_u64(), want, "output {}", n + 1);
    }
}

/// **The `n`-th output is the mix of the state `n` increments on**, so a
/// counter-based reader lands on the stream's values without walking it.
#[test]
fn the_nth_output_is_the_mix_of_n_increments() {
    for (n, want) in SEED_0.into_iter().enumerate() {
        assert_eq!(
            SplitMix64::mix((n as u64).wrapping_mul(SplitMix64::GAMMA)),
            want
        );
    }
}

/// **A 32-bit word is the high half of the 64-bit one.**
#[test]
fn a_32_bit_word_is_the_high_half() {
    let mut g = SplitMix64::new(0);
    assert_eq!(g.next_u32(), (SEED_0[0] >> 32) as u32);
    assert_eq!(g.next_u32(), (SEED_0[1] >> 32) as u32);
}
