//! Render tutti-dsp's filters and the export resampler over a matrix of settings,
//! for independent judging in Python (`verify_dsp.py`).
//!
//! The pattern is the sampler harness's: this half writes audio (impulse responses
//! and test tones as float WAVs) and a `cases.json` of the *parameters* each was
//! made with; the Python half re-derives the expected response from the published
//! definition (the RBJ Audio EQ Cookbook, Butterworth and Linkwitz-Riley
//! prototypes, the Moog ladder's small-signal model, the first-order all-pass) with
//! scipy and numpy, which know nothing of tutti. Neither side reads the other's
//! expectation.
//!
//! Only tutti-dsp's public API is used: a design's model and its bank, as a node drives them.
//!
//! Run: `cargo run --release -p tutti-verify --bin render_dsp_cases -- <outdir>`

use std::f64::consts::TAU;
use std::fmt::Write as _;
use std::path::Path;

use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{
    ChunkSize, CrossoverBank, CrossoverControls, CrossoverModel, LadderBank, LadderBlend,
    LadderControls, LadderModel, LadderType, PhaserChain, Ramp, SimdLanes, Stages,
    StreamingResampler, StreamingResamplerConfig, SvfBank, SvfCoefficients, SvfControls, SvfModel,
    SvfType, allpass_complement,
};
use tutti_fixtures::signal::write_wav_f32;
use tutti_units::{Db, Drive, Hz, Q, Resonance, SampleRate};

const RATE: f64 = 48_000.0;
/// Impulse-response length: long enough that every case below has rung out.
const IR_FRAMES: usize = 1 << 16;

