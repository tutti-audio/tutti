//! The process's one CoreMIDI client.
//!
//! CoreMIDI sends a client's notifications to the run loop of the thread that
//! created the process's first client, and coremidi never disposes a client on
//! drop (disposing the last one may make the MIDI server exit and fail every later
//! client creation). So the process keeps one client for its life, made once by
//! [`SharedClient::start`] on the thread whose run loop should receive the
//! notifications; every endpoint created through this crate is created on it.

use super::client_retry::create_client;
use super::error::{Error, Operation, OsStatus};
use crate::Notification;
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{MIDIClientCreate, MIDIClientRef, MIDINotification};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

/// What a client's notifications are delivered to.
type Callback = Box<dyn Fn(Notification) + Send + Sync>;

/// The client, once started. Held across a creation so a second `start` waits for
/// the first and then finds it.
static CLIENT: Mutex<Option<MIDIClientRef>> = Mutex::new(None);

/// The process's one CoreMIDI client: the only way to hold one is
/// [`start`](Self::start), which makes the client once, so a second client is
/// unwritable through this crate. A copy of it names the same client.
///
/// `Send` and `Sync` without an `unsafe impl`: the client is an opaque `u32`
/// reference into CoreMIDI, whose objects are internally synchronised.
#[derive(Clone, Copy, Debug)]
pub struct SharedClient {
    reference: MIDIClientRef,
}

impl SharedClient {
    /// Creates the process's client, named `name`, and delivers each notification
    /// it receives to `on_notification`.
    ///
    /// Notifications arrive on the run loop of the calling thread, and only while
    /// that thread runs it. The closure runs on that thread, inside CoreMIDI: it
    /// must not wait on a call into CoreMIDI. A panic in it is caught and the
    /// notification is lost.
    ///
    /// The creation retries the status CoreMIDI returns while its server exits, for
    /// a bounded time, and a failed creation is not kept: a later call tries again.
    ///
    /// # Errors
    ///
    /// [`Error::ClientExists`] when the client has been started, and
    /// [`Error::Call`] when CoreMIDI refuses it.
    pub fn start<F>(name: &str, on_notification: F) -> Result<Self, Error>
    where
        F: Fn(Notification) + Send + Sync + 'static,
    {
        let mut slot = CLIENT.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return Err(Error::ClientExists);
        }
        let name = CFString::new(name);
        let callback: *mut Callback = Box::into_raw(Box::new(Box::new(on_notification)));
        let created = create_client(|| {
            let mut client: MIDIClientRef = 0;
            // SAFETY: `name` outlives the call and `client` is a valid out-pointer.
            // `callback` is a `Box<Callback>` leaked above, which `notify` reads as
            // one; it is freed only if no client was created (below), so it is
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
        });
        match created {
            Ok(reference) => {
                *slot = Some(reference);
                Ok(Self { reference })
            }
            Err(status) => {
                // SAFETY: no client was created, so CoreMIDI holds no reference to
                // `callback`, which `Box::into_raw` made and nothing else freed.
                drop(unsafe { Box::from_raw(callback) });
                Err(Error::call(Operation::CreateClient, status))
            }
        }
    }

    /// The client's reference, for creating an endpoint on it.
    pub(crate) fn get(self) -> MIDIClientRef {
        self.reference
    }
}

/// The `MIDINotifyProc` of the client: reads the message and calls the closure
/// the client was started with.
///
/// # Safety
///
/// CoreMIDI calls it with a `message` valid for the call and the `context` that
/// [`SharedClient::start`] gave it.
unsafe extern "C" fn notify(message: *const MIDINotification, context: *mut c_void) {
    // SAFETY: `message` is valid for the call (CoreMIDI's contract), and `context`
    // is the leaked `Callback` of `start`, never freed once a client holds it.
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
        .get_or_init(|| SharedClient::start("tutti-ffi-test", |_| {}))
        .as_ref()
        .expect("CoreMIDI client")
}
