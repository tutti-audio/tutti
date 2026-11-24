use super::*;
use crate::AudioEngine;
use crate::output::Opened;
use crate::output::tests::{driver_over, manual_spec, on_device};
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

/// **A restart that cannot resolve a device is refused before the running
/// stream is stopped**: a driver over a supplied stream has no device to
/// restart onto, `restart` and `restart_with` return the refusal
/// `InvalidDevice`, and the stream still runs.
#[test]
fn a_restart_that_resolves_no_device_leaves_the_stream_running() {
    let transport = Transport::new(SampleRate::SR_44K1);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_44K1,
        MaxBlock::of(Samples::new(512)),
    ));
    let engine = Engine::new(&transport, &mut ed, exec).expect("an empty graph");
    let state = Arc::new(AudioCallbackState::new(engine));
    let mut driver = TuttiDriver::new(AudioEngine::from_spec(spec_at(44_100.0)), state);
    let (supplied, stream) = ManualStreamDriver::new();
    driver.start_with(supplied).expect("starts");
    assert!(driver.stream().is_running() && stream.is_open());

    let err = driver
        .restart(DeviceSelector::Default)
        .expect_err("no device to restart onto");
    assert!(matches!(err, crate::DeviceError::InvalidDevice(_)), "{err}");
    assert!(
        driver.stream().is_running() && stream.is_open(),
        "restart stopped the stream"
    );

    let err = driver
        .restart_with(DeviceSelector::Default, |_, _| {
            Ok::<(), crate::DeviceError>(())
        })
        .expect_err("no device to restart onto");
    assert!(matches!(err, crate::DeviceError::InvalidDevice(_)), "{err}");
    assert!(
        driver.stream().is_running() && stream.is_open(),
        "restart_with stopped the stream"
    );
    drop(ed);
}

/// **A switch onto a supplied driver keeps the device on record**: the driver
/// still holds the device for the restart that opens it again, and names none
/// meanwhile.
#[test]
fn a_switch_to_a_supplied_driver_keeps_the_device_to_restart_onto() {
    let mut driver = driver_over(on_device(Opened::Named("Card".into())));
    let (manual, _stream) = ManualStreamDriver::new();
    driver
        .switch(manual_spec(), manual, |_, _| {
            Ok::<(), crate::DeviceError>(())
        })
        .unwrap();
    assert_eq!(driver.opened_name().unwrap(), None);
    driver
        .origin
        .target_mut()
        .expect("the driver still holds the device");
}
