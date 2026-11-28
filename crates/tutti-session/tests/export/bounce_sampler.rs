//! What a bounce renders past its range for the sampler's nodes, and what it
//! does with a voice pool: a placed clip is the programme and rings out
//! nothing, a free-running voice rings to the end of its wave, and a pool
//! forks with the voices of its table.

use crate::bounced::bounced;
use std::sync::Arc;
use tutti_buffers::Interleaved;
use tutti_fixtures::WavePlanes;
use tutti_sampler::testing::{free_source, placed_config};
use tutti_session::ToMemory;

use tutti_buffers::ChannelLayout;
use tutti_buffers::Wave;
use tutti_sampler::{Clip, Loop, SlotId, VoiceNodeConfig, VoicePool, VoicePoolConfig};
use tutti_session::{Bounce, BounceTail, MainOutput, Range, Session};
use tutti_units::{Beat, PlaybackRate, SampleRate, Samples, Seconds};

const RATE: f64 = 48_000.0;

fn session() -> Session {
    Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .sample_rate(SampleRate::of(RATE))
        .block_dates(tutti_session::BlockDates::Frames)
        .build()
        .expect("an offline session builds")
}

/// A mono wave of `frames` frames of `level`, at the session's rate.
fn constant(frames: usize, level: f32) -> Arc<Wave> {
    let mut wave = Wave::new(1, SampleRate::of(RATE));
    for _ in 0..frames {
        wave.append(Interleaved::new(&[level], wave.layout()).unwrap())
            .unwrap();
    }
    Arc::new(wave)
}

/// The main output's frame count of a bounce of the first `seconds`, with the
/// tail the bounce picks without being asked.
fn length_of(session: &mut Session, seconds: f64) -> usize {
    let bounce = Bounce::new()
        .main_output(ToMemory::new())
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(seconds)))
        .sample_rate(SampleRate::of(RATE));
    bounced(session, bounce)
        .expect("the bounce renders")
        .into_main_output()
        .to_planes()[0]
        .len()
}

/// **A placed clip rings out nothing, so a bounce keeps the range and no
/// cap.** A two-second clip at beat 0, bounced over the first second with the
/// default tail: exactly the second's frames. A clip that declared an
/// unbounded tail rendered the tail's cap (ten seconds) after it.
#[test]
fn a_placed_clip_adds_no_tail_to_the_range() {
    let mut session = session();
    let clip = session
        .graph()
        .add(Clip::new(constant(2 * RATE as usize, 0.25)).expect("a placeable wave"));
    session.graph().connect(clip, MainOutput).unwrap().unfaded();
    assert_eq!(length_of(&mut session, 1.0), RATE as usize);
}

/// **A free-running voice rings out the whole of its wave at the rate it
/// reads.** A 12 000-frame wave: its length after the range, half of it at
/// double speed; and a looped one never ends, so the bounce's cap is what it
/// keeps.
#[test]
fn a_free_running_voice_rings_out_its_wave() {
    let frames = 12_000;
    let at = |speed: f64, looped: Option<Loop>| {
        let mut session = session();
        let mut source = free_source(constant(frames, 0.25));
        source.set_speed(PlaybackRate::clamped(speed));
        source.set_loop(looped);
        let voice = session.graph().add(source);
        session
            .graph()
            .connect(voice, MainOutput)
            .unwrap()
            .unfaded();
        length_of(&mut session, 1.0)
    };
    let second = RATE as usize;
    assert_eq!(at(1.0, None), second + frames);
    assert_eq!(at(2.0, None), second + frames / 2);

    let looped = Loop::new(Samples::new(0)..Samples::new(frames)).expect("a loop");
    let mut session = session();
    let mut source = free_source(constant(frames, 0.25));
    source.set_loop(Some(looped));
    let voice = session.graph().add(source);
    session
        .graph()
        .connect(voice, MainOutput)
        .unwrap()
        .unfaded();
    let cap = Seconds::new(0.5);
    let bounce = Bounce::new()
        .main_output(ToMemory::new())
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(1.0)))
        .sample_rate(SampleRate::of(RATE))
        .tail(BounceTail::Graph { cap });
    let len = bounced(&mut session, bounce)
        .expect("renders")
        .into_main_output()
        .to_planes()[0]
        .len();
    assert_eq!(len, second + (RATE * 0.5) as usize);
}

/// **A pool built with its voices forks them.** A voice placed at beat 0
/// inserted before the pool joins the graph is in the bounce, at its level.
#[test]
fn a_pool_built_with_its_voices_bounces_them() {
    let mut session = session();
    let source = placed_config(
        constant(RATE as usize, 0.25),
        tutti_sampler::VoiceWindow::from(Beat::new(0.0)),
    );
    let row = VoiceNodeConfig::placed(source).expect("a finite gain and a nonempty width");
    let config = VoicePoolConfig::new(ChannelLayout::STEREO).expect("a stereo pool");
    let pool = VoicePool::from(config.row(SlotId(1), row).expect("a row in the table"));
    let pool = session.graph().add(pool);
    session.graph().connect(pool, MainOutput).unwrap().unfaded();

    let bounce = Bounce::new()
        .main_output(ToMemory::new())
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.5)))
        .sample_rate(SampleRate::of(RATE))
        .tail(BounceTail::None);
    let planes = bounced(&mut session, bounce)
        .expect("renders")
        .into_main_output()
        .to_planes();
    assert_eq!(planes[0].len(), RATE as usize / 2);
    assert!(
        planes[0].iter().all(|&s| (s - 0.25).abs() < 1e-6),
        "the pool's voice is in the render at its level"
    );
}
