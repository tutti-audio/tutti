use super::*;
use crate::AudioEngine;
use tutti_buffers::ChannelLayout;
use tutti_buffers::MaxBlock;
use tutti_buffers::SampleFormat;
use tutti_engine::{Engine, ManualStreamDriver};
use tutti_graph::{Editor, Prepare};
use tutti_timeline::Transport;
use tutti_units::{SampleRate, Samples};

fn spec_at(rate: f64) -> OutputSpec {
    OutputSpec::new(
        SampleRate::of(rate),
        ChannelLayout::STEREO,
        SampleFormat::F32,
    )
}

/// **A plain restart refuses a device at another rate, and leaves the
/// stream stopped** — its hook, run through the device-free restart
/// (`restart` itself needs a device to resolve). At the same rate it
/// starts.
#[test]
fn a_plain_restart_refuses_a_new_rate_and_stays_stopped() {
    let transport = Transport::new(SampleRate::SR_44K1);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_44K1,
        MaxBlock::of(Samples::new(512)),
    ));
    let engine = Engine::new(&transport, &mut ed, exec).expect("an empty graph");
    let state = Arc::new(AudioCallbackState::new(engine));
    let mut driver = TuttiDriver::new(AudioEngine::from_spec(spec_at(44_100.0)), state);
    let graph = SampleRate::SR_44K1;

    // Twice onto the same 48 kHz device: the second attempt must not
    // find the first one's resolved spec and call it "no change".
    for attempt in 0..2 {
        let (d, stream) = ManualStreamDriver::new();
        let err = driver
            .switch(
                spec_at(48_000.0),
                d,
                refuse_rate_change(driver.graph_rate()),
            )
            .expect_err("a new rate is refused");
        assert!(
            matches!(
                err,
                crate::DeviceError::RateChanged { device, graph: g }
                    if device == SampleRate::SR_48K && g == graph
            ),
            "attempt {attempt}: {err}"
        );
        assert!(
            !driver.stream().is_running(),
            "nothing plays at the wrong rate"
        );
        assert!(!stream.is_open());
        assert_eq!(driver.graph_rate(), graph, "the graph did not move");
        assert_eq!(
            driver.stream().spec().sample_rate,
            graph,
            "the spec does not claim the refused rate"
        );
    }

    let (d, stream) = ManualStreamDriver::new();
    driver
        .switch(spec_at(44_100.0), d, refuse_rate_change(graph))
        .expect("the same rate restarts");
    assert!(driver.stream().is_running() && stream.is_open());
    drop(ed);
}
