//! The analysis job's figures: each is the amplitude or the frame the signal
//! was built with, read back through the job.

use std::num::NonZeroUsize;
use std::path::Path;

use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, Interleaved, Mono, Wave};
use tutti_fixtures::signal;
use tutti_measure::{
    ChromaConfig, FiniteLevel, FrequencyBand, Meter, PeakBlocks, PeakConfig, SilenceConfig,
    VectorscopeConfig,
};
use tutti_units::{Amplitude, Db, Hz, SampleRate, Samples, Seconds};

use super::{Analyze, AnalyzeError, BindError};

const CHUNK_FRAMES: usize = super::CHUNK_FRAMES.get();
use crate::{DetachedSession, Job, Session};

const RATE: SampleRate = SampleRate::SR_48K;

fn session() -> DetachedSession {
    Session::builder().build_detached(RATE).unwrap()
}

/// The largest absolute sample the blocks hold.
fn peak_of(blocks: &PeakBlocks) -> f64 {
    blocks
        .as_flat()
        .iter()
        .map(|block| block.peak().get())
        .fold(0.0, f64::max)
}

fn write(path: &Path, rate: SampleRate, channels: u16, interleaved: &[f32]) {
    signal::write_wav_f32(path, rate, channels, interleaved);
}

fn peak_blocks(samples_per_block: usize) -> PeakConfig {
    PeakConfig::new(Samples::new(samples_per_block))
}

fn land<A, S: Float>(
    session: &mut DetachedSession,
    job: &Job<Analyze<A, S>>,
) -> Result<<A::Meter as Meter>::Output, AnalyzeError>
where
    A: tutti_measure::Measure<S> + 'static,
    <A::Meter as Meter>::Output: Send + 'static,
{
    job.wait(session)
}

