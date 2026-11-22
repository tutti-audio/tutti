//! What a web session runs with no tutti thread, under node (`cargo xtask test-wasm`): an offline
//! build, a bounce to memory, a WAV in memory, and the work an app awaits itself (`analyze`,
//! `load_midi`). Every input is bytes or samples in memory: the web has no file system.
#![cfg(target_arch = "wasm32")]

use std::sync::Arc;
use tutti::codec::{read_wave, write_wave};
use tutti::measure::PeakConfig;
use tutti::sampler::StreamStart;
use tutti::session::{BackgroundWork, SamplerError};
use tutti::{
    Bounce, ChannelLayout, Clip, Format, Interleaved, MainOutput, MaxBlock, Pcm, Range, SampleRate,
    Samples, Seconds, Session, Wave,
};
use wasm_bindgen_test::wasm_bindgen_test;

const RATE: SampleRate = SampleRate::SR_48K;

/// An offline session at [`RATE`].
fn session() -> Session {
    Session::builder()
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples::new(128)))
        .build()
        .expect("an offline session builds")
}

/// A mono wave of the given samples at [`RATE`].
fn wave_of(samples: impl IntoIterator<Item = f32>) -> Wave {
    let mut wave = Wave::new(1, RATE);
    for sample in samples {
        wave.append(Interleaved::new(&[sample], wave.layout()).unwrap())
            .unwrap();
    }
    wave
}

/// A square wave of amplitude 0.5 and a period of four frames, `frames` long.
fn square(frames: usize) -> Wave {
    wave_of((0..frames).map(|i| if i % 4 < 2 { 0.5 } else { -0.5 }))
}

/// **A session built offline with no thread of tutti's bounces a clip of a constant to memory as
/// the range's frames, each at the constant.** One second at the session's rate holds exactly the
/// second's frames, and the clip's level is the
/// master's level on every frame.
#[wasm_bindgen_test]
async fn a_bounce_of_a_clip_to_memory_is_the_ranges_frames_at_the_clips_level() {
    let mut session = session();
    let clip = session
        .graph()
        .add(Clip::new(Arc::new(wave_of(vec![0.25; 2 * 48_000]))).expect("a placeable wave"));
    session.graph().connect(clip, MainOutput).unwrap().unfaded();
    let bounce = Bounce::new()
        .master_buffers()
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(1.0)))
        .sample_rate(RATE);
    let rendered = session.bounce(bounce).expect("the bounce starts").await;
    let planes = session
        .apply(rendered)
        .expect("the bounce renders")
        .into_master()
        .into_planes();
    assert_eq!(planes[0].len(), 48_000);
    assert!(
        planes[0].iter().all(|&s| s == 0.25),
        "a constant clip stays constant"
    );
}

/// **A WAV encoded to memory reads back as the wave it came from.** Float32 holds every `f32`
/// exactly, so the round trip is equality.
#[wasm_bindgen_test]
fn a_wav_in_memory_reads_back_equal() {
    let wave = square(1_001);
    let mut bytes = Vec::new();
    write_wave(&wave, &mut bytes, Format::Wav(Pcm::Float32)).expect("encodes");
    assert!(bytes.starts_with(b"RIFF"), "a WAV file starts with RIFF");
    assert_eq!(read_wave::<f32>(bytes).expect("decodes"), wave);
}

/// **An analysis the app awaits reads the wave in memory.** A square of amplitude 0.5 peaks at
/// 0.5 and has a root-mean-square of 0.5: the one block over all 4 096 frames.
#[wasm_bindgen_test]
async fn an_analysis_of_a_wave_in_memory_reads_its_peak_and_rms() {
    let session = session();
    let peaks = session
        .analyze(square(4_096), PeakConfig::new(Samples::new(4_096)))
        .await
        .expect("analyzes");
    assert_eq!(peaks.layout(), ChannelLayout::MONO);
    let block = peaks.channel(0).expect("a channel")[0];
    assert_eq!(block.peak().get(), 0.5);
    assert!((block.rms.get() - 0.5).abs() < 1e-12);
}

/// **A MIDI file loaded from bytes holds the notes it was written with.** A format 0 file of two
/// quarter notes: two paired notes, each a note-on and a note-off in the track's clip.
#[wasm_bindgen_test]
async fn a_midi_file_loaded_from_bytes_has_its_notes() {
    // A quarter note at the delta before it (480 ticks of a division of 480).
    let note = |key: u8| [0, 0x90, key, 100, 0x83, 0x60, 0x80, key, 64];
    let mut body: Vec<u8> = [note(60), note(64)].concat();
    body.extend([0, 0xFF, 0x2F, 0]);
    let mut bytes = b"MThd\0\0\0\x06\0\0\0\x01\x01\xE0MTrk".to_vec();
    bytes.extend((body.len() as u32).to_be_bytes());
    bytes.extend(body);

    let mut session = session();
    let loaded = session.midi().load_midi(bytes).await.expect("loads");
    assert_eq!(loaded.file().tracks().len(), 1);
    assert_eq!(loaded.file().tracks()[0].notes().len(), 2);
    assert_eq!(loaded.track(0).unwrap().events.len(), 4);
}

