//! An open winmm input.
//!
//! # Who may touch what, and when
//!
//! winmm calls the input's callback on a thread of its own and documents no
//! wait for a callback in flight when a port is reset or closed. So the
//! callback is handed an *id*, not a pointer, and the open-callback-close
//! protocol (the registry, the count of callbacks inside, the closing flag and
//! the give-up on a stuck callback) is `lifetime`'s, which has no OS call and
//! is modelled under `loom`. This file is its edge:
//!
//! - dropping the input unregisters it first, so no callback that has not yet
//!   looked it up finds anything, then waits (bounded) for the callbacks already
//!   inside to leave; only a `Closed` it gets back from that lets it reset,
//!   unprepare and close the port, and a callback or close that cannot be
//!   settled is a `Leaked`, which keeps the port open and the state allocated;
//! - the callback is not confined to what a `MIDIINPROC` may call. It takes two
//!   user-mode locks, which are outside that list: the registry's, held only for
//!   a scan of the open inputs' slots, and the handler's `Mutex` in `dispatch`,
//!   held for the handler's call. It also calls `SetEvent` (through
//!   `Requeue::drop` and the worker's wake), which is on the list. It calls no
//!   other function of the system's, and the buffers go back to winmm only from
//!   the worker;
//! - the callback only hands the recorded bytes to the handler and marks the
//!   buffer done; a worker thread gives the buffer back to winmm, because
//!   `midiInAddBuffer` is not one of the calls a `MIDIINPROC` may make.

use std::cell::UnsafeCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tutti_platform::{Executor, Instant, Task, join};

use windows_sys::Win32::Media::Audio::{
    CALLBACK_FUNCTION, HMIDIIN, MIDIHDR, midiInAddBuffer, midiInClose, midiInOpen,
    midiInPrepareHeader, midiInReset, midiInStart, midiInUnprepareHeader,
};
use windows_sys::Win32::Media::{
    MM_MIM_CLOSE, MM_MIM_DATA, MM_MIM_LONGDATA, MM_MIM_LONGERROR, MMSYSERR_INVALHANDLE,
};

use tutti_ump::Ump;

use super::InputPort;
use super::error::{Error, MmOperation, OsOperation, check};
use super::sleeper::{TimerWaiter, TimerWaker, waitable_timer};
use crate::lifetime::{Outcome, Owner, Poll, Registry, Slot};
use crate::ump::{Stamps, unpack};

/// How many SysEx buffers an input keeps with winmm, and how long each is:
/// a message longer than one buffer arrives as consecutive [`Event::Long`]s.
const BUFFERS: usize = 4;
const BUFFER_BYTES: usize = 4096;

/// How long dropping an input waits for callbacks already running to return,
/// and how often it looks.
const LEAVE_BOUND: Duration = Duration::from_secs(1);
const LEAVE_POLL: Duration = Duration::from_millis(1);

/// What winmm delivered, with its stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// A short message (`MIM_DATA`) as a MIDI 1.0 UMP on Group 0: type 2 for a
    /// channel voice message, type 1 for a system one. Running status is
    /// already expanded. A status that is not a message is not delivered.
    Short {
        /// The message.
        ump: Ump,
        /// The time since the input started.
        at: Duration,
    },
    /// Bytes of a System Exclusive message (`MIM_LONGDATA`): a message that
    /// fills a buffer continues in the next event, with no `0xF7` between.
    Long {
        /// The bytes winmm recorded, valid for the call only.
        data: &'a [u8],
        /// The time since the input started.
        at: Duration,
    },
    /// The driver closed the port (`MIM_CLOSE`): the device was removed, or
    /// the driver was stopped. Nothing is delivered after it. Dropping the
    /// input then makes no call on the handle, which may be reused, and leaks
    /// its state (the buffers and the handler) for the life of the process,
    /// because the driver may still hold them: once per unplug of an open port.
    /// Not sent for the close that dropping the input makes.
    Closed,
}

