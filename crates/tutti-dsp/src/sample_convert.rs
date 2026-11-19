//! [`convert_equal`]: a format conversion between two slices of one length.

use tutti_buffers::{Mono, MonoMut, Sample, convert_mono};

/// `dst = src` in `dst`'s format, through [`tutti_buffers::convert_mono`]. Allocates nothing.
///
/// # Panics
///
/// If the two slices differ in length; every caller passes two of one length by construction.
pub(crate) fn convert_equal<A: Sample, B: Sample>(src: &[A], dst: &mut [B]) {
    let done = convert_mono(Mono::new(src), MonoMut::new(dst));
    assert!(
        done.is_ok(),
        "BUG: a conversion between equal lengths: {done:?}"
    );
}
