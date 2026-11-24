use super::*;
use std::sync::Arc;
use tutti_buffers::MaxBlock;
use tutti_buffers::SampleFormat;
use tutti_engine::{AudioCallbackState, Engine, ManualStreamDriver};
use tutti_graph::{Editor, Prepare};
use tutti_timeline::Transport;
use tutti_units::{SampleRate, Samples};

fn manual_spec() -> OutputSpec {
    OutputSpec::new(
        SampleRate::SR_48K,
        tutti_buffers::ChannelLayout::STEREO,
        SampleFormat::F32,
    )
}

/// The state over an empty graph at `rate`.
fn empty_state(rate: SampleRate) -> Arc<AudioCallbackState> {
    let transport = Transport::new(rate);
    let (mut ed, exec) = Editor::new(Prepare::new(rate, MaxBlock::of(Samples::new(512))));
    let graph = Engine::new(&transport, &mut ed, exec).expect("an empty graph");
    Arc::new(AudioCallbackState::new(graph))
}

fn driver_over(engine: AudioEngine) -> crate::TuttiDriver {
    crate::TuttiDriver::new(engine, empty_state(SampleRate::SR_48K))
}

/// **Naming the opened device asks the host nothing, and tells a supplied
/// driver from a device that would not be named.** The engine here has no
/// host at all, so any look at it would be an error: `Ok(Some)` is the name
/// the open recorded, `Ok(None)` a driver the caller supplied (no device, by
/// design), `Err` a device the host could not name. A start on a supplied
/// driver ends in `Ok(None)`.
#[test]
fn the_opened_name_tells_a_supplied_driver_from_an_unnamed_device() {
    let mut engine = AudioEngine::from_spec(manual_spec());
    assert_eq!(engine.opened_name().unwrap(), None, "a supplied driver");
    engine.opened = Opened::Named("Card".into());
    assert_eq!(engine.opened_name().unwrap().as_deref(), Some("Card"));
    engine.opened = Opened::Unnamed("no name".into());
    assert!(engine.opened_name().is_err(), "a device that would not say");
    engine.opened = Opened::Named("Card".into());

    let mut driver = driver_over(engine);
    let (manual, _stream) = ManualStreamDriver::new();
    driver
        .switch(manual_spec(), manual, |_, _| {
            Ok::<(), crate::DeviceError>(())
        })
        .unwrap();
    assert_eq!(driver.opened_name().unwrap(), None);
}

/// **A restart that fails leaves the name of the device still open.** The
/// hook refuses after the new spec is in, before any stream starts; the name
/// is recorded only once the stream runs.
#[test]
fn a_failed_restart_keeps_the_opened_name() {
    let mut engine = AudioEngine::from_spec(manual_spec());
    engine.opened = Opened::Named("Card".into());
    let mut driver = driver_over(engine);
    let (manual, _stream) = ManualStreamDriver::new();
    driver
        .switch(manual_spec(), manual, |_, _| {
            Err(crate::DeviceError::InvalidConfig("refused".into()))
        })
        .expect_err("the hook refuses");
    assert_eq!(driver.opened_name().unwrap().as_deref(), Some("Card"));
}

/// **Starting an engine whose stream runs is an error, and the running
/// stream is untouched.** Through `start_with` over a `ManualStreamDriver`
/// (`start` resolves a real device first, so it needs hardware; it ends in
/// the same `OutputStream::start_at` refusal). After `stop` a start succeeds
/// again.
#[test]
fn starting_a_running_engine_is_an_error() {
    let state = empty_state(SampleRate::SR_44K1);
    let mut engine = AudioEngine::from_spec(OutputSpec::new(
        SampleRate::SR_44K1,
        tutti_buffers::ChannelLayout::STEREO,
        SampleFormat::F32,
    ));
    let (first, first_stream) = ManualStreamDriver::new();
    engine
        .start_with(Arc::clone(&state), first)
        .expect("starts");
    assert!(engine.stream().is_running() && first_stream.is_open());

    let (second, second_stream) = ManualStreamDriver::new();
    let err = engine
        .start_with(Arc::clone(&state), second)
        .expect_err("a running engine refuses a second start");
    assert!(matches!(err, DeviceError::AlreadyRunning), "{err}");
    assert!(first_stream.is_open(), "the running stream is untouched");
    assert!(!second_stream.is_open(), "the refused driver never opened");

    engine.stop();
    let (third, third_stream) = ManualStreamDriver::new();
    engine.start_with(state, third).expect("starts after stop");
    assert!(third_stream.is_open());
}

/// **A restart onto another device asks it for the same sample format**: naming
/// a new device replaces the device and keeps the chosen format.
#[test]
fn selecting_another_device_keeps_the_chosen_sample_format() {
    let mut config =
        OutputConfig::from(DeviceSelector::named("A")).sample_format(SampleFormat::I16);
    config.select(DeviceSelector::named("B"));
    assert_eq!(
        config,
        OutputConfig::from(DeviceSelector::named("B")).sample_format(SampleFormat::I16)
    );
}

/// **Selecting a device on a spec-built engine is refused at the call.** The
/// engine has no device to select, so the call names the same refusal `start`
/// would, on the engine and on the driver wrapping one.
#[test]
fn selecting_a_device_on_a_spec_built_engine_is_refused() {
    let mut engine = AudioEngine::from_spec(manual_spec());
    assert!(matches!(
        engine.select_device(DeviceSelector::Default),
        Err(DeviceError::InvalidDevice(_))
    ));
    let mut driver = driver_over(engine);
    assert!(matches!(
        driver.select_device(DeviceSelector::Default),
        Err(DeviceError::InvalidDevice(_))
    ));
}