/// The handler, and the widening of the stamps winmm gives it: both are used
/// under the one lock, one callback at a time.
struct Sink {
    handler: Box<dyn FnMut(Event<'_>) + Send>,
    stamps: Stamps,
}

/// One SysEx buffer. winmm holds the header's address while it is prepared and
/// writes the header and the bytes while the buffer is queued, so both are
/// behind `UnsafeCell`s and the buffer sits in the `Shared` allocation, which
/// never moves.
struct Buffer {
    header: UnsafeCell<MIDIHDR>,
    data: UnsafeCell<[u8; BUFFER_BYTES]>,
    /// Set by the callback when winmm has returned the buffer; cleared by the
    /// worker when it hands the buffer back.
    done: AtomicBool,
}

// SAFETY: `header` and `data` are written by winmm only while the buffer is
// queued with it, and read by this crate only in the callback that winmm makes
// when it returns the buffer, before the worker queues it again; the queued and
// returned states alternate, so no read overlaps a write. `done` is atomic.
unsafe impl Sync for Buffer {}
// SAFETY: the one pointer a buffer holds (the header's `lpData`) points into the
// buffer itself, and nothing is tied to the thread that made it.
unsafe impl Send for Buffer {}

impl Buffer {
    fn new() -> Self {
        Self {
            header: UnsafeCell::new(MIDIHDR::default()),
            data: UnsafeCell::new([0; BUFFER_BYTES]),
            done: AtomicBool::new(false),
        }
    }

    /// The header, pointing at this buffer's bytes. Every use goes through
    /// this one pointer.
    fn header(&self) -> *mut MIDIHDR {
        self.header.get()
    }
}

/// What the callback, the worker and the owner share.
struct Shared {
    handler: Mutex<Sink>,
    buffers: [Buffer; BUFFERS],
    /// The open port, set once `midiInOpen` has returned; the worker queues
    /// buffers on it.
    handle: AtomicPtr<core::ffi::c_void>,
    /// Set when the worker can no longer give buffers back: the input then
    /// records until its buffers run out and no longer.
    dead: AtomicBool,
    /// Wakes the worker: a buffer is done, or the input is closing.
    worker: TimerWaker,
}

/// The inputs that are open, by the id their callback is given.
static REGISTRY: Registry<Shared> = Registry::new();

/// An open input, started. Dropping it returns winmm's buffers and closes the
/// port.
///
/// A `MidiIn` exists only once its port is open, and the port is closed in
/// `Drop` and nowhere else, so every path out of `open` after the port opened
/// closes it exactly once.
///
/// After the driver closes the port (`Event::Closed`) the drop makes no call on
/// the handle, which winmm may have reused, and leaks the input's state (the
/// buffers and the handler) for the life of the process: the driver may still
/// hold the headers. That costs one state per unplug of an open port.
///
/// Dropping a `MidiIn` on the handler's own thread (inside the callback) leaks
/// the port: the drop waits for the callbacks in flight, the handler's own is
/// one of them, so the wait ends at its bound and the port stays open with its
/// buffers allocated for the life of the process. Drop it from another thread.
///
/// The handler is dropped with the input's state, which can be on winmm's
/// thread after the `MidiIn` is gone, so it must be fit to run there.
pub struct MidiIn {
    /// Taken by `Drop`.
    owner: Option<Owner<'static, Shared>>,
    handle: HMIDIIN,
    worker: Option<Task<()>>,
}

impl std::fmt::Debug for MidiIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiIn").finish_non_exhaustive()
    }
}

// SAFETY: the `HMIDIIN` is a token for a driver object; winmm takes it from any
// thread, and `MidiIn` uses it only in `Drop`, once. The rest of `MidiIn` is
// `Send` (an `Arc` of `Sync` state and the worker's task).
unsafe impl Send for MidiIn {}

fn header_size() -> u32 {
    size_of::<MIDIHDR>() as u32
}

