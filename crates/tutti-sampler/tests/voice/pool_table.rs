//! A pool plays the rows of its published table: a row's presence and values reach the
//! audio thread at the block after the write, a removed row is silent at once and its
//! slot is freed by the control side, a row's voice arrives over a ring that may defer
//! it, and a streamed row waits for its stream's reader.
//!
//! The figures are analytic. The wave is a ramp, frame `k` is `(k + 1) / 4096`, which a
//! placed voice at the render's rate reads back exactly, so a row at gain `g` renders
//! `g * ramp` at every frame, and rows sum.

use std::path::Path;
use std::sync::Arc;

use tutti_buffers::{ChannelLayout, Interleaved, Mono, Wave};
use tutti_graph::{IntoNode, NodeParts, Prepare, Published, RenderMode};
use tutti_rt::Reclaim;
use tutti_sampler::testing::{
    MockTransport, block, controlled_pool, prepared, source_config, wait_for_open,
};
use tutti_sampler::{
    DiskStreamer, DiskVoice, Manual, Placed, Row, SlotId, StreamHandle, StreamHold, StreamStart,
    VoiceNodeConfig, VoicePool, VoicePoolConfig, VoicePoolControls, VoiceTable, VoiceWindow,
};
use tutti_units::{Amplitude, Beat, Bpm, Cents, SampleRate, StretchFactor};

const SR: f64 = 44_100.0;
const BLOCK: usize = 64;
const RAMP: usize = 4096;

/// The frames at the start of a placed read that the gate's edge shapes.
const EDGE: usize = 8;

/// A mono ramp, frame `k` is `(k + 1) / RAMP`: a new allocation each call, so two calls are
/// two waves.
fn ramp() -> Arc<Wave> {
    let data: Vec<f32> = (0..RAMP).map(|k| (k as f32 + 1.0) / RAMP as f32).collect();
    Arc::new(Wave::from_mono(SampleRate::of(SR), Mono::new(&data)))
}

fn ramp_at(frame: usize) -> f32 {
    (frame as f32 + 1.0) / RAMP as f32
}

fn mono_pool() -> VoicePoolConfig<f32> {
    VoicePoolConfig::new(ChannelLayout::MONO).expect("mono is a width a pool runs")
}

/// A placed row over `wave` from beat 0 at `gain`.
fn row(wave: &Arc<Wave>, gain: f64) -> VoiceNodeConfig<f32, Placed> {
    let source = source_config(
        Arc::clone(wave),
        ChannelLayout::MONO,
        Some(VoiceWindow::from(Beat::new(0.0))),
    )
    .gain(Amplitude::new(gain));
    VoiceNodeConfig::placed(source).expect("a finite gain on a placed source")
}

/// The table of `rows`, one per `(key, wave, gain)`.
fn table(rows: &[(u128, &Arc<Wave>, f64)]) -> Arc<VoiceTable<f32>> {
    let mut config = mono_pool();
    for &(key, wave, gain) in rows {
        config = config
            .row(SlotId(key), row(wave, gain))
            .expect("distinct keys in the voices");
    }
    Arc::clone(config.table())
}

/// A pool rendered by hand under a rolling transport, with its controls.
struct Rig {
    pool: VoicePool<f32>,
    controls: VoicePoolControls<f32>,
    transport: Arc<MockTransport>,
    /// Frames rendered so far: the next block's first.
    at: usize,
}

impl Rig {
    fn over(config: VoicePoolConfig<f32>) -> Self {
        let (pool, controls) = controlled_pool(VoicePool::from(config));
        Self {
            pool: prepared(pool, SampleRate::of(SR), BLOCK),
            controls,
            transport: MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0)),
            at: 0,
        }
    }

    /// One block, channel 0; the transport moves on.
    fn block(&mut self) -> Vec<f32> {
        let out = block(&mut self.pool, &self.transport, SampleRate::of(SR), BLOCK).remove(0);
        self.transport.advance(BLOCK as i64, SampleRate::of(SR));
        self.at += BLOCK;
        out
    }

    fn write(&self, table: &Arc<VoiceTable<f32>>) {
        self.controls.replace(table);
    }
}

