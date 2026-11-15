//! Who disposes an in-process Audio Unit that is dropped while a graph renders it.
//!
//! An `AudioComponentInstance` is disposed on the thread it was made on. The render thread
//! holds the unit through the [`AuProcessor`]; the control side holds it through the
//! controller. Whichever drops last must not dispose it on the render thread: the disposal
//! goes to the owner's main-thread collector and runs at its next tick.

#![cfg(target_os = "macos")]

use std::sync::mpsc;

use tutti_au_host::AuLoaded;
use tutti_fixtures::main_thread;
use tutti_plugin_types::{Load, Loaded, MainWork, MaxBlock, Prepare, SampleRate};
use tutti_types::Samples;

const FRAMES: usize = 64;

/// The unit is alive while the render thread still renders it, however the control side lets
/// go; the render thread's drop of the last reference disposes nothing, it hands the
/// disposal to the collector, and the control side's tick disposes it, once.
///
/// Spec: AudioComponent.h, `AudioComponentInstanceDispose`: the instance is disposed once,
/// on the thread that owns it.
#[test]
fn an_in_process_au_dropped_while_rendered_is_disposed_by_the_control_tick() {
    let main = main_thread();
    let (work, handle) = MainWork::new();
    let loaded = AuLoaded::load(
        main,
        &handle,
        &tutti_fixtures::au_bundle(),
        &Load::new().plugin(tutti_shared_plugin::AU_ID),
    )
    .expect("the shared AU loads");
    let prepare = Prepare::new(SampleRate::of(48_000.0), MaxBlock::of(Samples(FRAMES)));
    let (mut processor, controller) = loaded
        .initialize(prepare)
        .unwrap_or_else(|(_, error)| panic!("the shared AU activates: {error}"));

    let (rendered_tx, rendered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let render_thread = std::thread::spawn(move || {
        let input = [[0.0f32; FRAMES]; 2];
        let mut left = [0.0f32; FRAMES];
        let mut right = [0.0f32; FRAMES];
        let input: Vec<&[f32]> = input.iter().map(|c| c.as_slice()).collect();
        processor
            .render_f32(&input, &mut [&mut left, &mut right], FRAMES as u32)
            .expect("renders");
        rendered_tx.send(()).expect("the control side waits");
        release_rx.recv().expect("the control side releases");
        // The last reference to the unit goes here, off its home thread.
        drop(processor);
    });
    rendered_rx.recv().expect("the render thread rendered");

    // The control side lets go first. The render thread still holds the unit, so nothing is
    // disposed and nothing is pending for the tick.
    let before = MainWork::deferred_drops();
    drop(controller);
    assert_eq!(
        MainWork::deferred_drops(),
        before,
        "the control side's drop was not the last reference"
    );
    assert_eq!(work.run(main), 0, "nothing was disposed while it renders");

    // The render thread's drop is the last: it hands the disposal off.
    release_tx.send(()).expect("the render thread waits");
    render_thread.join().expect("the render thread drops");
    assert_eq!(
        MainWork::deferred_drops(),
        before + 1,
        "the last drop, on the render thread, was deferred to the collector"
    );
    assert!(
        work.run(main) >= 1,
        "the disposal was still pending: the render thread did not run it"
    );
    assert_eq!(work.run(main), 0, "disposed once");
}
