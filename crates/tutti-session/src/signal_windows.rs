//! From a silence scan to the timeline: each stretch of signal becomes the
//! window a voice plays.

use tutti_measure::Region;
use tutti_sampler::VoiceWindow;
use tutti_tempo::TempoView;
use tutti_units::{Frame, Samples};

/// The windows a strip-silence scan leaves: one [`VoiceWindow`] per signal
/// region, so a voice plays what lies between the silences.
///
/// A window starts at the beat `tempo` gives its region's `start` and lasts
/// the beats from there to the beat of its `end`. The regions are
/// [`strip_silence`](tutti_measure::strip_silence)'s, whose result already is
/// the signal between silences; a scan that found no signal gives no windows.
#[must_use]
pub fn windows_from_signal(regions: &[Region], tempo: &mut TempoView<'_>) -> Vec<VoiceWindow> {
    let mut beat_at = |at: Samples| tempo.beat_at_frame(Frame::ZERO + at);
    regions
        .iter()
        .map(|region| {
            let start = beat_at(region.start());
            VoiceWindow::span(start, beat_at(region.end()) - start)
        })
        .collect()
}

#[cfg(test)]
mod tests;