impl MidiIn {
    /// Opens `port` and starts it: `handler` runs on winmm's
    /// thread for each message from then on.
    ///
    /// The handler is called from a thread winmm owns, one call at a time, and
    /// must return quickly and call no winmm function. Buffers are handed back
    /// to winmm by a future on a dedicated thread of `executor`, never by the
    /// callback.
    ///
    /// The worker blocks on a wait it cannot interrupt but by its wake, so it
    /// cannot see its executor shut down: an input still open when the last clone
    /// of its executor drops costs the executor's shutdown bound, and the thread is
    /// then detached with a warning (it ends when the input drops). Drop an input
    /// before its executor.
    ///
    /// `handler` is dropped when the last reference to the input's state goes,
    /// which can be on winmm's thread after the `MidiIn` has been dropped (a
    /// callback that was inside when the drop finished leaves last), so its
    /// destructor must be fit to run there.
    ///
    /// winmm answers the device interface query for a device id and refuses a
    /// handle, so the open cannot ask the open handle which device it is. It
    /// reads the port's position before `midiInOpen`, and opens only if the
    /// device there is the one listed, then reads it again once open and
    /// closes the port (with no reset) unless it still is. Only a shift
    /// between those two reads that puts the listed device back at the position
    /// goes unseen.
    ///
    /// # Errors
    ///
    /// [`Error::PortMoved`] when the device at the position is not the one
    /// listed (checked before and after the open); otherwise the failing call: `midiInOpen`
    /// ([`MmResult::Allocated`](crate::MmResult::Allocated) when another
    /// application holds the port exclusively,
    /// [`MmResult::BadDeviceId`](crate::MmResult::BadDeviceId) when it is
    /// gone), a buffer's preparation, `midiInStart`, or the worker's or its
    /// wake event's creation.
    pub fn open(
        executor: &Executor,
        port: &InputPort,
        handler: impl FnMut(Event<'_>) + Send + 'static,
    ) -> Result<Self, Error> {
        port.verify()?;
        let (wake, wait) = waitable_timer()?;
        let shared = Shared {
            handler: Mutex::new(Sink {
                handler: Box::new(handler),
                stamps: Stamps::default(),
            }),
            buffers: std::array::from_fn(|_| Buffer::new()),
            handle: AtomicPtr::new(std::ptr::null_mut()),
            dead: AtomicBool::new(false),
            worker: wake,
        };
        for buffer in &shared.buffers {
            // SAFETY: the header is not yet shared with winmm or any thread.
            unsafe {
                (*buffer.header()).lpData = buffer.data.get().cast();
                (*buffer.header()).dwBufferLength = BUFFER_BYTES as u32;
            }
        }
        let owner = REGISTRY.open(shared);
        let id = owner.id();
        let mut handle: HMIDIIN = std::ptr::null_mut();
        // SAFETY: `handle` is a live out-pointer. The callback has the
        // `MIDIINPROC` signature, and the instance value is an id, which names
        // no memory.
        let code = unsafe {
            midiInOpen(
                &raw mut handle,
                port.index(),
                callback as *const () as usize,
                id,
                CALLBACK_FUNCTION,
            )
        };
        // A failed open leaves nothing registered: the owner's drop removes it.
        check(MmOperation::MidiInOpen, code)?;
        // Until confirmed the open is a registration and a port: a refusal
        // unregisters and only closes, as nothing was queued or started, so no
        // reset is sent.
        if let Err(e) = port.verify() {
            drop(owner);
            // SAFETY: `handle` is the input just opened, closed once.
            unsafe { midiInClose(handle) };
            return Err(e);
        }
        let shared = owner.slot();
        shared.handle.store(handle, Ordering::Release);
        // The port is open: from here `input` owns it, and a failure drops
        // `input`, which closes the port (once, in `Drop`).
        let mut input = Self {
            owner: Some(owner),
            handle,
            worker: None,
        };
        let worker = {
            let shared = Arc::clone(&shared);
            // The worker stops by the input's own wake and closing flag, and
            // `Drop` waits for it to end before it closes the port.
            executor
                .spawn_dedicated("tutti-winmm-in", None, async move {
                    requeue(&shared, wait);
                })
                .map_err(|source| Error::Os {
                    operation: OsOperation::SpawnInputWorker,
                    source,
                })?
        };
        input.worker = Some(worker);
        for buffer in &shared.buffers {
            // SAFETY: `header` is this buffer's header, whose `lpData` is its
            // bytes; both live in `Shared`, which this input and the registry
            // keep allocated, unmoved, until the port is closed.
            let code = unsafe { midiInPrepareHeader(input.handle, buffer.header(), header_size()) };
            check(MmOperation::MidiInPrepareHeader, code)?;
            // SAFETY: the header was prepared just above.
            let code = unsafe { midiInAddBuffer(input.handle, buffer.header(), header_size()) };
            check(MmOperation::MidiInAddBuffer, code)?;
        }
        // SAFETY: `handle` is the open input.
        let code = unsafe { midiInStart(input.handle) };
        check(MmOperation::MidiInStart, code)?;
        Ok(input)
    }

    /// Whether the input still delivers: `false` once its worker could not
    /// give a buffer back to winmm, after which it goes quiet as its buffers
    /// fill.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|owner| !owner.dead.load(Ordering::Acquire))
    }
}