/// Runs `input` (mono in) through `process` in the float `F`, a block of 512 frames at a time,
/// into `outputs` planes; the planes leave as `f32` for the WAV.
fn blocks<F: Float>(
    input: &[f32],
    outputs: usize,
    mut process: impl FnMut(Planes<'_, F>, PlanesMut<'_, '_, F>),
) -> Vec<Vec<f32>> {
    let input: Vec<F> = input.iter().map(|&x| F::from_f32(x)).collect();
    let mut out = vec![vec![F::ZERO; input.len()]; outputs];
    for start in (0..input.len()).step_by(512) {
        let end = (start + 512).min(input.len());
        let ins = [&input[start..end]];
        let mut outs: Vec<&mut [F]> = out.iter_mut().map(|c| &mut c[start..end]).collect();
        process(
            Planes::new(&ins).expect("one plane"),
            PlanesMut::new(&mut outs).expect("equal planes"),
        );
    }
    out.iter()
        .map(|plane| plane.iter().map(|&y| y.to_f32()).collect())
        .collect()
}

/// Runs an SVF in the float `F`, held at `controls`, over `input` in blocks of 512 frames.
fn run_svf<F: Float + SimdLanes>(
    controls: &SvfControls,
    rate: SampleRate,
    input: &[f32],
) -> Vec<Vec<f32>> {
    let mut bank = SvfBank::<F>::new(ChannelLayout::MONO);
    let mut coefficients = Ramp::new(SvfCoefficients::from(&SvfModel::new(*controls, rate)));
    blocks::<F>(input, 1, |x, y| bank.process(&mut coefficients, x, y))
}

fn impulse(amplitude: f32) -> Vec<f32> {
    let mut x = vec![0.0f32; IR_FRAMES];
    x[0] = amplitude;
    x
}

/// Planes to one interleaved buffer.
fn interleave(planes: &[Vec<f32>]) -> Vec<f32> {
    let n = planes[0].len();
    (0..n)
        .flat_map(|i| planes.iter().map(move |p| p[i]))
        .collect()
}

fn wav(dir: &Path, name: &str, rate: f64, planes: &[Vec<f32>]) {
    write_wav_f32(
        &dir.join(format!("{name}.wav")),
        SampleRate::of(rate),
        planes.len() as u16,
        &interleave(planes),
    );
}

#[derive(Default)]
struct Cases(Vec<String>);

impl Cases {
    fn push(&mut self, name: &str, kind: &str, rate: f64, params: &str) {
        self.0.push(format!(
            r#"  {{"name": "{name}", "kind": "{kind}", "rate": {rate}, "file": "{name}.wav", {params}}}"#
        ));
    }
}

fn svf(dir: &Path, cases: &mut Cases) {
    let rate = SampleRate::of(RATE);
    let settings: [(SvfType, &str, f64, f64, Db); 24] = [
        (SvfType::LowPass, "lowpass", 200.0, 0.707, Db::UNITY),
        (SvfType::LowPass, "lowpass", 1_000.0, 2.0, Db::UNITY),
        (SvfType::LowPass, "lowpass", 8_000.0, 0.5, Db::UNITY),
        (SvfType::HighPass, "highpass", 200.0, 0.707, Db::UNITY),
        (SvfType::HighPass, "highpass", 1_000.0, 2.0, Db::UNITY),
        (SvfType::HighPass, "highpass", 8_000.0, 0.5, Db::UNITY),
        (SvfType::BandPass, "bandpass", 200.0, 0.707, Db::UNITY),
        (SvfType::BandPass, "bandpass", 1_000.0, 4.0, Db::UNITY),
        (SvfType::BandPass, "bandpass", 8_000.0, 1.0, Db::UNITY),
        (SvfType::Notch, "notch", 200.0, 0.707, Db::UNITY),
        (SvfType::Notch, "notch", 1_000.0, 4.0, Db::UNITY),
        (SvfType::Notch, "notch", 8_000.0, 1.0, Db::UNITY),
        (SvfType::Allpass, "allpass", 1_000.0, 0.707, Db::UNITY),
        (SvfType::Allpass, "allpass", 4_000.0, 3.0, Db::UNITY),
        (SvfType::Bell, "bell", 1_000.0, 1.0, Db::new(9.0)),
        (SvfType::Bell, "bell", 250.0, 0.707, Db::new(-6.0)),
        (SvfType::Bell, "bell", 6_000.0, 3.0, Db::new(12.0)),
        (SvfType::LowShelf, "lowshelf", 200.0, 0.707, Db::new(9.0)),
        (SvfType::LowShelf, "lowshelf", 1_000.0, 1.0, Db::new(-12.0)),
        (SvfType::LowShelf, "lowshelf", 4_000.0, 0.5, Db::new(6.0)),
        (SvfType::HighShelf, "highshelf", 500.0, 0.707, Db::new(-9.0)),
        (SvfType::HighShelf, "highshelf", 3_000.0, 1.0, Db::new(12.0)),
        (SvfType::HighShelf, "highshelf", 8_000.0, 0.5, Db::new(-6.0)),
        (
            SvfType::HighShelf,
            "highshelf",
            12_000.0,
            0.707,
            Db::new(6.0),
        ),
    ];
    for (i, (ty, label, freq, q, level)) in settings.into_iter().enumerate() {
        let gain = level.get();
        let c = SvfControls {
            ty,
            freq: Hz::new(freq),
            q: Q::new(q),
            gain: level,
        };
        // Every setting in f64; the first of each type again in f32 state.
        let name = format!("svf_f64_{label}_{freq}_{q}_{gain}_{i}");
        wav(dir, &name, RATE, &run_svf::<f64>(&c, rate, &impulse(1.0)));
        cases.push(&name, "svf", RATE, &format!(r#""type": "{label}", "freq": {freq}, "q": {q}, "gain_db": {gain}, "precision": "f64""#));
        if i % 3 == 1 {
            let name = format!("svf_f32_{label}_{freq}_{q}_{gain}_{i}");
            wav(dir, &name, RATE, &run_svf::<f32>(&c, rate, &impulse(1.0)));
            cases.push(&name, "svf", RATE, &format!(r#""type": "{label}", "freq": {freq}, "q": {q}, "gain_db": {gain}, "precision": "f32""#));
        }
    }
}

fn ladder(dir: &Path, cases: &mut Cases) {
    let rate = SampleRate::of(RATE);
    // The impulse is small so the saturator (tanh) stays in its linear region:
    // the case judges the small-signal response, and the Python half divides the
    // amplitude out.
    let amplitude = 1e-4f32;
    for (ty, label) in [(LadderType::LP24, "lp24")] {
        for (freq, res) in [
            (500.0f64, 0.0f64),
            (500.0, 0.3),
            (4_000.0, 0.0),
            (4_000.0, 0.5),
            (1_000.0, 0.9),
        ] {
            let c = LadderControls {
                ty,
                freq: Hz::new(freq),
                res: Resonance::new(res),
                drive: Drive::UNITY,
            };
            let name = format!("ladder_{label}_{freq}_{res}");
            let mut bank = LadderBank::<f64>::new(ChannelLayout::MONO);
            let mut blend = Ramp::new(LadderBlend::from(&LadderModel::new(c, rate)));
            let out = blocks::<f64>(&impulse(amplitude), 1, |x, y| {
                bank.process(&mut blend, x, y);
            });
            wav(dir, &name, RATE, &out);
            cases.push(
                &name,
                "ladder",
                RATE,
                &format!(
                    r#""type": "{label}", "freq": {freq}, "res": {res}, "amplitude": {amplitude:e}"#
                ),
            );
        }
    }
}

fn crossover(dir: &Path, cases: &mut Cases) {
    let rate = SampleRate::of(RATE);
    for (name, splits) in [
        ("lr4_2band", vec![1_000.0f64]),
        ("lr4_3band", vec![300.0, 3_000.0]),
    ] {
        let hz: Vec<Hz> = splits.iter().map(|&f| Hz::new(f)).collect();
        let c = CrossoverControls::new(&hz, rate).expect("controls");
        let model = CrossoverModel::designed(splits.len() + 1, &c, rate).expect("bands");
        let mut bank = CrossoverBank::<f64>::new(ChannelLayout::MONO);
        let mut coefficients = Ramp::new(model.blend());
        // One channel per band, band 0 first.
        let out = blocks::<f64>(&impulse(1.0), model.bands(), |x, y| {
            bank.process(&mut coefficients, x, y);
        });
        wav(dir, name, RATE, &out);
        cases.push(name, "crossover", RATE, &format!(r#""splits": {splits:?}"#));
    }
}

fn phaser(dir: &Path, cases: &mut Cases) {
    for stages in [2usize, 4] {
        for fc in [300.0f64, 2_000.0, 10_000.0] {
            let c = allpass_complement(Hz::new(fc), SampleRate::of(RATE));
            let mut chain = PhaserChain::new(Stages::of(stages), ChannelLayout::MONO);
            let x = impulse(1.0);
            let mut y = vec![0.0f32; IR_FRAMES];
            chain.process(
                &mut [Ramp::new(c)],
                &mut Ramp::new(0.0),
                Planes::new(&[&x[..]]).expect("one plane"),
                PlanesMut::new(&mut [&mut y[..]]).expect("one plane"),
            );
            let name = format!("allpass_chain_{stages}x_{fc}");
            wav(dir, &name, RATE, &[y]);
            cases.push(
                &name,
                "allpass_chain",
                RATE,
                &format!(r#""stages": {stages}, "freq": {fc}"#),
            );
        }
    }
}

/// A sum of unit sines at integer hertz, random-looking but fixed phases.
fn multisine(rate: f64, secs: f64, freqs: &[f64], amplitude: f64) -> Vec<f32> {
    let n = (rate * secs) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            let s: f64 = freqs
                .iter()
                .enumerate()
                .map(|(k, f)| (TAU * f * t + 2.399963 * k as f64 * k as f64).sin())
                .sum();
            (s * amplitude / freqs.len() as f64) as f32
        })
        .collect()
}

fn resample(dir: &Path, cases: &mut Cases) {
    for (from, to) in [(44_100.0f64, 48_000.0f64), (48_000.0, 44_100.0)] {
        let mut jobs: Vec<(String, Vec<f64>)> = Vec::new();
        // Passband: 40 integer-hertz tones, log spaced from 40 Hz to just under the
        // lower Nyquist. One file, so the ripple is read across the whole band.
        let top = 0.5 * from.min(to);
        let mut pass: Vec<f64> = (0..40)
            .map(|i| (40.0 * (0.99 * top / 40.0).powf(i as f64 / 39.0)).round())
            .collect();
        pass.dedup();
        jobs.push(("passband".into(), pass));
        // The band edge: tones every 150 Hz up to 150 Hz under the Nyquist.
        jobs.push((
            "edge".into(),
            (0..17).map(|i| 19_500.0 + 150.0 * i as f64).collect(),
        ));
        // Stopband: single tones above the target's Nyquist (only a downsample has
        // any; an upsample's are images, judged by the same tones in the passband).
        if to < from {
            for f in [22_500.0, 23_500.0, 24_000.0 - 1.0] {
                jobs.push((format!("stop_{f}"), vec![f]));
            }
        }
        for (label, freqs) in jobs {
            let secs = 1.0;
            let input = multisine(from, secs, &freqs, 0.5);
            let (mut rs, out) = (
                StreamingResampler::try_from(
                    StreamingResamplerConfig::new(
                        ChannelLayout::MONO,
                        SampleRate::of(from),
                        SampleRate::of(to),
                    )
                    .chunk(ChunkSize::N1024),
                )
                .expect("resampler"),
                &mut vec![Vec::<f32>::new()],
            );
            for block in input.chunks(4_096) {
                let planes = [block.to_vec()];
                rs.push(Planes::from_vecs(&planes).expect("planes"), out)
                    .expect("push");
            }
            rs.finish(out).expect("finish");
            let name = format!("resample_{from}_{to}_{label}");
            wav(dir, &format!("{name}_in"), from, &[input]);
            wav(dir, &format!("{name}_out"), to, &[out[0].clone()]);
            let list = freqs
                .iter()
                .map(|f| format!("{f}"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut p = String::new();
            let _ = write!(
                p,
                r#""from": {from}, "to": {to}, "label": "{label}", "freqs": [{list}], "file_in": "{name}_in.wav", "file_out": "{name}_out.wav""#
            );
            cases.0.push(format!(
                r#"  {{"name": "{name}", "kind": "resample", "rate": {to}, "file": "{name}_out.wav", {p}}}"#
            ));
        }
    }
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: render_dsp_cases <outdir>");
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).expect("create the output directory");
    let mut cases = Cases::default();
    svf(dir, &mut cases);
    ladder(dir, &mut cases);
    crossover(dir, &mut cases);
    phaser(dir, &mut cases);
    resample(dir, &mut cases);
    let json = format!("[\n{}\n]\n", cases.0.join(",\n"));
    std::fs::write(dir.join("cases.json"), json).expect("write cases.json");
    println!("wrote {} cases to {}", cases.0.len(), dir.display());
}
