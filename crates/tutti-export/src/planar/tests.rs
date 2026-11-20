use super::append;
use crate::RenderError;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{BufferError, Interleaved};

/// A block of another width than the planes is refused, and no plane grows.
#[test]
fn append_refuses_a_width_mismatch_and_appends_nothing() {
    let samples = [1.0f32, 2.0, 3.0, 4.0];
    let block = Interleaved::new(&samples, ChannelLayout::from(2u16)).unwrap();
    let mut planes = vec![Vec::new(), Vec::new(), Vec::new()];
    let err = append(block, &mut planes).unwrap_err();
    assert!(matches!(
        err,
        RenderError::Buffer(BufferError::WidthMismatch { .. })
    ));
    assert!(planes.iter().all(Vec::is_empty));
}

/// A block of the planes' width extends each plane by its channel.
#[test]
fn append_extends_each_plane_by_its_channel() {
    let samples = [1.0f32, 2.0, 3.0, 4.0];
    let block = Interleaved::new(&samples, ChannelLayout::from(2u16)).unwrap();
    let mut planes = vec![vec![0.5], vec![0.25]];
    append(block, &mut planes).unwrap();
    assert_eq!(planes, vec![vec![0.5, 1.0, 3.0], vec![0.25, 2.0, 4.0]]);
}