/// Every frame of `out` past the gate's edge, which starts at global frame `from`, is
/// `gain * ramp`.
fn assert_ramp(out: &[f32], from: usize, gain: f32, what: &str) {
    for (k, &got) in out
        .iter()
        .enumerate()
        .skip(if from == 0 { EDGE } else { 0 })
    {
        let want = gain * ramp_at(from + k);
        assert!(
            (got - want).abs() < 1e-6,
            "{what}: frame {} is {got}, {gain} * the ramp is {want}",
            from + k
        );
    }
}

/// A placed ramp at gain 0.5 renders `0.5 * ramp`, and a table written at gain 0.25 renders
/// `0.25 * ramp` on the very next block.
#[test]
fn a_row_renders_its_gain_times_the_ramp_and_a_table_write_moves_it_the_next_block() {
    let wave = ramp();
    let mut rig = Rig::over(mono_pool().row(SlotId(1), row(&wave, 0.5)).unwrap());
    let first = rig.block();
    assert_ramp(&first, 0, 0.5, "gain 0.5");
    rig.write(&table(&[(1, &wave, 0.25)]));
    let next = rig.block();
    assert_ramp(&next, BLOCK, 0.25, "gain 0.25 after the write");
}

/// Two rows over the ramp sum to twice it.
#[test]
fn two_rows_sum_to_twice_the_ramp() {
    let wave = ramp();
    let rig_config = mono_pool()
        .row(SlotId(1), row(&wave, 1.0))
        .unwrap()
        .row(SlotId(2), row(&wave, 1.0))
        .unwrap();
    let mut rig = Rig::over(rig_config);
    assert_ramp(&rig.block(), 0, 2.0, "two rows");
}

/// A row removed from the table is exactly silent from the next block, before the slot it
/// leaves behind is collected; the wave is freed only by the control side's reclaim.
#[test]
fn a_removed_row_is_exactly_silent_the_next_block_before_its_retire_drains() {
    let wave = ramp();
    let mut rig = Rig::over(mono_pool().row(SlotId(1), row(&wave, 1.0)).unwrap());
    assert_ramp(&rig.block(), 0, 1.0, "the row sounds");
    rig.write(&table(&[]));
    let silent = rig.block();
    assert!(
        silent.iter().all(|&x| x.to_bits() == 0.0f32.to_bits()),
        "a removed row renders exactly +0.0 from the next block"
    );
    assert!(
        Arc::strong_count(&wave) > 1,
        "the retired slot still holds the wave: the audio thread freed nothing"
    );
    for _ in 0..2 {
        rig.controls.reclaim();
    }
    assert_eq!(
        Arc::strong_count(&wave),
        1,
        "the control side's reclaim freed the slot and the tables"
    );
    assert_eq!(rig.controls.drops().leaked, 0);
}

/// A wave swapped under one key is a new voice: the old wave is exactly silent from the next
/// block, and the new one sounds its own figure once its voice has arrived. Matching slots by
/// key alone would keep playing the old wave.
#[test]
fn a_wave_swapped_under_one_key_is_a_new_voice() {
    let old = ramp();
    // Another wave: twice the ramp, so which one sounds is told by the level.
    let data: Vec<f32> = (0..RAMP)
        .map(|k| 2.0 * (k as f32 + 1.0) / RAMP as f32)
        .collect();
    let new = Arc::new(Wave::from_mono(SampleRate::of(SR), Mono::new(&data)));
    let mut rig = Rig::over(mono_pool().row(SlotId(1), row(&old, 1.0)).unwrap());
    assert_ramp(&rig.block(), 0, 1.0, "the old wave");
    rig.write(&table(&[(1, &new, 1.0)]));
    // The new voice was sent by the write: the block that takes it in plays it, and the
    // old one is not played in that block.
    let swapped = rig.block();
    assert_ramp(&swapped, BLOCK, 2.0, "the new wave, twice the ramp");
    rig.controls.reclaim();
    rig.controls.reclaim();
    assert_eq!(
        Arc::strong_count(&old),
        1,
        "the old voice was retired and freed"
    );
}