impl Drop for MidiIn {
    /// Stops the worker and callbacks, returns the buffers and closes the
    /// port; leaks the buffers if winmm may still hold them.
    fn drop(&mut self) {
        let Some(owner) = self.owner.take() else {
            return;
        };
        let shared = owner.slot();
        // No callback that looks the id up from here on finds the input.
        let closing = owner.begin_close(LEAVE_BOUND);
        // `midiInClose` runs only after the worker has ended, because a closed
        // handle may be reused by another port and the worker queues buffers on
        // it. A wake that failed would leave that wait for ever, so the task is
        // detached, the worker is left running and, with the port still open,
        // nothing is freed.
        if let Err(e) = shared.worker.wake() {
            tracing::warn!(
                "winmm MIDI input: the worker could not be woken, so the port is left open: {e}"
            );
            if let Some(task) = self.worker.take() {
                task.detach();
            }
            drop(closing.leak());
            return;
        }
        if let Some(task) = self.worker.take() {
            // The wake above is what ends the worker. A `None` means its future
            // panicked or was dropped unrun; it no longer touches the handle.
            let _ = join(task);
        }
        // Callbacks that found the input before it was removed finish. This
        // loop is the only place that reads the clock.
        let origin = Instant::now();
        let mut closing = closing;
        let closed = loop {
            match closing.poll(origin.elapsed()) {
                Poll::Quiesced(closed) => break closed,
                Poll::Waiting(again) => {
                    closing = again;
                    std::thread::sleep(LEAVE_POLL);
                }
                Poll::TimedOut(leaked) => {
                    // A callback has not left: it may be in the handler, or
                    // winmm may call it again. The port is left open and the
                    // allocation kept.
                    drop(leaked);
                    return;
                }
            }
        };
        // A close winmm did not complete may leave it holding the buffers: the
        // `Leaked` keeps the allocation for ever.
        if let Outcome::Leaked(leaked) = closed.teardown(|slot| self.teardown(slot)) {
            drop(leaked);
        }
    }
}

impl MidiIn {
    /// Resets and closes the port, once no callback is inside.
    fn teardown(&self, closed: &Slot<Shared>) -> bool {
        // `Closed::teardown` skips this closure after a driver `MIM_CLOSE`: the
        // handle is then invalid (Microsoft's `MIM_CLOSE` page) and the state is
        // leaked.
        // SAFETY: `handle` is the open input. A reset stops recording and
        // returns every queued buffer to its owner.
        unsafe { midiInReset(self.handle) };
        for buffer in &closed.buffers {
            // SAFETY: the header was prepared by `open`; a header that was
            // never prepared is accepted, and one still queued is refused.
            unsafe { midiInUnprepareHeader(self.handle, buffer.header(), header_size()) };
        }
        // SAFETY: `handle` is the open input, closed once.
        let code = unsafe { midiInClose(self.handle) };
        // A close that succeeded means winmm queues nothing of ours; one that
        // found the port already gone (the device was removed) means the same.
        // Any other result may leave winmm holding the buffers.
        code == 0 || code == MMSYSERR_INVALHANDLE
    }
}

