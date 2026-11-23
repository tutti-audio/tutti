//! A `loom` model of [`RenderGate`]: the shipped code, whose atomics are loom's
//! under the flag (see `src/render_gate.rs`'s `sync` module).
//!
//! ```text
//! cargo test -p tutti-rt --release --test render_gate_loom   # with the loom cfg flag set
//! ```
//!
//! The plugin's state is a `loom::cell::UnsafeCell`: a render uses it inside
//! an admitted guard, the control side writes it while the gate is `Opening`
//! (init) or `Closing` (uninit). Loom reports any access pair not ordered
//! by happens-before. Over every interleaving:
//!
//! - **`enter` racing `close`**: `close` returns only after every admitted render
//!   dropped (the uninit write is ordered after the render's read), and an `enter`
//!   that is refused leaves no count behind;
//! - **no admission after `Closing`**: a render that sees the closed gate
//!   afterwards is refused;
//! - **`enter` racing `begin_open` and its finish**: an admitted render sees
//!   the init write;
//! - **`hold_closed` against `begin_open`**: exactly one wins.

#![cfg(loom)]

use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::thread;
use tutti_rt::{GateState, RenderGate};

/// What the OS init and uninit write and a render reads.
struct Plugin(UnsafeCell<u32>);

// SAFETY (test): shared across threads only through the gate's protocol, which
// is what the model checks.
unsafe impl Sync for Plugin {}

impl Plugin {
    fn read(&self) -> u32 {
        // SAFETY (test): a shared access loom checks.
        self.0.with(|p| unsafe { *p })
    }
    fn write(&self, v: u32) {
        // SAFETY (test): an exclusive access loom checks.
        self.0.with_mut(|p| unsafe { *p = v });
    }
}

#[test]
fn close_returns_only_after_every_admitted_render_dropped() {
    loom::model(|| {
        let gate = Arc::new(RenderGate::new());
        let plugin = Arc::new(Plugin(UnsafeCell::new(0)));
        gate.begin_open().expect("closed").finish();
        plugin.write(5);

        let renderer = {
            let (gate, plugin) = (gate.clone(), plugin.clone());
            thread::spawn(move || {
                if let Some(_guard) = gate.enter() {
                    assert_eq!(plugin.read(), 5, "an admitted render sees the live plugin");
                    true
                } else {
                    false
                }
            })
        };

        let closing = gate.close().expect("open");
        plugin.write(0); // the uninit: no admitted render is still reading
        closing.finish();
        let admitted = renderer.join().unwrap();
        assert_eq!(gate.state(), GateState::Closed);
        assert!(gate.enter().is_none(), "no admission after the close");
        // The count came back to zero whether or not the render was admitted.
        gate.begin_open().expect("closed").finish();
        assert!(
            gate.close().is_some(),
            "refused enters left no count (admitted: {admitted})"
        );
    });
}

#[test]
fn a_failed_uninit_reopens_without_losing_a_render() {
    loom::model(|| {
        let gate = Arc::new(RenderGate::new());
        gate.begin_open().expect("closed").finish();
        let renderer = {
            let gate = gate.clone();
            thread::spawn(move || drop(gate.enter()))
        };
        drop(gate.close().expect("open")); // the uninit failed
        renderer.join().unwrap();
        assert_eq!(gate.state(), GateState::Open);
        assert!(gate.close().is_some());
    });
}

#[test]
fn an_admitted_render_sees_the_init_write() {
    loom::model(|| {
        let gate = Arc::new(RenderGate::new());
        let plugin = Arc::new(Plugin(UnsafeCell::new(0)));
        let renderer = {
            let (gate, plugin) = (gate.clone(), plugin.clone());
            thread::spawn(move || {
                if let Some(_guard) = gate.enter() {
                    assert_eq!(plugin.read(), 9);
                }
            })
        };
        let opening = gate.begin_open().expect("closed");
        plugin.write(9);
        opening.finish();
        renderer.join().unwrap();
        assert_eq!(gate.state(), GateState::Open);
    });
}

#[test]
fn hold_closed_and_begin_open_are_exclusive() {
    loom::model(|| {
        let gate = Arc::new(RenderGate::new());
        let holder = {
            let gate = gate.clone();
            thread::spawn(move || {
                gate.hold_closed().map(|held| {
                    // While held, no opening is granted.
                    let refused = gate.begin_open().is_none();
                    held.finish();
                    refused
                })
            })
        };
        let opened = gate.begin_open().map(|opening| {
            // While opening, no hold is granted.
            let refused = gate.hold_closed().is_none();
            opening.finish();
            refused
        });
        if let Some(refused) = opened {
            assert!(refused, "a hold was granted during Opening");
        }
        if let Some(refused) = holder.join().unwrap() {
            assert!(refused, "an opening was granted during Held");
        }
    });
}

#[test]
fn a_held_gate_refuses_a_concurrent_begin_open() {
    loom::model(|| {
        let gate = Arc::new(RenderGate::new());
        let held = gate.hold_closed().expect("closed");
        let opener = {
            let gate = gate.clone();
            thread::spawn(move || gate.begin_open().is_none())
        };
        assert!(opener.join().unwrap(), "refused while held");
        drop(held);
        assert!(gate.begin_open().is_some());
    });
}

#[test]
fn an_unfinished_opening_admits_no_render_and_leaves_the_gate_closed() {
    loom::model(|| {
        let gate = Arc::new(RenderGate::new());
        let renderer = {
            let gate = gate.clone();
            thread::spawn(move || gate.enter().is_some())
        };
        drop(gate.begin_open().expect("closed"));
        assert!(
            !renderer.join().unwrap(),
            "no render during or after a failed open"
        );
        assert_eq!(gate.state(), GateState::Closed);
    });
}

#[test]
fn close_and_begin_open_are_granted_only_from_their_own_states() {
    // From Closed: close is refused, begin_open granted.
    loom::model(|| {
        let gate = Arc::new(RenderGate::new());
        let closer = {
            let gate = gate.clone();
            thread::spawn(move || match gate.close() {
                Some(closing) => {
                    closing.finish();
                    true
                }
                None => false,
            })
        };
        let opening = gate
            .begin_open()
            .expect("only the opener moves a closed gate");
        opening.finish();
        let closed = closer.join().unwrap();
        // A close granted only after the open finished, and it finished: the
        // gate ends Closed exactly when the closer was granted.
        let expected = if closed {
            GateState::Closed
        } else {
            GateState::Open
        };
        assert_eq!(gate.state(), expected);
    });
    // From Open: begin_open is refused, close granted.
    loom::model(|| {
        let gate = Arc::new(RenderGate::new());
        gate.begin_open().expect("closed").finish();
        let opener = {
            let gate = gate.clone();
            thread::spawn(move || gate.begin_open().is_none())
        };
        let closing = gate.close().expect("open");
        assert!(opener.join().unwrap(), "no open from Open or Closing");
        closing.finish();
        assert_eq!(gate.state(), GateState::Closed);
    });
}