/// A table change takes voices in, retires others and moves values, and the audio thread
/// allocates and frees nothing (the area binary aborts an allocation or a free inside a
/// render).
#[test]
fn a_table_change_allocates_and_frees_nothing_on_the_audio_thread() {
    let first: Vec<_> = (0..4).map(|_| ramp()).collect();
    let second: Vec<_> = (0..4).map(|_| ramp()).collect();
    let rows = |waves: &[Arc<Wave>], gain: f64| {
        table(
            &waves
                .iter()
                .enumerate()
                .map(|(k, w)| (k as u128, w, gain))
                .collect::<Vec<_>>(),
        )
    };
    let mut config = mono_pool();
    for (k, wave) in first.iter().enumerate() {
        config = config.row(SlotId(k as u128), row(wave, 0.25)).unwrap();
    }
    let mut rig = Rig::over(config);
    assert_ramp(&rig.block(), 0, 1.0, "four rows at a quarter");
    // Values move, voices are replaced, rows leave and arrive.
    rig.write(&rows(&first, 0.5));
    assert_ramp(&rig.block(), BLOCK, 2.0, "four rows at a half");
    rig.write(&rows(&second, 0.25));
    let moved = rig.block();
    assert_ramp(&moved, 2 * BLOCK, 1.0, "four other voices at a quarter");
    rig.write(&rows(&second[..2], 0.25));
    assert_ramp(&rig.block(), 3 * BLOCK, 0.5, "two rows left");
}

/// A stereo wave in a mono pool is placed by a fold, which owns its matrix: matching its row
/// against the table's, and moving its value, copies nothing on the audio thread (the area
/// binary aborts an allocation or a free inside a render).
#[test]
fn a_folded_row_is_matched_and_moved_without_allocating() {
    let mut wave = Wave::new(2, SampleRate::of(SR));
    for _ in 0..RAMP {
        wave.append(Interleaved::new(&[0.25, 0.25], wave.layout()).unwrap())
            .unwrap();
    }
    let wave = Arc::new(wave);
    let at = |gain: f64| {
        let source = source_config(
            Arc::clone(&wave),
            ChannelLayout::MONO,
            Some(VoiceWindow::from(Beat::new(0.0))),
        )
        .gain(Amplitude::new(gain));
        Arc::clone(
            mono_pool()
                .row(
                    SlotId(1),
                    VoiceNodeConfig::placed(source).expect("a placed source"),
                )
                .unwrap()
                .table(),
        )
    };
    let mut rig = Rig::over(mono_pool());
    rig.write(&at(1.0));
    let loud = rig.block();
    rig.write(&at(0.5));
    let soft = rig.block();
    let level = |out: &[f32]| out[EDGE..].iter().copied().fold(0.0f32, f32::max);
    assert!(level(&loud) > 0.0, "the folded row sounds");
    assert!(
        (level(&soft) - 0.5 * level(&loud)).abs() < 1e-6,
        "half the gain, half the level: {} and {}",
        level(&soft),
        level(&loud)
    );
}

