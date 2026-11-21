//! The process's one CoreMIDI client.
//!
//! CoreMIDI sends a client's notifications to the run loop of the thread that
//! created the process's first client, and coremidi never disposes a client on
//! drop (disposing the last one may make the MIDI server exit and fail every later
//! client creation). So the process keeps one client for its life, made once by a
//! [`StartClient`] polled on the thread whose run loop should receive the
//! notifications; every endpoint created through this crate is created on it.

use super::error::{Error, Operation, OsStatus};
use crate::Notification;
use crate::checks::backoff::Failure;
use crate::checks::client_start::ClientStart;
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{MIDIClientCreate, MIDIClientRef, MIDINotification};
use std::ffi::c_void;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::task::Poll;
use tutti_platform::Instant;

/// What a client's notifications are delivered to.
type Callback = Box<dyn Fn(Notification) + Send + Sync>;

/// Whether the process's client is made, and whether a start is under way.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// No start exists and no client was made.
    Free,
    /// A [`StartClient`] exists.
    Starting,
    /// The client is made.
    Started,
}

static CLAIM: Mutex<Claim> = Mutex::new(Claim::Free);

fn claim() -> std::sync::MutexGuard<'static, Claim> {
    CLAIM.lock().unwrap_or_else(|p| p.into_inner())
}

/// The process's one CoreMIDI client: the only way to hold one is a
/// [`StartClient`] that has finished, which makes the client once, so a second
/// client is unwritable through this crate. A copy of it names the same client.
///
/// `Send` and `Sync` without an `unsafe impl`: the client is an opaque `u32`
/// reference into CoreMIDI, whose objects are internally synchronised.
#[derive(Clone, Copy, Debug)]
pub struct SharedClient {
    reference: MIDIClientRef,
}

impl SharedClient {
    /// The client's reference, for creating an endpoint on it.
    pub(crate) fn get(self) -> MIDIClientRef {
        self.reference
    }
}

/// The creation of the process's client, named `name`, in progress.
///
/// CoreMIDI refuses a client creation with a status while its server exits, for
/// about five seconds. A `StartClient` is polled with the current time: a poll makes
/// an attempt only when one is due, [`deadline`](Self::deadline) is the instant the
/// next poll can make progress, and the creation ends in the client or in
/// [`Error::RetryTimedOut`] when the bound passes. It waits for nothing itself.
///
/// Notifications arrive on the run loop of the thread that polls it to completion,
/// and only while that thread runs it, so one `StartClient` stays on one thread:
///
/// ```compile_fail,E0277
/// fn send<T: Send>() {}
/// send::<tutti_coremidi_ffi::StartClient>();
/// ```
#[must_use = "a start that is not polled makes no client"]
pub struct StartClient {
    name: CFString,
    // The leaked `Box<Callback>` CoreMIDI is given; freed by `Drop` only while no
    // client holds it.
    callback: *mut Callback,
    state: ClientStart,
    made: bool,
    // Makes the type `!Send` and `!Sync` whatever its fields are.
    _thread: PhantomData<*const ()>,
}

impl core::fmt::Debug for StartClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StartClient")
            .field("made", &self.made)
            .finish_non_exhaustive()
    }
}

impl StartClient {
    /// A creation of the process's client, named `name`, whose first attempt is
    /// due at `now`, delivering each notification it receives to `on_notification`.
    ///
    /// `gave_up` is the instant an earlier creation's window ended in failure
    /// ([`gave_up_at`](Self::gave_up_at)): within ten seconds of it the server is
    /// still known to be gone, so this creation makes its one attempt and does not
    /// retry.
    ///
    /// The closure runs on the polling thread, inside CoreMIDI: it must not wait
    /// on a call into CoreMIDI. A panic in it is caught and the notification is
    /// lost.
    ///
    /// # Errors
    ///
    /// [`Error::ClientExists`] when the client has been started, or another start
    /// exists.
    pub fn new<F>(
        name: &str,
        on_notification: F,
        now: Instant,
        gave_up: Option<Instant>,
    ) -> Result<Self, Error>
    where
        F: Fn(Notification) + Send + Sync + 'static,
    {
        let mut claimed = claim();
        if *claimed != Claim::Free {
            return Err(Error::ClientExists);
        }
        *claimed = Claim::Starting;
        let callback: Callback = Box::new(on_notification);
        Ok(Self {
            name: CFString::new(name),
            callback: Box::into_raw(Box::new(callback)),
            state: ClientStart::new(now, gave_up),
            made: false,
            _thread: PhantomData,
        })
    }

