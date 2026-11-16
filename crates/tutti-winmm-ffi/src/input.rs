//! An open winmm input.
//!
//! # Who may touch what, and when
//!
//! winmm calls the input's callback on a thread of its own and documents no
//! wait for a callback in flight when a port is reset or closed. So the
//! callback is handed an *id*, not a pointer, and finds its state in a
//! process-wide registry that holds one `Arc` per open input:
//!
//! - the callback looks its id up and clones the `Arc` under the registry
//!   lock, so what it touches stays allocated for as long as it runs;
//! - dropping the input removes it from the registry first, so no callback
//!   that has not yet looked it up finds anything, then waits (bounded) for
//!   the callbacks already inside to leave;
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
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Media::Audio::{
    CALLBACK_FUNCTION, HMIDIIN, MIDIHDR, midiInAddBuffer, midiInClose, midiInOpen,
    midiInPrepareHeader, midiInReset, midiInStart, midiInUnprepareHeader,
};
use windows_sys::Win32::Media::{
    MM_MIM_CLOSE, MM_MIM_DATA, MM_MIM_LONGDATA, MM_MIM_LONGERROR, MMSYSERR_INVALHANDLE,
};

use crate::deadline::Deadline;
use crate::error::{Error, check};
use crate::sleeper::Sleeper;

/// How many SysEx buffers an input keeps with winmm, and how long each is:
/// a message longer than one buffer arrives as consecutive [`Event::Long`]s.
const BUFFERS: usize = 4;
const BUFFER_BYTES: usize = 4096;

/// How long dropping an input waits for callbacks already running to return.
const LEAVE_BOUND: Duration = Duration::from_secs(1);

/// What winmm delivered, with its stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// A short message (`MIM_DATA`): the status byte and its data bytes, low
    /// byte first, as winmm packs them. Running status is already expanded.
    Short {
        /// The packed message.
        message: u32,
        /// Milliseconds since the input started.
        millis: u32,
    },
    /// Bytes of a System Exclusive message (`MIM_LONGDATA`): a message that
    /// fills a buffer continues in the next event, with no `0xF7` between.
    Long {
        /// The bytes winmm recorded, valid for the call only.
        data: &'a [u8],
        /// Milliseconds since the input started.
        millis: u32,
    },
    /// The driver closed the port (`MIM_CLOSE`): the device was removed, or
    /// the driver was stopped. Nothing is delivered after it. Not sent for
    /// the close that dropping the input makes.
    Closed,
}

type Handler = Box<dyn FnMut(Event<'_>) + Send>;

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
    handler: Mutex<Handler>,
    buffers: [Buffer; BUFFERS],
    /// The open port, set once `midiInOpen` has returned; the worker queues
    /// buffers on it.
    handle: AtomicPtr<core::ffi::c_void>,
    /// Set when the worker can no longer give buffers back: the input then
    /// records until its buffers run out and no longer.
    dead: AtomicBool,
    /// Set before the input resets: nothing is handed back to winmm after.
    closing: AtomicBool,
    /// Callbacks inside their body; incremented under the registry lock.
    in_flight: AtomicUsize,
    /// Wakes the worker: a buffer is done, or the input is closing.
    worker: Sleeper,
}

/// The inputs that are open, by the id their callback is given.
static REGISTRY: Mutex<Vec<(usize, Arc<Shared>)>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

fn registry() -> std::sync::MutexGuard<'static, Vec<(usize, Arc<Shared>)>> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An open input, started. Dropping it returns winmm's buffers and closes the
/// port.
///
/// Drop must not run on the handler's thread (the callback): it waits for the
/// callbacks in flight, and the handler's own would be one of them.
pub struct MidiIn {
    id: usize,
    handle: HMIDIIN,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for MidiIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiIn").finish_non_exhaustive()
    }
}

