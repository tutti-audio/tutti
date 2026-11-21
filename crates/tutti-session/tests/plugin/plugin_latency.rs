//! A hosted plugin's latency is the session's, and a restart onto another
//! callback size re-declares it (design 015 §4.3), over the reference CLAP
//! plugin in a real `plugin-server`.
//!
//! `Session::crossfade_plugin` installs no meter on a crossfade's incoming
//! client.
//!
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`):
//! it is not a dev-dependency (a cycle through `tutti-plugin`), and without it
//! every test here fails, naming that command. They must fail rather than
//! skip.

use crate::pooled::{LoadPlugin, Spec};
use crate::support;

use tutti_buffers::MaxBlock;
use tutti_platform::Instant;

use support::clap_probe;
use tutti_buffers::ChannelLayout;
use tutti_engine::{ManualStreamDriver, OutputSpec};
use tutti_plugin::PluginClient;
use tutti_plugin_db::Loadable;
use tutti_plugin_types::PluginRef;
use tutti_session::{
    Bounce, BounceLatency, BounceTail, BufferMaster, Crossfade, ExportClock, OutPort, PHASES,
    Phase, PhaseCx, Range, Root, Session, SessionEvent,
};
use tutti_session::{PluginChange, PluginEvent};
use tutti_units::{SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 1_024;

/// SAFETY: nextest runs each test in its own process, and the server's path
/// is set before the first server is spawned, from the test's own thread.
fn set_server() {
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
}

/// An offline stereo session at 48 kHz whose block is the plugin's pipeline
/// chunk ([`BLOCK`]: with no device callback size known, a plugin ships its
/// chunk at the graph's `MaxBlock`).
fn offline() -> Session {
    set_server();
    Session::builder()
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .build()
        .expect("an offline session builds")
}

fn add_probe_on_outputs(s: &mut Session) -> tutti_graph::NodeKey {
    let node = s
        .load_plugin_blocking(
            tutti_fixtures::main_thread(),
            Spec::new(PluginRef::file(clap_probe())),
        )
        .expect("the reference CLAP plugin loads");
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(node, 0)])
        .unwrap();
    let _ = s.update();
    node.erase()
}

/// **The graph plans PDC against a plugin's declared latency, and a change
/// of it is the session's phase's.** The node's latency is the plugin's own
/// figure plus the one chunk its out-of-process pipeline holds; the plugin
/// takes MIDI and automation on one event input. Idle updates leave the graph
/// clean (the phase compares the plugin's declared figure with the editor's
/// and, while they agree, edits nothing: no recompile and no republish frame
/// after frame).
#[test]
fn a_plugins_latency_is_the_sessions_and_follows_the_plugin() {
    let mut s = offline();
    let node = add_probe_on_outputs(&mut s);

    assert_eq!(
        s.graph_ref().node_event_inputs(node).unwrap(),
        1,
        "a plugin takes MIDI (and automation) on its node's event input"
    );
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples::new(tutti_clap_test_plugin::REPORTED_LATENCY_SAMPLES as usize + BLOCK),
        "the graph plans PDC against the node's whole latency"
    );

    // Idle updates leave the graph clean: observed between the plugins phase
    // and the commit (which clears the edit flag).
    let now = Instant::now();
    for _ in 0..5 {
        let mut out = Vec::new();
        for &phase in PHASES {
            s.run_phase(phase, PhaseCx::at(now), &mut out);
            if phase == Phase::Plugins {
                assert!(
                    !s.graph_ref().is_edited(),
                    "an unchanged plugin latency marked the graph edited"
                );
            }
        }
        assert!(
            !out.iter().any(|e| matches!(
                e,
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::LatencyChanged { .. },
                    ..
                })
            )),
            "{out:?}"
        );
    }
}

/// **The graph holds the tail ceiling the plugin claims** (`clap.tail`), apart
/// from its latency: the node's shape declares it through
/// `Processing::ceiling`, an unmeasured plugin's claim, and the session's
/// phase leaves it alone while the plugin's figure stands.
#[test]
fn a_plugins_tail_ceiling_is_the_plugins_claim() {
    let mut s = offline();
    let node = add_probe_on_outputs(&mut s);
    assert_eq!(
        s.graph_ref().node_tail(node).unwrap(),
        tutti_units::Tail::Finite(Samples::new(
            tutti_clap_test_plugin::REPORTED_TAIL_SAMPLES as usize
        )),
    );
    let now = Instant::now();
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, PhaseCx::at(now), &mut out);
        if phase == Phase::Plugins {
            assert!(
                !s.graph_ref().is_edited(),
                "an unchanged plugin tail marked the graph edited"
            );
        }
    }
}

/// A short offline bounce into memory: the render mode a plugin is told
/// Offline for as long as it is in flight.
fn request() -> Bounce<tutti_graph::NodeKey, BufferMaster> {
    Bounce::project()
        .master_buffers()
        .channels(ChannelLayout::STEREO)
        .range(Range::Seconds(
            tutti_units::Seconds::new(0.0),
            tutti_units::Seconds::new(0.05),
        ))
        .sample_rate(RATE)
        .clock(ExportClock::frozen())
        .latency(BounceLatency::Keep)
        .tail(BounceTail::None)
}

/// **A latency the plugin reports reaches the graph through the session's
/// phase.** The reference plugin adds `TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY`
/// frames to its latency while it renders offline and says so (`latency.changed`):
/// a bounce in flight is that report. It is `diagnostics().reported_latency()` at
/// once, and the graph's (`latency()`, the report plus the pipeline's chunk) at
/// the next update, with a `LatencyChanged`.
#[test]
fn a_latency_the_plugin_reports_reaches_the_graph() {
    // The plugin's file is probed before its variables are set.
    crate::pooled::prime(&PluginRef::file(clap_probe()));
    // SAFETY: as `set_server`: before the first server, from the test's thread.
    unsafe { std::env::set_var("TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY", "300") };
    let mut s = offline();
    let node = add_probe_on_outputs(&mut s);
    let own = tutti_clap_test_plugin::REPORTED_LATENCY_SAMPLES as usize;
    assert_eq!(
        diagnostics_of(&s, node).reported_latency(),
        Samples::new(own)
    );
    assert_eq!(
        diagnostics_of(&s, node).latency(),
        Samples::new(own + BLOCK)
    );

    let _job = s.bounce_start(request()).expect("the bounce starts");
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while diagnostics_of(&s, node).reported_latency() != Samples::new(own + 300) {
        assert!(Instant::now() < deadline, "the report never arrived");
        round_trip(&s, node);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        diagnostics_of(&s, node).latency(),
        Samples::new(own + BLOCK),
        "the graph has not taken it yet"
    );

    let events = s.update();
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples::new(own + 300 + BLOCK),
        "a changed plugin latency reaches the graph"
    );
    assert_eq!(
        diagnostics_of(&s, node).latency(),
        Samples::new(own + 300 + BLOCK)
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Plugin(PluginEvent { node: n, change: PluginChange::LatencyChanged { latency }, .. })
                if *n == node && *latency == Samples::new(own + 300 + BLOCK)
        )),
        "{events:?}"
    );
}

/// **A plugin crossfaded in is the one the latency phase reads.** The outgoing
/// plugin announces a longer latency than the incoming one's, so the figure says
/// which plugin the session's record follows: after `crossfade_plugin` the graph
/// plans against the incoming plugin's figure plus the chunk, and a later change
/// of the incoming plugin's latency is the one the phase hands the graph (its
/// record moved to the incoming plugin). The node keeps its key.
#[test]
fn a_crossfaded_plugins_latency_is_the_incoming_ones() {
    // The probe adds this to its 137 when told to render offline, and announces it
    // through `clap.latency`.
    const OFFLINE_EXTRA: usize = 74;
    const OWN: usize = tutti_clap_test_plugin::REPORTED_LATENCY_SAMPLES as usize;
    // The plugin's file is probed before its variables are set.
    crate::pooled::prime(&PluginRef::file(clap_probe()));
    // SAFETY: as `set_server`: before the first server, from the test's thread.
    unsafe {
        std::env::set_var(
            "TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY",
            OFFLINE_EXTRA.to_string(),
        );
    }
    // Tells `handle`'s plugin a render mode and waits for the latency it announces.
    fn told(handle: &tutti_plugin_host::PluginHandle, mode: tutti_plugin_types::RenderMode) {
        let (reported, heard) = std::sync::mpsc::channel();
        let _hearing = handle.subscribe(move |notice| {
            if matches!(
                notice,
                tutti_plugin_host::PluginSignal::Invalidate(
                    tutti_plugin_types::PluginInvalidation::Latency { .. }
                )
            ) {
                let _ = reported.send(());
            }
        });
        assert!(
            handle.announce_render_mode(mode),
            "the plugin takes the mode"
        );
        heard
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the plugin announced its latency");
    }
    let mut s = offline();
    let node = add_probe_on_outputs(&mut s);
    let outgoing_handle = s.plugin_view(node).expect("hosted").handle().clone();
    told(&outgoing_handle, tutti_plugin_types::RenderMode::Offline);
    let _ = s.update();
    let outgoing = s.graph_ref().node_latency(node).unwrap();
    assert_eq!(outgoing, Samples::new(OWN + OFFLINE_EXTRA + BLOCK));

    let incoming = PluginClient::new(
        tutti_plugin_ipc::IpcConfig::at(
            tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source"),
        ),
        tutti_fixtures::executor(),
        &tutti_fixtures::detached_work(),
        &Loadable::uncatalogued(PluginRef::file(clap_probe())),
        RATE,
    )
    .expect("load a second instance of the reference plugin");
    let incoming_handle = incoming.handle();
    let landed = s
        .graph()
        .crossfade_plugin(
            node,
            Box::new(incoming),
            tutti_units::Seconds::new(0.005),
            tutti_graph::FadeLaw::EqualAmplitude,
        )
        .expect("the crossfade is accepted");
    assert!(matches!(landed, Crossfade::Landed(())), "nothing waits");
    let _ = s.update();
    let _ = s.update();

    assert!(s.graph_ref().contains(node), "the node keeps its key");
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples::new(OWN + BLOCK),
        "the graph plans against the incoming plugin's declared latency, not the outgoing one's"
    );
    told(&incoming_handle, tutti_plugin_types::RenderMode::Offline);
    let _ = s.update();
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples::new(OWN + OFFLINE_EXTRA + BLOCK),
        "the latency phase must read the incoming plugin"
    );
}

/// **A restart onto a device with another callback size re-declares a hosted
/// plugin's latency, and PDC follows.** A live plugin ships one device
/// callback per chunk to its server, so its declared latency is its own 137
/// frames plus the callback: 137 + 512 on the first device, 137 + 256 once the
/// restart re-prepares the graph for the second (`OutputSpec::quantum` ->
/// `Prepare::quantum`). The session's total latency (the plugin alone on
/// output 0) is the new figure before the first block.
#[test]
fn a_restart_to_another_callback_size_re_declares_a_plugins_latency() {
    fn spec(quantum: usize) -> OutputSpec {
        OutputSpec::new(
            RATE,
            ChannelLayout::STEREO,
            tutti_buffers::SampleFormat::F32,
        )
        .with_quantum(MaxBlock::of(Samples::new(quantum)))
    }
    set_server();
    let (driver, stream) = ManualStreamDriver::new();
    let mut s = Session::builder()
        .device(Default::default())
        .build_device_on(spec(512), driver)
        .expect("builds with no device");

    let client = PluginClient::new(
        tutti_plugin_ipc::IpcConfig::at(
            tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source"),
        ),
        tutti_fixtures::executor(),
        &tutti_fixtures::detached_work(),
        &Loadable::uncatalogued(PluginRef::file(clap_probe())),
        RATE,
    )
    .expect("the reference plugin loads");
    let node = s.graph().insert_plugin(Box::new(client)).erase();
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(node, 0)])
        .unwrap();
    let _ = s.update();
    let _ = stream
        .render_block(Samples::new(512))
        .expect("the stream is open");
    let _ = s.update();
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples::new(137 + 512),
        "one 512-frame callback"
    );
    assert_eq!(s.latency(), s.graph_ref().node_latency(node).unwrap());

    let (driver, _new) = ManualStreamDriver::new();
    s.restart_on(spec(256), driver, None, None)
        .expect("restarts");
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples::new(137 + 256),
        "one 256-frame callback"
    );
    assert_eq!(
        s.latency(),
        Samples::new(137 + 256),
        "PDC follows before the first block"
    );
}

/// **A plugin keeps its handle across a clock switch and its latency follows
/// the callback size of each clock**: on the system clock the callback is the
/// timer's block; on the card it is the card's quantum. The node renders on
/// both, its handle still answers (a state save round trips to the server), and
/// the session's latency is the figure of the clock it is on.
#[test]
fn a_clock_switch_re_declares_a_plugins_latency_and_keeps_its_handle() {
    set_server();
    let mut s = Session::builder()
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples::new(480)))
        .system_clock()
        .build()
        .expect("builds");
    let client = PluginClient::new(
        tutti_plugin_ipc::IpcConfig::at(
            tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source"),
        ),
        tutti_fixtures::executor(),
        &tutti_fixtures::detached_work(),
        &Loadable::uncatalogued(PluginRef::file(clap_probe())),
        RATE,
    )
    .expect("the reference plugin loads");
    let node = s.graph().insert_plugin(Box::new(client)).erase();
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(node, 0)])
        .unwrap();
    let _ = s.update();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = s.update();
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples::new(137 + 480),
        "one timer block"
    );

    let (driver, stream) = ManualStreamDriver::new();
    let mut live = s
        .into_device_on(
            OutputSpec::new(
                RATE,
                ChannelLayout::STEREO,
                tutti_buffers::SampleFormat::F32,
            )
            .with_quantum(MaxBlock::of(Samples::new(256))),
            driver,
        )
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    let _ = stream
        .render_block(Samples::new(256))
        .expect("the stream is open");
    let _ = live.update();
    assert_eq!(
        live.graph_ref().node_latency(node).unwrap(),
        Samples::new(137 + 256),
        "one 256-frame callback"
    );
    assert_eq!(live.latency(), Samples::new(137 + 256));
    assert!(
        live.plugin_view(node)
            .expect("hosted")
            .handle()
            .save_state(tutti_plugin_types::StateUse::Project)
            .is_ok(),
        "the handle still answers"
    );

    let mut system = live
        .into_system_clock()
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    let _ = system.update();
    assert!(
        system
            .plugin_view(node)
            .expect("hosted")
            .handle()
            .save_state(tutti_plugin_types::StateUse::Project)
            .is_ok(),
        "and on the timer"
    );
    assert_eq!(
        system.graph_ref().node_latency(node).unwrap(),
        Samples::new(137 + 480),
        "one timer block again"
    );
}

fn diagnostics_of(s: &Session, node: tutti_graph::NodeKey) -> tutti_session::Diagnostics<'_> {
    s.plugin_view(node).expect("hosted").diagnostics()
}

/// One control round trip to the probe's server: the server sends the
/// `latency.changed` of a render mode after that call's reply, and the bridge
/// thread reads it off the socket only while it waits for the next reply.
fn round_trip(s: &Session, node: tutti_graph::NodeKey) {
    let _ = s
        .plugin_view(node)
        .expect("hosted")
        .handle()
        .save_state(tutti_plugin_types::StateUse::Project);
}
