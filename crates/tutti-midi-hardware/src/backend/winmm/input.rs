//! Reading MIDI 1.0 from a winmm input.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use super::super::legacy_port::anchor::Anchor;
use super::super::legacy_port::bytes::ByteIn;
use crate::InputProducerHandle;
use crate::endpoints::InputConnection;
use crate::error::Result;
use crate::guard::Guard;
use tutti_winmm_ffi::{Event, InputPort, MidiIn};

/// An open winmm input. Dropping it stops the port and returns its buffers.
pub struct WinMmInput {
    /// Set when the driver closes the port or the callback panics.
    died: Arc<AtomicBool>,
    port: MidiIn,
}

impl InputConnection for WinMmInput {
    fn is_alive(&self) -> bool {
        !self.died.load(Ordering::Acquire) && self.port.is_alive()
    }
}

/// Opens `port` and delivers what it receives into `producer`.
///
/// # Errors
///
/// [`Error::Os`](crate::Error::Os) from the port's opening, such as
/// [`MmResult::Allocated`](tutti_winmm_ffi::MmResult::Allocated) when another
/// application holds it.
pub(super) fn open(port: &InputPort, mut producer: InputProducerHandle) -> Result<WinMmInput> {
    let died = Arc::new(AtomicBool::new(false));
    let guard = Guard::new(Arc::clone(&died));
    let closed = Arc::clone(&died);
    let mut bytes = ByteIn::new();
    // Read before the port starts, so no stamp precedes it.
    let mut anchor = Anchor::new(Instant::now());
    // A full inlet counts the drop itself (the input's `lost` cell); a log
    // line here would run on winmm's thread.
    let mut push = move |ump, at| {
        let _ = producer.push(ump, at);
    };
    // winmm calls this on its own thread, and a panic must not leave it: the
    // guard catches it and ends the input.
    let port = MidiIn::open(port, move |event| {
        guard.run(|| {
            let seen = Instant::now();
            match event {
                Event::Short { message, millis } => {
                    if let Some(ump) = bytes.short(message) {
                        push(ump, anchor.arrival(millis, seen));
                    }
                }
                Event::Long { data, millis } => {
                    let at = anchor.arrival(millis, seen);
                    for ump in bytes.long(data) {
                        push(ump, at);
                    }
                }
                Event::Closed => closed.store(true, Ordering::Release),
            }
        });
    })?;
    Ok(WinMmInput { died, port })
}
