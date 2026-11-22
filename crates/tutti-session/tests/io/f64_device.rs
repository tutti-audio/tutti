//! A session on a device clock rendered at `f64`: the stream's callback state
//! carries the graph's own format, so a figure `f32` cannot hold reaches the
//! device buffer unrounded.

use tutti_buffers::ChannelLayout;
use tutti_buffers::SampleFormat;
use tutti_engine::{ManualStreamDriver, OutputSpec};
use tutti_graph::{ForkByClone, Params};
use tutti_rt::Param;
use tutti_session::{MainOutput, Session};
use tutti_units::{Amplitude, SampleRate, Samples};

#[path = "../support/f64_nodes.rs"]
pub(super) mod f64_nodes;
use f64_nodes::{Dc, FINE, Gain};

/// **The device callback of an `f64` session hands the stream `0.75 * FINE`
/// exactly.** A gain set through `Session::set` scales a constant `f32` rounds
/// away, and the buffer the stream renders is `f64`.
#[test]
fn an_f64_device_session_renders_a_figure_f32_rounds_away_to_its_stream() {
    // Setup: `f32` loses the figure, so a pass is not a rounded one.
    assert_ne!(f64::from((0.75 * FINE) as f32), 0.75 * FINE);
    let (driver, stream) = ManualStreamDriver::<f64>::new();
    let spec = OutputSpec::new(SampleRate::SR_48K, ChannelLayout::MONO, SampleFormat::F64);
    let mut s = Session::builder()
        .float::<f64>()
        .outputs_floor(ChannelLayout::MONO)
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec, driver)
        .expect("an f64 device session builds with no device");
    let dc = s.graph().add(ForkByClone(Dc(FINE)));
    let gain = s.graph().add(Params(Gain(Param::new(Amplitude::new(1.0)))));
    s.graph()
        .connect(dc.out(0), gain.input(0))
        .unwrap()
        .unfaded();
    s.graph()
        .connect(gain.out(0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    let _ = s.update();
    s.set(gain.volume().to(Amplitude::new(0.75))).unwrap();

    let block = stream
        .render_block(Samples::new(64))
        .expect("the stream is open");
    assert!(block.iter().all(|&x| x == 0.75 * FINE), "{:?}", &block[..3]);
}

/// **An `f64` device session moves onto the timer and keeps rendering `FINE`
/// exactly**: the switch carries the graph, the transport and the stream host
/// in the session's own float, and the heard tap reads the constant `f32`
/// rounds away.
#[test]
fn an_f64_device_session_switches_to_the_timer_and_keeps_rendering_fine() {
    use std::time::Duration;
    use tutti_buffers::InterleavedMut;

    assert_ne!(f64::from(FINE as f32), FINE);
    let (driver, stream) = ManualStreamDriver::<f64>::new();
    let spec = OutputSpec::new(SampleRate::SR_48K, ChannelLayout::MONO, SampleFormat::F64);
    let mut live = Session::builder()
        .float::<f64>()
        .outputs_floor(ChannelLayout::MONO)
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec, driver)
        .expect("an f64 device session builds with no device");
    let dc = live.graph().add(ForkByClone(Dc(FINE)));
    live.graph()
        .connect(dc.out(0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    let _ = live.update();
    stream.render_block(Samples::new(64)).expect("open");
    let mut system = live
        .into_system_clock()
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    assert!(
        stream.render_block(Samples::new(64)).is_none(),
        "the card's stream stopped"
    );
    system.play().expect("queued");
    let guard = Duration::from_secs(10);
    let mut tap = None;
    tutti_fixtures::wait_until("the audio side never took the graph", guard, || {
        let _ = system.update();
        tap = system.open_heard_tap(Samples::new(2_048)).unwrap().reader();
        tap.is_some()
    });
    let mut tap = tap.expect("opened");
    let mut heard = vec![0.0f64; 512];
    tutti_fixtures::wait_until("FINE never reached the tap", guard, || {
        let _ = system.update();
        let frames = tap
            .pop_into(InterleavedMut::new(&mut heard, tap.layout()).unwrap())
            .unwrap()
            .get();
        frames > 0 && heard[..frames].iter().all(|&x| x == FINE)
    });
    assert_eq!(system.faults().count(), 0);
}
