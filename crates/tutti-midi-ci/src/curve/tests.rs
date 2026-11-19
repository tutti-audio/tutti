use tutti_ump::ControlValue;

use super::super::*;

fn gain(raw: u32) -> f64 {
    volume_gain(ControlValue::new(raw)).get()
}

#[test]
fn full_scale_is_0_db_and_zero_is_silence() {
    // M2-125 Table 6, M2-121 Table 8: 0xFFFFFFFF is 0 dB, 0 is -infinity.
    assert_eq!(gain(0xFFFF_FFFF), 0.0);
    assert_eq!(gain(0), f64::NEG_INFINITY);
}

#[test]
fn the_formula_is_40_log10_of_the_fraction_of_full_scale() {
    // M2-125 §5.1 at the 32-bit values of Table 6's rows: 40 * log10(v /
    // 0xFFFFFFFF), the first from 0.75394 and the others from powers of two
    // (20 * log10(2) = 6.0206).
    for (raw, want) in [
        (0xC104_1041, -4.9059),
        (0x8000_0000, -12.0412),
        (0x4000_0000, -24.0824),
        (0x2000_0000, -36.1236),
    ] {
        assert!((gain(raw) - want).abs() < 1e-3, "{raw:#x}: {}", gain(raw));
    }
}

/// Spec: m2-125/5.2#1, m2-125/6.1.8#1
#[test]
fn a_send_level_is_linear_in_amplitude_and_100_percent_at_full_scale() {
    // M2-125 §5.2: linear with respect to amplitude, 100% at 0xFFFFFFFF.
    let level = |raw| send_level(ControlValue::new(raw)).get();
    assert_eq!(level(0xFFFF_FFFF), 1.0);
    assert_eq!(level(0), 0.0);
    let full = f64::from(u32::MAX);
    assert_eq!(level(0x4000_0000), f64::from(0x4000_0000_u32) / full);
    assert_eq!(level(0x8000_0000), f64::from(0x8000_0000_u32) / full);
}
