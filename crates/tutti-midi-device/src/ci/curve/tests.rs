use tutti_ump::ControlValue;

use super::super::*;

fn gain(raw: u32) -> f32 {
    volume_gain(ControlValue::new(raw)).get()
}

#[test]
fn full_scale_is_0_db_and_zero_is_silence() {
    // M2-125 Table 6, M2-121 Table 8: 0xFFFFFFFF is 0 dB, 0 is -infinity.
    assert_eq!(gain(0xFFFF_FFFF), 0.0);
    assert_eq!(gain(0), f32::NEG_INFINITY);
}

#[test]
fn the_formula_is_40_log10_of_the_fraction_of_full_scale() {
    // The formula of M2-125 §5.1 at powers of two: 40 * log10(2^-n), to the
    // 5 digits 20 * log10(2) = 6.0206 gives.
    for (raw, want) in [
        (0x8000_0000, -12.0412),
        (0x4000_0000, -24.0824),
        (0x2000_0000, -36.1236),
    ] {
        assert!((gain(raw) - want).abs() < 1e-3, "{raw:#x}: {}", gain(raw));
    }
}

#[test]
fn the_printed_figures_of_the_table_are_within_two_tenths_of_a_decibel() {
    // M2-125 Table 6 and M2-121 Table 8 print -4.9, -11.9, -23.9, -36.0 dB;
    // the first is the formula's to the printed digit, the other three are
    // 0.14, 0.18 and 0.12 dB above it.
    assert!((gain(0xC104_1041) - -4.9).abs() < 0.05);
    for (raw, printed) in [
        (0x8000_0000, -11.9),
        (0x4000_0000, -23.9),
        (0x2000_0000, -36.0),
    ] {
        assert!((gain(raw) - printed).abs() < 0.2, "{raw:#x}");
    }
}

#[test]
fn a_send_level_is_linear_in_amplitude_and_100_percent_at_full_scale() {
    // M2-125 §5.2: linear with respect to amplitude, 100% at 0xFFFFFFFF.
    let level = |raw| send_level(ControlValue::new(raw)).get();
    assert_eq!(level(0xFFFF_FFFF), 1.0);
    assert_eq!(level(0), 0.0);
    assert!((level(0x4000_0000) - 0.25).abs() < 1e-6);
    assert!((level(0x8000_0000) - 0.5).abs() < 1e-6);
}