#[test]
fn a_sine_of_half_amplitude_has_a_peak_of_half_in_memory() {
    let mut s = session();
    // 1 kHz at 48 kHz: 48 samples a cycle, a sample on every crest.
    let samples = signal::sine(
        Hz::new(1000.0),
        Amplitude::new(0.5),
        RATE,
        Samples::new(48_000),
    );
    let wave = Wave::from_mono(RATE, Mono::new(&samples));
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
    let samples = signal::sine(
        Hz::new(1000.0),
        Amplitude::new(0.5),
        RATE,
        Samples::new(48_000),
    );
    write(&path, RATE, 1, &samples);
    let job = s.analyze::<f32, _>(path.as_path(), peak_blocks(512));
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
    let job = s.analyze::<f32, _>(path, peak_blocks(1000));
    let blocks = land(&mut s, &job).unwrap();
    let found: Vec<(usize, f64)> = blocks
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
    let job = s.analyze::<f32, _>(path, peak_blocks(256));
    let blocks = land(&mut s, &job).unwrap();
    let peak = |channel: usize| {
        blocks
            .channel(channel)
            .unwrap()
            .iter()
            .map(|block| block.peak().get())
            .fold(0.0, f64::max)
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
    let band = FrequencyBand::new(Hz::new(100.0), Hz::new(20_000.0)).unwrap();
    let job = s.analyze::<f32, _>(path, ChromaConfig::new().band(band));
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
    let wave = Wave::from_mono(RATE, Mono::new(&[0.0; 64]));
    let job = s.analyze(wave, VectorscopeConfig::new(NonZeroUsize::MIN));
    let failed = land(&mut s, &job).unwrap_err();
    assert!(
        matches!(
            failed,
            AnalyzeError::Bind(BindError::Width { needs, found })
                if needs == tutti_buffers::ChannelLayout::STEREO
                    && found == tutti_buffers::ChannelLayout::MONO
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
    let job = s.analyze::<f32, _>(path, peak_blocks(512));
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
        wave.append(Interleaved::new(&[level, level], ChannelLayout::STEREO).unwrap())
            .unwrap();
    }
    let config = SilenceConfig::new(FiniteLevel::of(Db::new(-40.0)), Seconds::new(0.01));
    let job = s.analyze(wave, config);
    let regions = land(&mut s, &job).unwrap();
    let spans: Vec<_> = regions
        .iter()
        .map(|region| (region.start().get(), region.end().get()))
        .collect();
    assert_eq!(spans, [(1000, 2000)]);
}

/// An analysis that counts the chunks its meter is pushed.
struct Counting(std::sync::Arc<std::sync::atomic::AtomicUsize>);

struct CountingMeter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl tutti_measure::Measure<f32> for Counting {
    type Context = ();
    type Meter = CountingMeter;

    fn measure(&self, (): ()) -> tutti_measure::Result<CountingMeter> {
        Ok(CountingMeter(self.0.clone()))
    }
}

impl Meter for CountingMeter {
    type Input<'a> = tutti_buffers::Mono<'a>;
    type Item = ();
    type Output = usize;

    fn push(&mut self, _: Self::Input<'_>, _: &mut impl Extend<()>) -> tutti_measure::Result<()> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn finish(self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[test]
fn a_job_dropped_before_its_worker_starts_pushes_no_chunk_and_lands_nothing() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let held = tutti_fixtures::HeldFutures::new();
    let mut s = Session::builder()
        .executor(held.executor())
        .build_detached(RATE)
        .unwrap();
    let pushed = Arc::new(AtomicUsize::new(0));
    // Ten chunks long: a worker that ignored the drop would push ten.
    let wave = Wave::from_mono(RATE, Mono::new(&vec![0.0; 10 * CHUNK_FRAMES]));
    let job = s.analyze(wave, Counting(Arc::clone(&pushed)));
    drop(job);
    held.run_all();
    assert_eq!(pushed.load(Ordering::SeqCst), 0);
    let landed = s
        .update()
        .iter()
        .filter(|event| matches!(event, crate::SessionEvent::JobDone(_)))
        .count();
    assert_eq!(landed, 0);
}

#[test]
fn a_job_that_is_not_dropped_pushes_every_chunk() {
    let mut s = session();
    let pushed = std::sync::Arc::default();
    let wave = Wave::from_mono(RATE, Mono::new(&vec![0.0; 2 * CHUNK_FRAMES + 1]));
    let job = s.analyze(wave, Counting(std::sync::Arc::clone(&pushed)));
    assert_eq!(land(&mut s, &job).unwrap(), 3);
}

/// An analysis that reads a stereo pair as planes and keeps each channel's peak.
struct ChannelPeaks;

struct ChannelPeaksMeter([f32; 2]);

impl tutti_measure::Measure<f32> for ChannelPeaks {
    type Context = ();
    type Meter = ChannelPeaksMeter;

    fn measure(&self, (): ()) -> tutti_measure::Result<ChannelPeaksMeter> {
        Ok(ChannelPeaksMeter([0.0; 2]))
    }
}

impl Meter for ChannelPeaksMeter {
    type Input<'a> = tutti_buffers::StereoPlanes<'a>;
    type Item = ();
    type Output = [f32; 2];

    fn push(
        &mut self,
        input: Self::Input<'_>,
        _: &mut impl Extend<()>,
    ) -> tutti_measure::Result<()> {
        for (peak, channel) in self.0.iter_mut().zip(0..2) {
            let plane = input.plane(channel);
            *peak = plane.iter().fold(*peak, |m, x| m.max(x.abs()));
        }
        Ok(())
    }

    fn finish(self) -> [f32; 2] {
        self.0
    }
}

#[test]
fn a_stereo_wave_reads_each_channels_peak_through_its_own_planes() {
    let mut s = session();
    let mut wave = Wave::new(2, RATE);
    for frame in 0..(2 * CHUNK_FRAMES + 5) {
        let (left, right) = match frame {
            3 => (0.5, 0.0),
            f if f == CHUNK_FRAMES + 9 => (0.0, -0.25),
            _ => (0.0, 0.0),
        };
        wave.append(Interleaved::new(&[left, right], ChannelLayout::STEREO).unwrap())
            .unwrap();
    }
    let job = s.analyze(wave, ChannelPeaks);
    assert_eq!(land(&mut s, &job).unwrap(), [0.5, 0.25]);
}

#[cfg(codec)]
#[test]
fn a_stereo_file_reads_each_channels_peak_through_deinterleaved_planes() {
    let mut s = session();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("planes.wav");
    let mut samples = vec![0.0f32; 2 * (CHUNK_FRAMES + 20)];
    samples[2 * 7] = 0.5;
    samples[2 * (CHUNK_FRAMES + 11) + 1] = -0.25;
    write(&path, RATE, 2, &samples);
    let job = s.analyze(path, ChannelPeaks);
    assert_eq!(land(&mut s, &job).unwrap(), [0.5, 0.25]);
}

/// An analysis in the audio's own format: the largest absolute sample of a
/// mono read, widened to `f64` without rounding.
struct MaxAbs;

struct MaxAbsMeter<S: Float>(S);

impl<S: Float> tutti_measure::Measure<S> for MaxAbs {
    type Context = ();
    type Meter = MaxAbsMeter<S>;

    fn measure(&self, (): ()) -> tutti_measure::Result<MaxAbsMeter<S>> {
        Ok(MaxAbsMeter(S::SILENCE))
    }
}

impl<S: Float> Meter for MaxAbsMeter<S> {
    type Input<'a> = Mono<'a, S>;
    type Item = ();
    type Output = f64;

    fn push(
        &mut self,
        input: Self::Input<'_>,
        _: &mut impl Extend<()>,
    ) -> tutti_measure::Result<()> {
        for &x in input.samples() {
            let x = if x < S::SILENCE { -x } else { x };
            if x > self.0 {
                self.0 = x;
            }
        }
        Ok(())
    }

    fn finish(self) -> f64 {
        self.0.to_f64()
    }
}

#[test]
fn an_f64_wave_reports_a_value_f32_cannot_hold_exactly_through_an_f64_measure() {
    let mut s = session();
    // 1 + 2^-40 is exact in f64 and rounds to 1.0 in f32.
    let value = 1.0 + 2.0f64.powi(-40);
    assert_ne!(f64::from(value as f32), value);
    let wave: Wave<f64> = Wave::from_mono(RATE, Mono::new(&[0.0, value, -0.5]));
    let job = s.analyze(wave, MaxAbs);
    assert_eq!(job.wait(&mut s).unwrap(), value);
}

#[cfg(codec)]
#[test]
fn a_full_scale_i16_file_decoded_into_f32_peaks_at_32767_over_32768() {
    let mut s = session();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("full_scale.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for x in [0, i16::MAX, -1000, 0] {
        w.write_sample(x).unwrap();
    }
    w.finalize().unwrap();
    let job = s.analyze::<f32, _>(path, MaxAbs);
    assert_eq!(job.wait(&mut s).unwrap(), 32767.0 / 32768.0);
}