/// A full table removed, written again under new keys and removed again before any
/// reclaim retires every slot through the ring without the audio thread allocating or
/// freeing (the area binary aborts either in a render), and one reclaim collects them all.
#[test]
fn a_full_table_removed_twice_before_a_reclaim_is_collected_whole() {
    const ROWS: usize = 64;
    let waves: Vec<_> = (0..2 * ROWS).map(|_| ramp()).collect();
    let rows_at = |first: usize| {
        let rows: Vec<_> = (0..ROWS)
            .map(|k| ((first + k) as u128, &waves[first + k], 1.0 / ROWS as f64))
            .collect();
        table(&rows)
    };
    let mut rig = Rig::over(mono_pool());
    rig.write(&rows_at(0));
    assert_ramp(&rig.block(), 0, 1.0, "the first 64 rows sound");
    assert_eq!(rig.pool.voice_count(), ROWS);
    rig.write(&table(&[]));
    assert!(
        rig.block().iter().all(|&x| x == 0.0),
        "the first table is gone"
    );
    rig.write(&rows_at(ROWS));
    assert_ramp(&rig.block(), 2 * BLOCK, 1.0, "64 rows under new keys sound");
    rig.write(&table(&[]));
    assert!(
        rig.block().iter().all(|&x| x == 0.0),
        "the second table is gone"
    );
    assert_eq!(rig.pool.voice_count(), 0, "every slot left the pool");
    let held = |waves: &[Arc<Wave>]| waves.iter().filter(|w| Arc::strong_count(w) > 1).count();
    assert_eq!(
        held(&waves),
        2 * ROWS,
        "128 retired slots wait in the ring, none freed by the audio thread"
    );
    rig.controls.reclaim();
    rig.controls.reclaim();
    assert_eq!(held(&waves), 0, "one reclaim collected every slot");
    assert_eq!(rig.controls.drops().leaked, 0);
}

/// A table of 64 rows written over and over with the audio thread not draining takes more
/// voices than the ring holds: the rest wait for later ticks, and the table is whole within
/// a few of them, never dropped.
#[test]
fn a_table_larger_than_the_ring_installs_over_reclaim_ticks() {
    const ROWS: usize = 64;
    let gain = 1.0 / ROWS as f64;
    let tables: Vec<_> = (0..3)
        .map(|_| {
            let waves: Vec<_> = (0..ROWS).map(|_| ramp()).collect();
            let rows: Vec<_> = waves
                .iter()
                .enumerate()
                .map(|(k, w)| (k as u128, w, gain))
                .collect();
            (table(&rows), waves)
        })
        .collect();
    let mut rig = Rig::over(mono_pool());
    for (table, _) in &tables {
        rig.write(table);
    }
    rig.block();
    assert!(
        rig.pool.voice_count() < ROWS,
        "installs are not atomic: the ring held part of the last table"
    );
    let mut ticks = 0;
    while rig.pool.voice_count() < ROWS {
        assert!(ticks < 3, "the table is whole within three ticks");
        rig.controls.reclaim();
        rig.block();
        ticks += 1;
    }
    let whole = rig.block();
    assert_ramp(
        &whole,
        rig.at - BLOCK,
        1.0,
        "64 rows at 1/64 sum to the ramp",
    );
}

/// The fork of a pool is a pool over the table as it was when the fork was taken: a later
/// write moves the live pool and not the fork.
#[test]
fn a_fork_reflects_the_table_at_fork_time() {
    let wave = ramp();
    let NodeParts {
        mut node,
        controls,
        fork,
        ..
    } = VoicePool::from(mono_pool().row(SlotId(1), row(&wave, 0.5)).unwrap()).into_parts();
    let fork = fork.expect("a pool forks");
    node.prepare(&Prepare::new(
        SampleRate::of(SR),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(BLOCK)),
    ));
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    // The live pool drains its first voices.
    let _ = block(&mut *node, &transport, SampleRate::of(SR), BLOCK);
    controls.replace(&table(&[(1, &wave, 0.5), (2, &wave, 0.5)]));
    let mut forked = fork
        .fork(RenderMode::Realtime)
        .expect("a pool forks after a write")
        .node;
    controls.replace(&table(&[(1, &wave, 0.25)]));
    forked.prepare(&Prepare::new(
        SampleRate::of(SR),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(BLOCK)),
    ));
    let forked = block(&mut *forked, &transport, SampleRate::of(SR), BLOCK).remove(0);
    let live = block(&mut *node, &transport, SampleRate::of(SR), BLOCK).remove(0);
    assert_ramp(
        &forked,
        0,
        1.0,
        "the fork has the two rows of the table it was taken at",
    );
    assert_ramp(&live, 0, 0.25, "the live pool has the table written since");
}