/// **On a target with no thread, an offline build with the sampler on succeeds and
/// `open_stream` refuses by name**: the streamer is background work of tutti's own thread,
/// so the session has none, and the refusal names it.
#[wasm_bindgen_test]
fn open_stream_names_the_streamer_where_no_thread_starts() {
    let session = session();
    let refused = session.open_stream("a.wav", StreamStart::HEAD).err();
    assert!(
        matches!(
            refused,
            Some(SamplerError::NeedsThreads(BackgroundWork::Streamer))
        ),
        "{refused:?}"
    );
}

/// The longest a driver test awaits: a guard against a hang, never a speed.
const GUARD: std::time::Duration = std::time::Duration::from_secs(30);

/// Awaits `task` or fails at [`GUARD`]: the browser's timeout is the guard's clock.
async fn within_guard<T>(task: tutti_platform::Task<T>) -> T {
    use futures_lite::future::or;
    let guard = async {
        gloo_timers::future::sleep(GUARD).await;
        None
    };
    or(async { Some(task.await) }, guard)
        .await
        .expect("the driver ran the task before the guard")
}

/// **With no tick by the test, a page-driven executor resolves a blocking task and a sleep.**
/// `unblock` returns its closure's value, and a 20 ms `Timer` sleep resolves `Ok` once the
/// clock is at its deadline.
#[wasm_bindgen_test]
async fn a_page_driven_executor_resolves_an_unblock_task_and_a_sleep() {
    let executor = tutti_platform::Executor::new();
    let _driver = executor.drive_on_page();
    let blocked = executor.unblock(|| 6 * 7).expect("queues");
    assert_eq!(within_guard(blocked).await, 42);

    let timer = executor.timer();
    let deadline = tutti_platform::Instant::now() + std::time::Duration::from_millis(20);
    let slept = executor.spawn(async move { timer.sleep_until(deadline).await });
    assert_eq!(within_guard(slept).await, Ok(()));
    assert!(tutti_platform::Instant::now() >= deadline);
}

/// **Two executors on one page each make progress, and a sleep armed earlier than the
/// driver's wait is still fired.** The first holds a long sleep, the second then a short
/// one: both resolve, the short one without waiting for the long.
#[wasm_bindgen_test]
async fn two_page_driven_executors_each_run_their_own_tasks() {
    let (a, b) = (
        tutti_platform::Executor::new(),
        tutti_platform::Executor::new(),
    );
    let (_da, _db) = (a.drive_on_page(), b.drive_on_page());
    let long = {
        let timer = a.timer();
        a.spawn(async move { timer.sleep(std::time::Duration::from_secs(3_600)).await })
    };
    let short = {
        let timer = a.timer();
        a.spawn(async move { timer.sleep(std::time::Duration::from_millis(10)).await })
    };
    assert_eq!(within_guard(short).await, Ok(()));
    assert_eq!(within_guard(b.unblock(|| 7).expect("queues")).await, 7);
    assert!(!long.is_finished(), "the hour-long sleep has not fired");
}

/// **A queue that never empties does not starve the page.** A task that wakes itself 10 000
/// times runs in batches, and a browser timeout set before it still fires: the page's own
/// task completes while the executor has work.
#[wasm_bindgen_test]
async fn a_self_waking_task_lets_the_pages_own_tasks_run() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let executor = tutti_platform::Executor::new();
    let _driver = executor.drive_on_page();
    let turns = Arc::new(AtomicUsize::new(0));
    let busy = {
        let turns = Arc::clone(&turns);
        executor.spawn(async move {
            loop {
                futures_lite::future::yield_now().await;
                if turns.fetch_add(1, Ordering::Relaxed) >= 10_000 {
                    break;
                }
            }
        })
    };
    // The page's own task: a browser timeout, which only runs between batches.
    gloo_timers::future::sleep(std::time::Duration::from_millis(1)).await;
    within_guard(busy).await;
    assert!(turns.load(Ordering::Relaxed) > 10_000);
}

/// **Stopping the driver ends it: nothing runs afterwards.** A task queued after the
/// `PageDriver` dropped stays unrun, and the executor still runs it when ticked by hand.
#[wasm_bindgen_test]
async fn a_dropped_page_driver_runs_nothing_more() {
    let executor = tutti_platform::Executor::new();
    drop(executor.drive_on_page());
    let task = executor.spawn(async { 1 });
    gloo_timers::future::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!task.is_finished(), "a stopped driver runs no task");
    executor.run_until_stalled();
    assert!(task.is_finished());
}
