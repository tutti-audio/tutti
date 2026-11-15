//! A session on the system clock: an OS timer thread renders the graph, with
//! no device and no host pulling blocks.

use std::time::{Duration, Instant};

use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::{OutPort, Root, Session, SystemSession};
use tutti_transport::MotionEvent;
use tutti_types::{MaxBlock, SampleRate, Samples};

const BLOCK: usize = 480;

fn system() -> SystemSession {
    Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples(BLOCK)))
        .system_clock()
        .build()
        .expect("a timer thread starts")
}

/// **A system session renders its graph with no device and no host.** A
/// constant wired to both outputs reaches the heard tap, in whole stereo
/// frames, and the playhead moves in whole timer blocks: the timer thread is
/// the only thing that called the engine.
#[test]
fn a_system_session_renders_its_graph_with_no_device() {
    let mut system = system();
    let s = system.session_mut();
    let (level, _) = s.graph().add(ForkByClone(Const::mono(0.25)));
    for port in 0..2 {
        s.graph()
            .set_sources_hard(Root::output(port), [OutPort::new(level, 0)])
            .unwrap();
    }
    let limit = Instant::now() + Duration::from_secs(10);
    let mut tap = loop {
        let _ = s.update();
        if let Some(tap) = s.open_heard_tap(Samples(4 * BLOCK)).unwrap().reader() {
            break tap;
        }
        assert!(
            Instant::now() < limit,
            "the audio side never took the graph"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(tap.width(), 2);

    s.transport()
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
    let mut heard = vec![0.0f32; 2 * BLOCK];
    loop {
        let _ = s.update();
        let frames = tap.pop_into(&mut heard).0;
        if frames > 0 && heard[..2 * frames].iter().all(|&x| x == 0.25) {
            break;
        }
        assert!(Instant::now() < limit, "the constant never reached the tap");
        std::thread::sleep(Duration::from_millis(2));
    }
    let head = s.playhead();
    assert!(head.playing(), "the transport rolls on the timer's blocks");
    assert_eq!(head.frame().get() % BLOCK as u64, 0);
}

/// **A timer that has fallen behind by nothing has recorded no fault**: the
/// fault count of a fresh system session is zero.
#[test]
fn a_fresh_system_session_has_no_faults() {
    let system = system();
    assert_eq!(system.faults().count(), 0);
}
