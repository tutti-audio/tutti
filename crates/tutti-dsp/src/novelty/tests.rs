use super::*;
use crate::Complex64;

#[test]
fn detection_functions_respond_to_what_they_are_for() {
    let loud = vec![Complex64::new(1.0, 0.0); 16];
    let previous = vec![0.1f64; 16];

    // Flux counts growth only.
    assert!(spectral_flux(&previous, &loud) > 0.0);
    let falling = vec![Complex64::new(0.01, 0.0); 16];
    assert_eq!(spectral_flux(&previous, &falling), 0.0);

    // Complex-domain counts change in either direction.
    assert!(complex_domain_deviation(&previous, &falling) > 0.0);

    // HFC weights high bins more than low ones.
    let mut low = vec![Complex64::default(); 16];
    low[1] = Complex64::new(1.0, 0.0);
    let mut high = vec![Complex64::default(); 16];
    high[15] = Complex64::new(1.0, 0.0);
    assert!(high_frequency_content(&high) > high_frequency_content(&low));

    assert!(spectral_energy(Mono::new(&[1.0f64; 4])) > spectral_energy(Mono::new(&[0.1f64; 4])));
}
