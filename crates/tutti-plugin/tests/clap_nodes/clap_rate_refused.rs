//! **A plugin that refuses the engine's rate is silent, visibly, and retried.**
//!
//! The graph re-prepares the node at a new rate and the host tells the plugin.
//! A plugin that refuses (CLAP `activate` returns false) rolls back to the
//! rate it had, or stays deactivated, while the host's blocks and transport
//! are at the new rate: left running it would sound pitch-shifted. Instead
//! the server renders silence for it, the host reports
//! `PluginStatus::RateRefused` (not `Dead`), and every later rate change
//! tries it again; one it takes clears the error and the plugin renders.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

use crate::clap_probe;
use clap_probe::{ProbeEnv, Rig, load_probe, render};
use tutti_buffers::MaxBlock;
use tutti_fixtures::machine_lock;

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_platform::Instant;

use tutti_graph::Prepare;
use tutti_plugin_host::{PluginHandle, PluginStatus};
use tutti_plugin_types::{PluginInvalidation, RateRefusalCause};
use tutti_units::{SampleRate, Samples};

/// The rate the plugin is loaded at, and accepts.
const LOAD_RATE: f64 = 48_000.0;
/// The rate the probe refuses.
const REFUSED_RATE: f64 = 96_000.0;
/// Another rate the probe accepts.
const OTHER_RATE: f64 = 44_100.0;
const BLOCK: usize = 64;
/// A hang guard on waiting for the plugin's status to change.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(60);

/// A value neither silence nor anything the probe adds to it.
const INPUT_DC: f32 = 7.0;
/// `INPUT_DC` plus the probe's `TagPassthrough` tag on port 0, channel 0.
const LIVE: f32 = INPUT_DC + 1.0;

/// One block, then the wait for the server's answer to it: the pipeline never
/// waits for a reply. A plugin that is not alive (refused, or dead) answers
/// nothing, and is not waited for.
fn drive_block(unit: &mut Rig, handle: &PluginHandle) -> Vec<f32> {
    let out = unit.run(BLOCK, |_, _| INPUT_DC);
    clap_probe::await_answer_or(
        &unit.controls,
        "the server's answer, or the plugin to stop being alive",
        || !handle.status().is_alive(),
    );
    out.into_iter().next().expect("the probe has outputs")
}

fn reprepare(unit: &mut Rig, rate: f64) {
    unit.renderer()
        .editor_mut()
        .reprepare(Prepare::new(
            SampleRate::of(rate),
            MaxBlock::of(Samples::new(BLOCK)),
        ))
        .expect("the graph re-prepares");
}

/// Drive blocks (a re-prepare reaches the node at the next block) until
/// `want` holds of the status, or the budget runs out.
fn wait_for(
    unit: &mut Rig,
    handle: &PluginHandle,
    mut want: impl FnMut(&PluginStatus) -> bool,
) -> PluginStatus {
    let deadline = Instant::now() + NOTICE_TIMEOUT;
    loop {
        let _ = drive_block(unit, handle);
        let status = handle.status();
        if want(&status) || Instant::now() >= deadline {
            return status;
        }
    }
}

fn refused_at(status: &PluginStatus, rate: f64) -> bool {
    matches!(status, PluginStatus::RateRefused(refusal) if refusal.rate.get() == rate && matches!(refusal.cause, RateRefusalCause::Deactivated { .. } | RateRefusalCause::RolledBack { .. }))
}

/// Drive until two consecutive blocks are live. Blocks are submitted faster
/// than the server answers them and the bridge skips the ones it is behind on,
/// so how many blocks that takes is the machine's; the wait is for the event,
/// and only a hang ends it early.
fn renders_live(unit: &mut Rig, handle: &PluginHandle) -> bool {
    let deadline = Instant::now() + NOTICE_TIMEOUT;
    let mut live = 0;
    while Instant::now() < deadline {
        if drive_block(unit, handle).iter().all(|&s| s == LIVE) {
            live += 1;
            if live >= 2 {
                return true;
            }
        } else {
            live = 0;
        }
    }
    false
}

fn assert_silent(unit: &mut Rig, handle: &PluginHandle, what: &str) {
    for block in 0..4 {
        let out = drive_block(unit, handle);
        assert!(
            out.iter().all(|&s| s == 0.0),
            "{what}: block {block} is silent, not the plugin at the wrong rate: {out:?}"
        );
    }
}

/// (The dropout count is pinned inside the cycle below.)
///
/// The whole cycle on one plugin: running, refused (silent, visible, not
/// dead, an invalidation raised), refused again, revived by a rate it takes,
/// and a drop during a refusal that returns.
#[test]
fn a_refused_rate_is_silent_visible_and_retried() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_PASSTHROUGH)
        .refuse_rate(REFUSED_RATE);
    let probe = load_probe(LOAD_RATE);
    let handle = probe.handle.clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let _subscription = handle.subscribe(move |n| {
        if let tutti_plugin_host::PluginSignal::Invalidate(e) = n {
            sink.lock().unwrap().push(e);
        }
    });

    let mut unit = Rig::new(
        probe.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
        LOAD_RATE,
        BLOCK,
    );
    assert!(
        renders_live(&mut unit, &handle),
        "the plugin renders at {LOAD_RATE}"
    );
    assert!(handle.status().is_alive());

    // The graph moves to a rate the plugin refuses.
    reprepare(&mut unit, REFUSED_RATE);
    let status = wait_for(&mut unit, &handle, |s| !s.is_alive());
    assert!(refused_at(&status, REFUSED_RATE), "{status:?}");
    assert!(!status.is_dead() && !handle.is_crashed());
    let missed = unit.controls.missed_chunks();
    assert_silent(&mut unit, &handle, "refused");
    assert_eq!(
        unit.controls.missed_chunks(),
        missed,
        "a refused plugin's silence is deliberate, not a dropout"
    );
    assert!(
        events.lock().unwrap().iter().any(
            |e| matches!(e, PluginInvalidation::RateRefused(refusal) if refusal.rate.get() == REFUSED_RATE)
        ),
        "{:?}",
        events.lock().unwrap()
    );

    // Asked again at the same rate: refused again, still silent.
    let refusals = || {
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, PluginInvalidation::RateRefused(refusal) if refusal.rate.get() == REFUSED_RATE))
            .count()
    };
    reprepare(&mut unit, REFUSED_RATE);
    let _ = wait_for(&mut unit, &handle, |_| refusals() >= 2);
    assert_eq!(refusals(), 2, "the second ask is refused and announced too");
    assert!(refused_at(&handle.status(), REFUSED_RATE));
    assert_silent(&mut unit, &handle, "refused again");

    // A rate the plugin takes brings it back.
    reprepare(&mut unit, OTHER_RATE);
    let status = wait_for(&mut unit, &handle, |s| s.is_alive());
    assert!(status.is_alive(), "the error clears: {status:?}");
    assert!(renders_live(&mut unit, &handle), "the plugin renders again");
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PluginInvalidation::RateRestored)),
        "{:?}",
        events.lock().unwrap()
    );

    // Refused once more, then dropped: the teardown returns.
    reprepare(&mut unit, REFUSED_RATE);
    let status = wait_for(&mut unit, &handle, |s| !s.is_alive());
    assert!(refused_at(&status, REFUSED_RATE), "{status:?}");
    // The drop returns: run on a thread, under a hang guard.
    let (done, returned) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(unit);
        drop(probe.handle);
        drop(handle);
        let _ = done.send(());
    });
    returned
        .recv_timeout(Duration::from_secs(60))
        .expect("dropping a refused plugin never returned");
}
