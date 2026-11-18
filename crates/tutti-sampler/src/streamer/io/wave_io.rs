//! The read half of the streamer's cold-path disk refill.
//!
//! The streamer moves frames from a resident (whole-file) [`Wave`] into a
//! per-region ring buffer. This module owns the source end: [`read_wave`] is the one
//! place the planar unpack lives, zero-padded past the end. A loop is not its
//! business: the refill cuts its reads at the loop's end and places each run
//! (`loops::Mapping::fill`), so a read always runs the file straight on.
//!
//! Both ends speak flat interleaved `&[f32]` at a runtime width, with every
//! count denominated in frames. The sink end is
//! [`RegionOut::push_interleaved`](crate::streamer::prefetch::RegionOut::push_interleaved).

use crate::nonempty;
use tutti_buffers::{Float, InterleavedMut, Wave, interleave};
use tutti_types::{ChannelLayout, Samples};

/// Read file frames `at..` of a resident `wave` into `out` (`channels` wide,
/// floored at one), zero past its end: the whole-file source a region without
/// a decoder reads, and the loop's reads of a resident file.
///
/// The wave is read as it stands, so it is as wide as `channels`: `false`
/// (with `out` silent) for a wave of another width, which is the ring's bug,
/// never a fold.
///
/// It fills the whole buffer, so it is not a [`Source`](tutti_buffers::Source):
/// it never ends and has no verdict to give. The caller owns the bound, by the
/// size of the scratch buffer it hands in.
pub(crate) fn read_wave<F: Float>(
    wave: &Wave<F>,
    at: usize,
    out: &mut [F],
    channels: ChannelLayout,
) -> bool {
    let channels = nonempty(channels);
    let frames = channels.from_interleaved_len(out.len());
    let whole = channels.interleaved_len(frames);
    out.fill(F::ZERO);
    if wave.layout() != channels {
        return false;
    }
    let have = wave.len().get().saturating_sub(at).min(frames.get());
    let from = Samples(at.min(wave.len().get()));
    let planes = wave.planes().slice(from..Samples(from.get() + have));
    let head = &mut out[..channels.interleaved_len(Samples(have)).min(whole)];
    InterleavedMut::new(head, channels)
        .and_then(|dst| interleave(planes, dst))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::Interleaved;
    use tutti_types::{SampleRate, Seconds};

    fn test_wave(samples: &[(f32, f32)]) -> Wave {
        let mut wave = Wave::new(2, SampleRate::SR_48K);
        for &(l, r) in samples {
            wave.append(Interleaved::new(&[l, r], wave.layout()).unwrap())
                .unwrap();
        }
        wave
    }

    fn indexed_wave(channels: usize, len: usize) -> Wave {
        let mut w = Wave::zero(
            channels,
            SampleRate::SR_48K,
            Seconds::new(len as f64) / 48000.0,
        );
        for i in 0..w.len().get() {
            for c in 0..channels {
                w.set(c, i, (c + 1) as f32);
            }
        }
        w
    }

    #[test]
    fn read_wave_polls_forward_frames() {
        let wave = test_wave(&[(0.1, 0.1), (0.2, 0.2), (0.3, 0.3)]);
        let mut out = [0.0f32; 6];
        read_wave(&wave, 0, &mut out, ChannelLayout::STEREO);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn read_wave_zero_pads_past_end() {
        let wave = test_wave(&[(0.2, 0.2)]);
        let mut out = [9.0f32; 4];
        read_wave(&wave, 1, &mut out, ChannelLayout::STEREO);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn read_wave_reads_six_channels() {
        let wave = indexed_wave(6, 4);
        let mut out = [0.0f32; 12];
        read_wave(&wave, 0, &mut out, ChannelLayout::from(6u16));
        for f in 0..2 {
            for c in 0..6 {
                assert_eq!(out[f * 6 + c], (c + 1) as f32, "frame {f} channel {c}");
            }
        }
    }
}