// ---------------------------------------------------------------------------
// Streamed rows.
// ---------------------------------------------------------------------------

/// The level of the streamed file.
const LEVEL: f32 = 0.5;

/// A hand-stepped streamer over a stereo DC file of `frames` frames at `LEVEL`, its stream and
/// the voice of its reader, placed at beat 0.
fn stream_over(
    path: &Path,
    frames: usize,
) -> (
    DiskStreamer<Manual<f32>, f32>,
    StreamHandle<f32>,
    DiskVoice<f32>,
) {
    tutti_fixtures::signal::write_wav_f32(path, SampleRate::of(SR), 2, &vec![LEVEL; 2 * frames]);
    let mut streamer =
        DiskStreamer::manual(SampleRate::of(SR), Default::default()).expect("a streamer builds");
    let opening = streamer.open_stream(path, StreamStart::HEAD);
    let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    assert!(streamer.step_until_idle(1_000) < 1_000, "the ring primes");
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
    (streamer, stream, voice)
}

/// A streamed row declared again over the same stream waits (`Waiting`, exactly zero) until
/// the slot that holds the reader is dropped by the control side's reclaim, and then holds it
/// and plays the file's level.
#[test]
fn a_streamed_row_declared_over_the_same_stream_waits_for_the_holder_to_be_reclaimed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, stream, voice) = stream_over(&dir.path().join("dc.wav"), 8_192);
    let streamed = VoiceNodeConfig::streaming(
        voice,
        stream,
        Amplitude::new(1.0),
        StretchFactor::UNITY,
        Cents::new(0.0),
    )
    .expect("a stereo stream");
    let stereo = || VoicePoolConfig::<f32>::new(ChannelLayout::STEREO).expect("stereo");
    let mut rig = {
        let (pool, controls) = controlled_pool(VoicePool::from(
            stereo().row(SlotId(1), streamed.clone()).unwrap(),
        ));
        Rig {
            pool: prepared(pool, SampleRate::of(SR), BLOCK),
            controls,
            transport: MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0)),
            at: 0,
        }
    };
    assert_eq!(rig.controls.hold(SlotId(1)), Some(StreamHold::Holding));
    rig.block();

    // The same stream under another key: the first slot is retired at the next block, and
    // the new slot waits for the reader the first one holds until it is dropped.
    rig.write(stereo().row(SlotId(2), streamed.clone()).unwrap().table());
    assert_eq!(rig.controls.hold(SlotId(2)), Some(StreamHold::Waiting));
    let waiting = block(&mut rig.pool, &rig.transport, SampleRate::of(SR), BLOCK);
    assert!(
        waiting.iter().flatten().all(|&x| x == 0.0),
        "the old slot is retired and the new one waits: exactly zero"
    );
    assert_eq!(rig.controls.hold(SlotId(2)), Some(StreamHold::Waiting));
    assert_eq!(
        rig.controls.hold(SlotId(1)),
        None,
        "a removed row has no hold"
    );

    // The reclaim drops the old slot, which gives the reader to the waiting one.
    rig.controls.reclaim();
    let mut level = Vec::new();
    for _ in 0..8 {
        let _ = streamer.step();
        level = block(&mut rig.pool, &rig.transport, SampleRate::of(SR), BLOCK).remove(0);
        rig.transport.advance(BLOCK as i64, SampleRate::of(SR));
    }
    assert_eq!(rig.controls.hold(SlotId(2)), Some(StreamHold::Holding));
    assert!(
        level.iter().skip(EDGE).all(|&x| (x - LEVEL).abs() < 1e-5),
        "the holder plays the file's level {LEVEL}: {:?}",
        &level[..4]
    );
    assert_eq!(
        rig.controls.hold(SlotId(3)),
        None,
        "a key with no installed slot has no hold"
    );
}