// SAFETY: the `HMIDIIN` is a token for a driver object; winmm takes it from any
// thread, and `MidiIn` uses it only in `Drop`, once. The rest of `MidiIn` is
// `Send` (an `Arc` of `Sync` state and a `JoinHandle`).
unsafe impl Send for MidiIn {}

fn header_size() -> u32 {
    size_of::<MIDIHDR>() as u32
}

impl MidiIn {
    /// Opens input port `index` and starts it: `handler` runs on winmm's
    /// thread for each message from then on.
    ///
    /// The handler is called from a thread winmm owns, one call at a time, and
    /// must return quickly and call no winmm function. Buffers are handed back
    /// to winmm by a thread of this input's own, never by the callback.
    ///
    /// # Errors
    ///
    /// The failing call: `midiInOpen` (`MMSYSERR_ALLOCATED` when another
    /// application holds the port exclusively, `MMSYSERR_BADDEVICEID` when it
    /// is gone), a buffer's preparation, `midiInStart`, or the worker's or
    /// its wake event's creation.
    pub fn open(
        index: u32,
        handler: impl FnMut(Event<'_>) + Send + 'static,
    ) -> Result<Self, Error> {
        let shared = Arc::new(Shared {
            handler: Mutex::new(Box::new(handler)),
            buffers: std::array::from_fn(|_| Buffer::new()),
            handle: AtomicPtr::new(std::ptr::null_mut()),
            dead: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            worker: Sleeper::new()?,
        });
        for buffer in &shared.buffers {
            // SAFETY: the header is not yet shared with winmm or any thread.
            unsafe {
                (*buffer.header()).lpData = buffer.data.get().cast();
                (*buffer.header()).dwBufferLength = BUFFER_BYTES as u32;
            }
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        registry().push((id, Arc::clone(&shared)));
        let mut handle: HMIDIIN = std::ptr::null_mut();
        // SAFETY: `handle` is a live out-pointer. The callback has the
        // `MIDIINPROC` signature, and the instance value is an id, which names
        // no memory.
        let code = unsafe {
            midiInOpen(
                &raw mut handle,
                index,
                callback as *const () as usize,
                id,
                CALLBACK_FUNCTION,
            )
        };
        if let Err(e) = check("midiInOpen", code) {
            unregister(id);
            return Err(e);
        }
        shared.handle.store(handle, Ordering::Release);
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("tutti-winmm-in".to_string())
                .spawn(move || requeue(&shared))
        };
        let worker = match worker {
            Ok(worker) => worker,
            Err(source) => {
                let mut input = Self {
                    id,
                    handle,
                    shared,
                    worker: None,
                };
                input.close();
                return Err(Error::Spawn {
                    operation: "spawn the input worker",
                    source,
                });
            }
        };
        let input = Self {
            id,
            handle,
            shared,
            worker: Some(worker),
        };
        // From here a failure drops `input`, which closes the port.
        for buffer in &input.shared.buffers {
            // SAFETY: `header` is this buffer's header, whose `lpData` is its
            // bytes; both live in `Shared`, which this input and the registry
            // keep allocated, unmoved, until the port is closed.
            let code = unsafe { midiInPrepareHeader(input.handle, buffer.header(), header_size()) };
            check("midiInPrepareHeader", code)?;
            // SAFETY: the header was prepared just above.
            let code = unsafe { midiInAddBuffer(input.handle, buffer.header(), header_size()) };
            check("midiInAddBuffer", code)?;
        }
        // SAFETY: `handle` is the open input.
        let code = unsafe { midiInStart(input.handle) };
        check("midiInStart", code)?;
        Ok(input)
    }

    /// Whether the input still delivers: `false` once its worker could not
    /// give a buffer back to winmm, after which it goes quiet as its buffers
    /// fill.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.shared.dead.load(Ordering::Acquire)
    }