/// Gives each done buffer back to winmm until the input closes.
fn requeue(shared: &Slot<Shared>, mut wait: TimerWaiter) {
    loop {
        if wait.wait().is_err() {
            shared.dead.store(true, Ordering::Release);
            return;
        }
        if shared.closing() {
            return;
        }
        for buffer in &shared.buffers {
            // Checked before each queueing: a handle the driver closed may be
            // another input's. No lock is taken, so a close in the driver's
            // thread is never waited on.
            if shared.driver_closed() {
                return;
            }
            if buffer.done.swap(false, Ordering::AcqRel) {
                let handle = shared.handle.load(Ordering::Acquire);
                // SAFETY: the header is prepared and has been returned to us
                // by the callback that set `done`; `handle` is the open port,
                // which is not closed until this thread has ended.
                let code = unsafe { midiInAddBuffer(handle, buffer.header(), header_size()) };
                if code != 0 {
                    shared.dead.store(true, Ordering::Release);
                }
            }
        }
    }
}

/// Hands a buffer back to the worker when dropped, so a handler that panics
/// does not lose it.
struct Requeue<'a> {
    shared: &'a Slot<Shared>,
    buffer: &'a Buffer,
}

impl Drop for Requeue<'_> {
    fn drop(&mut self) {
        if !self.shared.closing() {
            self.buffer.done.store(true, Ordering::Release);
            let _ = self.shared.worker.wake();
        }
    }
}

/// winmm's `MIDIINPROC`: runs on the driver's thread. `instance` is the id
/// `MidiIn::open` passed; `param1` and `param2` are the message's.
extern "system" fn callback(
    _handle: HMIDIIN,
    message: u32,
    instance: usize,
    param1: usize,
    param2: usize,
) {
    let Some(inside) = REGISTRY.enter(instance) else {
        return;
    };
    let shared: &Slot<Shared> = &inside;
    // The OS documents the handle as invalid from `MIM_CLOSE` on; recorded
    // before the closing early-out so a close in that window is not lost.
    if message == MM_MIM_CLOSE {
        shared.note_driver_closed();
    }
    if shared.closing() {
        return;
    }
    // A panic must not unwind into winmm; a panicking handler drops the event.
    // The payload is forgotten: dropping it here would run arbitrary code on
    // the driver's thread.
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
        dispatch(shared, message, param1, param2);
    })) {
        std::mem::forget(payload);
    }
}

/// Hands the handler `event(at)`, `at` being the time since the input started
/// of the stamp `millis`.
fn deliver<'a>(shared: &Slot<Shared>, millis: u32, event: &dyn Fn(Duration) -> Event<'a>) {
    let mut sink = shared
        .handler
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let at = sink.stamps.since_start(millis);
    (sink.handler)(event(at));
}

fn dispatch(shared: &Slot<Shared>, message: u32, param1: usize, param2: usize) {
    match message {
        MM_MIM_DATA => {
            if let Some(ump) = unpack(param1 as u32) {
                deliver(shared, param2 as u32, &|at| Event::Short { ump, at });
            }
        }
        // A bad SysEx message (`MIM_LONGERROR`) comes back with its buffer too:
        // nothing is delivered for it, and the buffer is given back like any.
        MM_MIM_LONGDATA | MM_MIM_LONGERROR => {
            // The header winmm names is matched against this input's own and
            // never dereferenced as given.
            let Some(buffer) = shared
                .buffers
                .iter()
                .find(|b| b.header() as usize == param1)
            else {
                return;
            };
            let _requeue = Requeue { shared, buffer };
            // SAFETY: winmm has returned the buffer (this is the message that
            // says so) and the worker has not queued it again, so nothing
            // writes the header; it is packed, so the field is read unaligned.
            let recorded =
                unsafe { (&raw const (*buffer.header()).dwBytesRecorded).read_unaligned() };
            let recorded = (recorded as usize).min(BUFFER_BYTES);
            if message == MM_MIM_LONGDATA && recorded > 0 {
                // SAFETY: as above, for the bytes: `recorded` of them are
                // initialised and nothing writes them until the buffer is
                // queued again, after this call.
                let bytes =
                    unsafe { std::slice::from_raw_parts(buffer.data.get().cast::<u8>(), recorded) };
                deliver(shared, param2 as u32, &|at| Event::Long { data: bytes, at });
            }
        }
        MM_MIM_CLOSE => {
            let mut sink = shared
                .handler
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (sink.handler)(Event::Closed);
        }
        _ => {}
    }
}