/// A streamed row that carries a stretch plays through its vocoder: a stretch of 2 plays a
/// burst of DC twice as long as the file holds it.
#[test]
fn a_streamed_row_with_a_stretch_renders_at_that_stretch() {
    const BURST: usize = 4_000;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut samples = vec![LEVEL; 2 * BURST];
    samples.extend(vec![0.0; 2 * 20_000]);
    let play = |stretch: StretchFactor| {
        let path = dir.path().join(format!("burst-{}.wav", stretch.get()));
        tutti_fixtures::signal::write_wav_f32(&path, SampleRate::of(SR), 2, &samples);
        let mut streamer = DiskStreamer::manual(SampleRate::of(SR), Default::default())
            .expect("a streamer builds");
        let opening = streamer.open_stream(&path, StreamStart::HEAD);
        let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
        assert!(streamer.step_until_idle(1_000) < 1_000, "the ring primes");
        let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
        let config = VoiceNodeConfig::streaming(
            voice,
            stream,
            Amplitude::new(1.0),
            stretch,
            Cents::new(0.0),
        )
        .expect("a stereo stream");
        let pool = VoicePoolConfig::<f32>::new(ChannelLayout::STEREO)
            .unwrap()
            .row(SlotId(1), Row::from(config))
            .unwrap();
        let (pool, _controls) = controlled_pool(VoicePool::from(pool));
        let mut pool = prepared(pool, SampleRate::of(SR), BLOCK);
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
        let mut out = Vec::new();
        while out.len() < 3 * BURST {
            let _ = streamer.step();
            out.extend(block(&mut pool, &transport, SampleRate::of(SR), BLOCK).remove(0));
            transport.advance(BLOCK as i64, SampleRate::of(SR));
        }
        out
    };
    let unity = play(StretchFactor::UNITY);
    let doubled = play(StretchFactor::new(2.0));
    let held = |out: &[f32]| out.iter().filter(|x| x.abs() > LEVEL / 2.0).count();
    assert!(
        held(&unity).abs_diff(BURST) < 400,
        "unity plays the burst for its {BURST} frames: {}",
        held(&unity)
    );
    assert!(
        held(&doubled).abs_diff(2 * BURST) < 1_500,
        "a stretch of 2 plays it for twice as long, less the vocoder's window edges: {}",
        held(&doubled)
    );
}