    /// Stops the worker and callbacks, returns the buffers and closes the
    /// port; leaks the buffers if winmm may still hold them.
    fn close(&mut self) {
        // No callback that looks the id up from here on finds the input.
        unregister(self.id);
        self.shared.closing.store(true, Ordering::Release);
        // The worker queues no buffer after it has been joined.
        let _ = self.shared.worker.wake();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // Callbacks that found the input before it was removed finish.
        let leave = Deadline::after(Instant::now(), LEAVE_BOUND);
        while self.shared.in_flight.load(Ordering::Acquire) > 0 && !leave.passed(Instant::now()) {
            std::thread::yield_now();
        }
        if self.shared.in_flight.load(Ordering::Acquire) > 0 {
            // A callback has not left: it may be in the handler, or winmm may
            // call it again. The port is left open and the allocation kept.
            std::mem::forget(Arc::clone(&self.shared));
            return;
        }
        // SAFETY: `handle` is the open input. A reset stops recording and
        // returns every queued buffer to its owner.
        unsafe { midiInReset(self.handle) };
        for buffer in &self.shared.buffers {
            // SAFETY: the header was prepared by `open`; a header that was
            // never prepared is accepted, and one still queued is refused.
            unsafe { midiInUnprepareHeader(self.handle, buffer.header(), header_size()) };
        }
        // SAFETY: `handle` is the open input, closed once.
        let closed = unsafe { midiInClose(self.handle) };
        // A close that succeeded means winmm queues nothing of ours; one that
        // found the port already gone (the device was removed) means the same.
        // Any other result may leave winmm holding the buffers, so the
        // allocation is kept for ever.
        if closed != 0 && closed != MMSYSERR_INVALHANDLE {
            std::mem::forget(Arc::clone(&self.shared));
        }
    }
}

impl Drop for MidiIn {
    fn drop(&mut self) {
        self.close();
    }
}

fn unregister(id: usize) {
    registry().retain(|(registered, _)| *registered != id);
}

/// Gives each done buffer back to winmm until the input closes.
fn requeue(shared: &Shared) {
    loop {
        if shared.worker.wait(None).is_err() {
            shared.dead.store(true, Ordering::Release);
            return;
        }
        if shared.closing.load(Ordering::Acquire) {
            return;
        }
        for buffer in &shared.buffers {
            if buffer.done.swap(false, Ordering::AcqRel) {
                let handle = shared.handle.load(Ordering::Acquire);
                // SAFETY: the header is prepared and has been returned to us
                // by the callback that set `done`; `handle` is the open port,
                // which is not closed until this thread has been joined.
                let code = unsafe { midiInAddBuffer(handle, buffer.header(), header_size()) };
                if code != 0 {
                    shared.dead.store(true, Ordering::Release);
                }
            }
        }
    }
}

/// Leaves the callback: the count drops when this does, however the body ends.
struct Inside(Arc<Shared>);

impl Drop for Inside {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Hands a buffer back to the worker when dropped, so a handler that panics
/// does not lose it.
struct Requeue<'a> {
    shared: &'a Shared,
    buffer: &'a Buffer,
}

impl Drop for Requeue<'_> {
    fn drop(&mut self) {
        if !self.shared.closing.load(Ordering::Acquire) {
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
    let inside = {
        let registered = registry();
        let Some((_, shared)) = registered.iter().find(|(id, _)| *id == instance) else {
            return;
        };
        shared.in_flight.fetch_add(1, Ordering::AcqRel);
        Inside(Arc::clone(shared))
    };
    let shared: &Shared = &inside.0;
    if shared.closing.load(Ordering::Acquire) {
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

fn dispatch(shared: &Shared, message: u32, param1: usize, param2: usize) {
    let call = |event: Event<'_>| {
        let mut handler = shared
            .handler
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        handler(event);
    };
    match message {
        MM_MIM_DATA => call(Event::Short {
            message: param1 as u32,
            millis: param2 as u32,
        }),
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
                call(Event::Long {
                    data: bytes,
                    millis: param2 as u32,
                });
            }
        }
        MM_MIM_CLOSE => call(Event::Closed),
        _ => {}
    }
}