    /// When the next poll can make progress: the next attempt's instant.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.state.deadline()
    }

    /// The instant this creation's window ended in failure, once it has: the
    /// server was gone for the whole window. Pass it to the next creation.
    #[must_use]
    pub fn gave_up_at(&self) -> Option<Instant> {
        self.state.gave_up_at()
    }

    /// One poll at `now`: makes a `MIDIClientCreate` attempt only when one is due.
    ///
    /// `Pending` means try again at [`deadline`](Self::deadline). Polling after
    /// the start finished makes no attempt and reports [`Error::ClientExists`].
    ///
    /// # Errors
    ///
    /// [`Error::Call`] when CoreMIDI refuses the creation with a status that is not
    /// retried, and [`Error::RetryTimedOut`] when the server stayed gone until the
    /// bound.
    pub fn poll(&mut self, now: Instant) -> Poll<Result<SharedClient, Error>> {
        if self.made {
            return Poll::Ready(Err(Error::ClientExists));
        }
        let (name, callback) = (&self.name, self.callback);
        let polled = self.state.poll(now, || create(name, callback));
        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(reference)) => {
                self.made = true;
                *claim() = Claim::Started;
                Poll::Ready(Ok(SharedClient { reference }))
            }
            Poll::Ready(Err(Failure::Failed(status))) => {
                Poll::Ready(Err(Error::call(Operation::CreateClient, status)))
            }
            Poll::Ready(Err(Failure::TimedOut { last })) => {
                Poll::Ready(Err(Error::RetryTimedOut {
                    operation: Operation::CreateClient,
                    last,
                }))
            }
        }
    }
}

/// One `MIDIClientCreate` attempt.
fn create(name: &CFString, callback: *mut Callback) -> Result<MIDIClientRef, OsStatus> {
    let mut client: MIDIClientRef = 0;
    // SAFETY: `name` outlives the call and `client` is a valid out-pointer.
    // `callback` is a `Box<Callback>` leaked by `StartClient::new`, which `notify`
    // reads as one; it is freed only if no client was created (`Drop`), so it is
    // live whenever CoreMIDI calls `notify`.
    let status = unsafe {
        MIDIClientCreate(
            name.as_concrete_TypeRef(),
            Some(notify),
            callback.cast::<c_void>(),
            &mut client,
        )
    };
    OsStatus::check(status).map(|()| client)
}

impl Drop for StartClient {
    fn drop(&mut self) {
        if self.made {
            // The client keeps the callback for the process's life.
            return;
        }
        *claim() = Claim::Free;
        // SAFETY: no client was created, so CoreMIDI holds no reference to
        // `callback`, which `Box::into_raw` made and nothing else freed.
        drop(unsafe { Box::from_raw(self.callback) });
    }
}

/// The `MIDINotifyProc` of the client: reads the message and calls the closure
/// the client was started with.
///
/// # Safety
///
/// CoreMIDI calls it with a `message` valid for the call and the `context` that
/// [`StartClient::new`] made.
unsafe extern "C" fn notify(message: *const MIDINotification, context: *mut c_void) {
    // SAFETY: `message` is valid for the call (CoreMIDI's contract), and `context`
    // is the leaked `Callback` of `StartClient::new`, never freed once a client
    // holds it.
    let (message, callback) = unsafe { (&*message, &*context.cast::<Callback>()) };
    let notification = Notification::from_message_id(message.messageID);
    // A panic must not unwind into CoreMIDI's frames.
    let _ = catch_unwind(AssertUnwindSafe(|| callback(notification)));
}

/// The client of a test process: started once, whichever test needs it first.
#[cfg(test)]
pub(crate) fn test_client() -> SharedClient {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<Result<SharedClient, Error>> = OnceLock::new();
    *CLIENT
        .get_or_init(|| {
            let mut start = StartClient::new("tutti-ffi-test", |_| {}, Instant::now(), None)?;
            // A test may wait for the server: until the start's own deadline.
            loop {
                if let Poll::Ready(done) = start.poll(Instant::now()) {
                    return done;
                }
                let deadline = start.deadline();
                tutti_fixtures::wait_until(
                    "the next attempt's instant",
                    std::time::Duration::from_secs(30),
                    || Instant::now() >= deadline,
                );
            }
        })
        .as_ref()
        .expect("CoreMIDI client")
}