/// A streamed row's fork taken after a table write and before any block plays at the
/// written values: the fork's detached cells are given the row's speed, not the one the
/// live cells last held. The file is a ramp, frame `k` is `(k + 1) / 4096`, so at speed 2
/// the last frame of a 64-frame block is `(2 * 63 + 1) / 4096`, where speed 1 reads
/// `64 / 4096`.
#[test]
fn a_streamed_rows_fork_taken_after_a_write_plays_at_the_written_speed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    let samples: Vec<f32> = (0..RAMP)
        .flat_map(|k| [(k as f32 + 1.0) / RAMP as f32; 2])
        .collect();
    tutti_fixtures::signal::write_wav_f32(&path, SampleRate::of(SR), 2, &samples);
    let mut streamer =
        DiskStreamer::manual(SampleRate::of(SR), Default::default()).expect("a streamer builds");
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    assert!(streamer.step_until_idle(1_000) < 1_000, "the ring primes");
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
    let streamed = VoiceNodeConfig::streaming(
        voice,
        stream,
        Amplitude::new(1.0),
        StretchFactor::UNITY,
        Cents::new(0.0),
    )
    .expect("a stereo stream");
    let stereo = || VoicePoolConfig::<f32>::new(ChannelLayout::STEREO).expect("stereo");
    let NodeParts { controls, fork, .. } =
        VoicePool::from(stereo().row(SlotId(1), streamed.clone()).unwrap()).into_parts();
    // The same stream at another speed: a value, so the row keeps its voice.
    let faster = streamed.speed(tutti_units::PlaybackRate::of(2.0));
    controls.replace(stereo().row(SlotId(1), faster).unwrap().table());

    let offline = Prepare::new(
        SampleRate::of(SR),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(BLOCK)),
    )
    .with_render_mode(RenderMode::Offline);
    let mut forked = fork
        .expect("a pool forks")
        .fork(RenderMode::Offline)
        .expect("a streamed row forks for an offline render")
        .node;
    forked.prepare(&offline);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let env = transport.env(BLOCK, SampleRate::of(SR));
    let out = tutti_graph::contract::drive_input_in(
        &mut *forked,
        &offline,
        &env,
        tutti_buffers::Planes::default(),
        &[],
        &[],
    )
    .audio;
    let last = out[0][BLOCK - 1];
    let at_speed_2 = (2.0 * (BLOCK - 1) as f32 + 1.0) / RAMP as f32;
    assert!(
        (last - at_speed_2).abs() < 1e-4,
        "the fork's last frame is {last}, a read at speed 2 is {at_speed_2} (speed 1 is {})",
        BLOCK as f32 / RAMP as f32
    );
}

/// A row declared with one voice, then another structure, then the first again before any
/// block: the controls built the first voice twice, the audio thread already plays the
/// older one, and the newer replaces it, so the controls' hold reading describes the voice
/// that sounds and the row ends up `Holding`, sounding the file's level.
#[test]
fn a_row_that_returns_to_its_structure_before_a_block_ends_up_holding() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, stream, voice) = stream_over(&dir.path().join("a.wav"), 8_192);
    let (_other_streamer, other_stream, other_voice) =
        stream_over(&dir.path().join("b.wav"), 8_192);
    let make = |voice, stream| {
        VoiceNodeConfig::streaming(
            voice,
            stream,
            Amplitude::new(1.0),
            StretchFactor::UNITY,
            Cents::new(0.0),
        )
        .expect("a stereo stream")
    };
    let first = make(voice, stream);
    let second = make(other_voice, other_stream);
    let stereo = || VoicePoolConfig::<f32>::new(ChannelLayout::STEREO).expect("stereo");
    let (pool, controls) = controlled_pool(VoicePool::from(
        stereo().row(SlotId(1), first.clone()).unwrap(),
    ));
    let mut pool = prepared(pool, SampleRate::of(SR), BLOCK);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let play = |pool: &mut VoicePool<f32>, streamer: &mut DiskStreamer<Manual<f32>, f32>| {
        let _ = streamer.step();
        let out = block(pool, &transport, SampleRate::of(SR), BLOCK).remove(0);
        transport.advance(BLOCK as i64, SampleRate::of(SR));
        out
    };
    play(&mut pool, &mut streamer);
    assert_eq!(controls.hold(SlotId(1)), Some(StreamHold::Holding));

    // Another stream under the key, and the first again, with no block between.
    controls.replace(stereo().row(SlotId(1), second).unwrap().table());
    controls.replace(stereo().row(SlotId(1), first).unwrap().table());
    for _ in 0..4 {
        play(&mut pool, &mut streamer);
        controls.reclaim();
    }
    let level = play(&mut pool, &mut streamer);
    assert_eq!(
        controls.hold(SlotId(1)),
        Some(StreamHold::Holding),
        "the controls' record is of the voice that holds the reader"
    );
    assert!(
        level.iter().skip(EDGE).all(|&x| (x - LEVEL).abs() < 1e-5),
        "the row sounds the file's level"
    );
    assert_eq!(pool.voice_count(), 1, "one voice plays the key");
}
