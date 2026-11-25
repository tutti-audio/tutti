//! Reading MIDI 1.0 from a winmm input.

use std::sync::Arc;
use tutti_platform::{Executor, Instant};

use super::super::legacy_port::anchor::Anchor;
use super::super::legacy_port::bytes::ByteIn;
use crate::InputFault;
use crate::InputProducerHandle;
use crate::endpoints::InputConnection;
use crate::error::Result;
use crate::guard::Guard;
use tutti_rt::FaultLatch;
use tutti_winmm_ffi::{Event, InputPort, MidiIn};

/// An open winmm input. Dropping it stops the port and returns its buffers.
pub(super) struct WinMmInput {
    /// Raised when the driver closes the port or the callback panics.
    died: Arc<FaultLatch<InputFault>>,
    port: MidiIn,
}

impl InputConnection for WinMmInput {
    fn fault(&self) -> Option<InputFault> {
        // The latch holds what the callback raised. The port's own flag, which
        // its worker sets when it cannot return a buffer, is read each time and
        // not latched.
        self.died
            .cause()
            .or_else(|| (!self.port.is_alive()).then_some(InputFault::BufferLost))
    }
}

/// Opens `port` and delivers what it receives into `producer`.
///
/// # Errors
///
/// [`MidiHardwareError::Os`] from the port's opening. The endpoint's `open_input`
/// turns `MmResult::Allocated`, another application holding the port, into
/// [`MidiHardwareError::InUse`].
pub(super) fn open(
    executor: &Executor,
    port: &InputPort,
    mut producer: InputProducerHandle,
) -> Result<WinMmInput> {
    let died = Arc::new(FaultLatch::<InputFault>::new());
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
    let port = MidiIn::open(executor, port, move |event| {
        guard.run(|| {
            let seen = Instant::now();
            match event {
                Event::Short { ump, at } => push(ump, anchor.arrival(at, seen)),
                Event::Long { data, at } => {
                    let at = anchor.arrival(at, seen);
                    for ump in bytes.long(data) {
                        push(ump, at);
                    }
                }
                Event::Closed => closed.raise(InputFault::DriverClosed),
            }
        });
    })?;
    Ok(WinMmInput { died, port })
}
