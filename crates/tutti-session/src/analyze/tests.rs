//! The analysis job's figures: each is the amplitude or the frame the signal
//! was built with, read back through the job.

use std::num::NonZeroUsize;
use std::path::Path;

use tutti_codec::Wave;
use tutti_fixtures::signal;
use tutti_measure::{
    ChromaConfig, FiniteLevel, FrequencyBand, Meter, PeakBlocks, PeakConfig, SilenceConfig,
    VectorscopeConfig,
};
use tutti_types::{Amplitude, Db, Hz, SampleRate, Samples, Seconds};

use super::{Analyze, AnalyzeError, CHUNK_FRAMES};
use crate::{DetachedSession, Job, Session};

const RATE: SampleRate = SampleRate::SR_48K;

fn session() -> DetachedSession {
    Session::builder().build_detached(RATE).unwrap()
}

/// The largest absolute sample the blocks hold.
fn peak_of(blocks: &PeakBlocks) -> f32 {
    blocks
        .as_flat()
        .iter()
        .map(|block| block.peak().get())
        .fold(0.0, f32::max)
}

fn write(path: &Path, rate: SampleRate, channels: u16, interleaved: &[f32]) {
    signal::write_wav_f32(path, rate, channels, interleaved);
}

fn peak_blocks(samples_per_block: usize) -> PeakConfig {
    PeakConfig::new(Samples(samples_per_block))
}

fn land<A>(
    session: &mut DetachedSession,
    job: &Job<Analyze<A>>,
) -> Result<<A::Meter as Meter>::Output, AnalyzeError>
where
    A: tutti_measure::Measure + 'static,
    <A::Meter as Meter>::Output: Send + 'static,
{
    job.wait(session)
}

#[test]
fn a_sine_of_half_amplitude_has_a_peak_of_half_in_memory() {
    let mut s = session();
    // 1 kHz at 48 kHz: 48 samples a cycle, a sample on every crest.
    let samples = signal::sine(Hz(1000.0), Amplitude(0.5), RATE, Samples(48_000));
    let wave = Wave::from_samples(RATE, &samples);
    let job = s.analyze(wave, peak_blocks(512));
    let blocks = land(&mut s, &job).unwrap();
    assert!((peak_of(&blocks) - 0.5).abs() < 1e-6);
}

#[cfg(codec)]
#[test]
fn a_sine_of_half_amplitude_has_a_peak_of_half_streamed_from_a_file() {
    let mut s = session();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sine.wav");
    let samples = signal::sine(Hz(1000.0), Amplitude(0.5), RATE, Samples(48_000));
    write(&path, RATE, 1, &samples);
    let job = s.analyze(path.as_path(), peak_blocks(512));
    let blocks = land(&mut s, &job).unwrap();
    assert!((peak_of(&blocks) - 0.5).abs() < 1e-6);
    // 48 000 frames in blocks of 512: 93 whole blocks and a part.
    assert_eq!(blocks.blocks_per_channel(), 94);
}

#[cfg(codec)]
#[test]
fn a_peak_past_the_first_chunks_is_found_in_its_block_across_chunk_edges() {
    let mut s = session();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clicks.wav");
    // Blocks of 1000 frames straddle every chunk edge (16 384 is no multiple).
    let (early, late) = (CHUNK_FRAMES + 500, 3 * CHUNK_FRAMES - 100);
    let mut samples = vec![0.0f32; 3 * CHUNK_FRAMES + 700];
    samples[early] = 0.25;
    samples[late] = -0.75;
    write(&path, RATE, 1, &samples);
    let job = s.analyze(path, peak_blocks(1000));
    let blocks = land(&mut s, &job).unwrap();
    let found: Vec<(usize, f32)> = blocks
        .channel(0)
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, block)| block.peak().get() > 0.0)
        .map(|(index, block)| (index, block.peak().get()))
        .collect();
    assert_eq!(found, [(early / 1000, 0.25), (late / 1000, 0.75)]);
}

#[cfg(codec)]
#[test]
fn a_stereo_file_keeps_each_channels_peak() {
    let mut s = session();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stereo.wav");
    let frames = 2 * CHUNK_FRAMES + 10;
    let mut samples = vec![0.0f32; frames * 2];
    samples[2 * (CHUNK_FRAMES + 3)] = 0.5;
    samples[2 * (frames - 1) + 1] = 0.125;
    write(&path, RATE, 2, &samples);
    let job = s.analyze(path, peak_blocks(256));
    let blocks = land(&mut s, &job).unwrap();
    let peak = |channel: usize| {
        blocks
            .channel(channel)
            .unwrap()
            .iter()
            .map(|block| block.peak().get())
            .fold(0.0, f32::max)
    };
    assert_eq!((peak(0), peak(1)), (0.5, 0.125));
}

#[cfg(codec)]
#[test]
fn a_band_above_the_files_nyquist_fails_the_job_with_the_meters_error() {
    let mut s = session();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("narrow.wav");
    let rate = SampleRate::SR_8K;
    write(&path, rate, 1, &vec![0.0; 8_000]);
    let band = FrequencyBand::new(Hz(100.0), Hz(20_000.0)).unwrap();
    let job = s.analyze(path, ChromaConfig::new().band(band));
    let failed = land(&mut s, &job).unwrap_err();
    assert!(
        matches!(
            failed,
            AnalyzeError::Measure(tutti_measure::Error::AboveNyquist { .. })
        ),
        "{failed:?}"
    );
}

#[test]
fn a_stereo_analysis_of_mono_audio_fails_with_the_widths() {
    let mut s = session();
    let wave = Wave::from_samples(RATE, &[0.0; 64]);
    let job = s.analyze(wave, VectorscopeConfig::new(NonZeroUsize::MIN));
    let failed = land(&mut s, &job).unwrap_err();
    assert!(
        matches!(
            failed,
            AnalyzeError::Width { needs, found }
                if needs == tutti_types::ChannelLayout::STEREO
                    && found == tutti_types::ChannelLayout::MONO
        ),
        "{failed:?}"
    );
}

#[cfg(codec)]
#[test]
fn a_file_that_cannot_be_opened_fails_with_the_decode_error() {
    let mut s = session();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.wav");
    let job = s.analyze(path, peak_blocks(512));
    let failed = land(&mut s, &job).unwrap_err();
    assert!(matches!(failed, AnalyzeError::Decode(_)), "{failed:?}");
}

#[test]
fn a_mono_analysis_of_stereo_audio_reads_the_fold_of_its_channels() {
    let mut s = session();
    // Equal loud channels over frames 1000..2000 of 40 000: whatever the fold's
    // gain, the folded signal is loud there and nowhere else.
    let mut wave = Wave::new(2, RATE);
    for frame in 0..40_000 {
        let level = if (1000..2000).contains(&frame) {
            0.5
        } else {
            0.0
        };
        wave.push_frame(&[level, level]);
    }
    let config = SilenceConfig::new(FiniteLevel::of(Db(-40.0)), Seconds(0.01));
    let job = s.analyze(wave, config);
    let regions = land(&mut s, &job).unwrap();
    let spans: Vec<_> = regions
        .iter()
        .map(|region| (region.start().get(), region.end().get()))
        .collect();
    assert_eq!(spans, [(1000, 2000)]);
}
