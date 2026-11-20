//! A file's own sample format does not reach the graph: an `i16` file and an
//! `f32` file of the same signal stream to the same output.
//!
//! The signal is a square of period 8 frames, left `+0.5` / `-0.5` and right
//! `-0.25` / `+0.25`: values both formats hold exactly (`16384` and `8192` in
//! `i16`), distinct per channel, so a channel in the wrong place or a value
//! off by the conversion's scale is a wrong figure. The expected output is
//! the signal itself, never one render compared with the other.

use std::path::Path;
use tutti_buffers::Planes;
use tutti_graph::contract;
use tutti_sampler::testing::{MockTransport, wait_for_open};
use tutti_sampler::{DiskStreamer, DiskVoice, StreamStart, VoiceWindow};
use tutti_units::{Beat, Bpm, SampleRate};

const SR: u32 = 48_000;
const BLOCK: usize = 64;
const BLOCKS: usize = 8;
const FRAMES: usize = 4_096;

/// Left and right of frame `i`.
fn signal(i: usize) -> (f32, f32) {
    let high = (i / 4).is_multiple_of(2);
    if high { (0.5, -0.25) } else { (-0.5, 0.25) }
}

fn write_i16(path: &Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("writes");
    for i in 0..FRAMES {
        let (l, r) = signal(i);
        w.write_sample((l * 32768.0) as i16).expect("writes");
        w.write_sample((r * 32768.0) as i16).expect("writes");
    }
    w.finalize().expect("writes");
}

fn write_f32(path: &Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("writes");
    for i in 0..FRAMES {
        let (l, r) = signal(i);
        w.write_sample(l).expect("writes");
        w.write_sample(r).expect("writes");
    }
    w.finalize().expect("writes");
}

/// The graph's output for `BLOCKS` blocks of the file at `path` streamed to a
/// voice at beat 0, planar.
fn streamed(path: &Path) -> Vec<Vec<f32>> {
    let rate = SampleRate::of(f64::from(SR));
    let mut streamer = DiskStreamer::manual(rate, Default::default()).expect("builds");
    let opening = streamer.open(path, StreamStart::HEAD);
    let (_stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    assert!(streamer.step_until_idle(1_000) < 1_000, "the ring primes");
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
    let mut voice = contract::prepared(voice, rate, BLOCK);
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut out = vec![Vec::new(), Vec::new()];
    for _ in 0..BLOCKS {
        let env = clock.env(BLOCK, rate);
        let block = contract::drive_input(&mut voice, &env, Planes::default(), &[], &[]).audio;
        for (o, b) in out.iter_mut().zip(block) {
            o.extend(b);
        }
        clock.advance(BLOCK as i64, rate);
    }
    out
}

fn assert_the_signal(out: &[Vec<f32>], what: &str) {
    assert_eq!(out.len(), 2, "{what}: width");
    for (i, (&l, &r)) in out[0].iter().zip(&out[1]).enumerate() {
        assert_eq!((l, r), signal(i), "{what}: frame {i}");
    }
    assert_eq!(out[0].len(), BLOCKS * BLOCK, "{what}: length");
}

#[test]
fn an_i16_file_streams_to_the_signal_it_holds() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("square_i16.wav");
    write_i16(&path);
    assert_the_signal(&streamed(&path), "i16 file");
}

#[test]
fn an_f32_file_streams_to_the_signal_it_holds() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("square_f32.wav");
    write_f32(&path);
    assert_the_signal(&streamed(&path), "f32 file");
}
