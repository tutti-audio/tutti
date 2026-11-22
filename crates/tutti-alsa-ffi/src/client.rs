//! A UMP-mode ALSA sequencer client, and the queue clock it can start.

use std::ffi::CString;
use std::time::Duration;

use crate::address::{Address, ClientId, PortCaps, PortId, PortKind};
use crate::error::{Error, Operation, Result, address_byte, check};
use crate::event::Event;
use crate::records::{PoolRecord, PortRecord, StatusRecord};
use crate::sys;
use tutti_ump::Ump;

/// `-EAGAIN`: nothing queued yet on a non-blocking client.
pub(crate) const EAGAIN: i32 = -11;

/// A name as a C string.
///
/// # Errors
///
/// [`Error::NulInName`] when `name` has a NUL, which a C string cannot hold.
pub(crate) fn c_name(name: &str) -> Result<CString> {
    CString::new(name).map_err(|source| Error::NulInName { source })
}

/// What [`Client::read_event`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// One event.
    Event(Event),
    /// Nothing queued (`-EAGAIN`).
    Empty,
}

/// The MIDI protocol a UMP client asks the kernel to deliver (and to convert
/// to, at the port boundary, from what a source sends).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UmpVersion {
    /// MIDI 1.0 in UMP packets (type 2): a MIDI 1.0 source arrives as it is, and
    /// a MIDI 2.0 source is narrowed by the kernel.
    Midi1,
    /// MIDI 2.0 Protocol in UMP packets (type 4): a MIDI 1.0 source is
    /// up-converted by the kernel.
    Midi2,
}

impl UmpVersion {
    fn raw(self) -> i32 {
        match self {
            Self::Midi1 => sys::SND_SEQ_CLIENT_UMP_MIDI_1_0,
            Self::Midi2 => sys::SND_SEQ_CLIENT_UMP_MIDI_2_0,
        }
    }
}

/// An open sequencer client that speaks UMP. Owns the `snd_seq_t` and closes it
/// on drop.
///
/// It is `Send` and not `Sync`: one thread uses it at a time. A client that
/// several threads use lives in a [`Poller`](crate::Poller), which locks it per
/// call.
///
/// A client is non-blocking: [`read_event`](Client::read_event) answers
/// [`Read::Empty`] when nothing is queued, and a send into a full destination is
/// refused with `-EAGAIN`. One client per connection, not one per backend, so one
/// device going quiet stalls no other's reads.
#[derive(Debug)]
pub struct Client {
    seq: *mut sys::snd_seq_t,
    /// The client's number: what a sent event names as its source.
    pub(crate) id: ClientId,
    /// A test's stand-in for a failing or a held sequencer, run before each
    /// read ([`set_read_hook`](Self::set_read_hook)).
    #[cfg(feature = "testing")]
    read_hook: std::cell::RefCell<Option<ReadHook>>,
}

/// What [`Client::set_read_hook`] holds.
#[cfg(feature = "testing")]
struct ReadHook(Box<dyn FnMut() -> std::io::Result<()> + Send>);

#[cfg(feature = "testing")]
impl std::fmt::Debug for ReadHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadHook")
    }
}

/// Clients this process has opened, for a test that a path opens none.
#[cfg(feature = "testing")]
static OPENED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many clients this process has opened.
#[cfg(feature = "testing")]
#[doc(hidden)]
#[must_use]
pub fn clients_opened() -> usize {
    OPENED.load(std::sync::atomic::Ordering::SeqCst)
}

// SAFETY: a `Client` owns its `snd_seq_t` outright (no other copy of the
// pointer exists, and `Drop` closes it once), and alsa-lib ties a handle to no
// thread, so moving the owner to another thread moves every use with it. The
// `*mut` field keeps it `!Sync`, so two threads never call into the handle at
// once: it is used by one thread at a time.
unsafe impl Send for Client {}

impl Client {
    /// Opens a client in UMP mode with `version`, named `name` to other clients.
    ///
    /// Three steps, and all three matter:
    /// 1. `snd_seq_open`: an ordinary sequencer client.
    /// 2. `snd_seq_set_client_midi_version(version)`: **this is what makes
    ///    it a UMP client.** Without it the UMP event calls have nothing to
    ///    carry.
    /// 3. `snd_seq_set_client_ump_conversion(1)`: let the kernel translate at
    ///    the port boundary, so this client can talk to legacy ports and vice
    ///    versa. Kernel 6.5 or newer. Almost no Linux machine has a native-UMP
    ///    device yet, so without this the backend would be correct and useless.
    ///
    /// # Errors
    ///
    /// The step that failed, with alsa-lib's code.
    pub fn open(name: &str, version: UmpVersion) -> Result<Self> {
        let client = Self::open_plain(name)?;
        client.set_ump_version(version)?;
        // SAFETY: `client.seq` is open.
        let rc = unsafe { sys::snd_seq_set_client_ump_conversion(client.seq, 1) };
        check(rc, Operation::SetClientUmpConversion)?;
        Ok(client)
    }

    /// Changes the protocol this UMP client is delivered, for the events that
    /// arrive after the call. A caller that learns its source's protocol only
    /// from a query on an open client sets it before it subscribes.
    ///
    /// # Errors
    ///
    /// `snd_seq_set_client_midi_version`'s code.
    pub fn set_ump_version(&self, version: UmpVersion) -> Result<()> {
        // SAFETY: `self.seq` is open.
        let rc = unsafe { sys::snd_seq_set_client_midi_version(self.seq, version.raw()) };
        check(rc, Operation::SetClientMidiVersion).map(drop)
    }

    /// `snd_seq_open` and the name: a client that is not yet a UMP client.
    fn open_plain(name: &str) -> Result<Self> {
        // `"hw"`, not `"default"`. The sequencer is a kernel device, and `hw`
        // addresses it directly; `default` routes through a config-file plugin
        // that, on at least Ubuntu, **cannot be opened non-blocking**: it
        // returns `-ENOENT` for `SND_SEQ_NONBLOCK`, which read as "no MIDI
        // inputs" rather than as an open failure.
        let device = c"hw";
        let mut seq: *mut sys::snd_seq_t = std::ptr::null_mut();

        // SAFETY: `seq` is a valid out-pointer; `device` is a NUL-terminated
        // literal that outlives the call.
        let rc = unsafe {
            sys::snd_seq_open(
                &mut seq,
                device.as_ptr(),
                sys::SND_SEQ_OPEN_DUPLEX,
                sys::SND_SEQ_NONBLOCK,
            )
        };
        check(rc, Operation::Open)?;
        // Owned from here: any failure below closes it by `Drop`.
        #[cfg(feature = "testing")]
        OPENED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut client = Self {
            seq,
            id: ClientId::of(0),
            #[cfg(feature = "testing")]
            read_hook: std::cell::RefCell::new(None),
        };

        let cname = c_name(name)?;
        // SAFETY: `client.seq` is open; `cname` outlives the call.
        unsafe { sys::snd_seq_set_client_name(client.seq, cname.as_ptr()) };

        // SAFETY: `client.seq` is open.
        let rc = unsafe { sys::snd_seq_client_id(client.seq) };
        let id = check(rc, Operation::ClientId)?;
        client.id = ClientId::from_int(id).ok_or(Error::Number { what: "client", id })?;
        Ok(client)
    }

    /// Runs `hook` before each [`read_event`](Self::read_event) on this client,
    /// on the reading thread: an `Err` is that read's failure, and a hook that
    /// blocks holds the reader.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub fn set_read_hook(&self, hook: impl FnMut() -> std::io::Result<()> + Send + 'static) {
        *self.read_hook.borrow_mut() = Some(ReadHook(Box::new(hook)));
    }

    /// This client's sequencer id, for addressing events back to it.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    #[must_use]
    pub fn id(&self) -> ClientId {
        self.id
    }

    /// Creates a port on this client with `caps` and `kind`, and returns its number.
    ///
    /// # Errors
    ///
    /// `snd_seq_create_simple_port`'s code, or [`Error::Number`] for a number
    /// that is no port.
    pub fn open_port(&self, name: &str, caps: PortCaps, kind: PortKind) -> Result<PortId> {
        let cname = c_name(name)?;
        // SAFETY: `self.seq` is open; `cname` outlives the call.
        let rc = unsafe {
            sys::snd_seq_create_simple_port(self.seq, cname.as_ptr(), caps.bits(), kind.bits())
        };
        let id = check(rc, Operation::CreateSimplePort)?;
        PortId::from_int(id).ok_or(Error::Number { what: "port", id })
    }

    /// Subscribes `port` to what the port at `source` sends.
    ///
    /// # Errors
    ///
    /// `snd_seq_connect_from`'s code: the address is not one that exists.
    pub fn connect_from(&self, port: PortId, source: Address) -> Result<()> {
        // SAFETY: `self.seq` is open; the arguments are plain integers the
        // sequencer validates.
        let rc = unsafe {
            sys::snd_seq_connect_from(
                self.seq,
                i32::from(port.get()),
                i32::from(source.client().get()),
                i32::from(source.port().get()),
            )
        };
        check(rc, Operation::ConnectFrom).map(drop)
    }

    /// Subscribes the port at `dest` to what `port` sends.
    ///
    /// # Errors
    ///
    /// `snd_seq_connect_to`'s code: the address is not one that exists.
    pub fn connect_to(&self, port: PortId, dest: Address) -> Result<()> {
        // SAFETY: `self.seq` is open; the arguments are plain integers the
        // sequencer validates.
        let rc = unsafe {
            sys::snd_seq_connect_to(
                self.seq,
                i32::from(port.get()),
                i32::from(dest.client().get()),
                i32::from(dest.port().get()),
            )
        };
        check(rc, Operation::ConnectTo).map(drop)
    }

    /// Subscribes `port` to the system announce port, which sends an event
    /// whenever a client or port starts, exits or changes.
    ///
    /// # Errors
    ///
    /// `snd_seq_connect_from`'s code.
    pub fn connect_from_announce(&self, port: PortId) -> Result<()> {
        self.connect_from(port, Address::ANNOUNCE)
    }

    /// Reads one event, skipping a read that succeeds with none. The client
    /// answers [`Read::Empty`] when none is queued.
    ///
    /// # Errors
    ///
    /// `snd_seq_ump_event_input`'s code for anything but an empty queue, an
    /// overrun among them.
    pub fn read_event(&self) -> Result<Read> {
        #[cfg(feature = "testing")]
        if let Some(ReadHook(hook)) = self.read_hook.borrow_mut().as_mut() {
            hook().map_err(|source| Error::Call {
                operation: Operation::UmpEventInput,
                source,
            })?;
        }
        loop {
            let mut ev: *mut sys::snd_seq_ump_event_t = std::ptr::null_mut();
            // SAFETY: `self.seq` is open; `ev` is a valid out-pointer. On
            // success alsa-lib points it at a buffer it owns, valid until the
            // next call on this client. `ev` is copied by value at once below,
            // before any such call, and a `Client` is `!Sync`, so no other
            // thread calls into it meanwhile.
            let rc = unsafe { sys::snd_seq_ump_event_input(self.seq, &mut ev) };
            if rc == EAGAIN {
                return Ok(Read::Empty);
            }
            check(rc, Operation::UmpEventInput)?;
            // A success with no event is read again.
            // SAFETY: `rc >= 0` means alsa-lib wrote a complete event at `ev`,
            // which `as_ref` checks for NULL; it is copied out by value before
            // any other call.
            if let Some(ev) = unsafe { ev.as_ref() } {
                return Ok(Read::Event(Event::from_event(ev)));
            }
        }
    }

    /// Sends one UMP message to everything subscribed to `port` (see
    /// [`connect_to`](Self::connect_to)), immediately, bypassing any queue.
    ///
    /// # Errors
    ///
    /// `snd_seq_ump_event_output_direct`'s code; `-EAGAIN` when the
    /// destination has no room.
    pub fn send_to_subscribers(&self, port: PortId, ump: &Ump) -> Result<()> {
        self.send_ump_event(port, ump, None)
    }

    /// Sends one UMP message to everything subscribed to `port`, held by
    /// `queue` until the queue's real time reaches `at` (a time since the
    /// queue started: [`QueueClock::now`]); an `at` already past is delivered
    /// at once. The queue must be this client's own ([`start_queue`](Self::start_queue)):
    /// another client's is refused by the sequencer.
    ///
    /// # Errors
    ///
    /// `snd_seq_ump_event_output_direct`'s code; `-EAGAIN` when the
    /// destination has no room.
    pub fn send_to_subscribers_at(
        &self,
        port: PortId,
        ump: &Ump,
        queue: &QueueClock,
        at: Duration,
    ) -> Result<()> {
        self.send_ump_event(port, ump, Some((queue.queue, at)))
    }

    /// The one send: direct, or scheduled on a queue of this client's.
    fn send_ump_event(&self, port: PortId, ump: &Ump, on: Option<(u8, Duration)>) -> Result<()> {
        let operation = Operation::UmpEventOutputDirect;
        let mut ev = sys::snd_seq_ump_event_t {
            // `SND_SEQ_EVENT_NONE`. For a UMP event the type field is unused:
            // the message type lives in the first word's top nibble.
            type_: 0,
            source: sys::snd_seq_addr_t {
                client: self.id.get(),
                port: port.get(),
            },
            // **Not** the destination's own address, even though it is known.
            //
            // The port is already subscribed (`connect_to`), and an explicit
            // `dest` on top of a subscription is rejected outright:
            // `snd_seq_ump_event_output_direct` returns an error, not a partial
            // send. `SUBSCRIBERS` means "everyone connected to my port", which
            // is exactly the set `connect_to` established.
            dest: sys::snd_seq_addr_t {
                client: sys::SND_SEQ_ADDRESS_SUBSCRIBERS,
                port: 0,
            },
            // Dispatch now rather than through a queue. Zero here is queue 0, a
            // real queue this client never created, which is the other half of
            // that same `-EINVAL`.
            queue: sys::SND_SEQ_QUEUE_DIRECT,
            ..Default::default()
        };
        if let Some((queue, at)) = on {
            ev.queue = queue;
            // Absolute (`SND_SEQ_TIME_MODE_ABS` is the clear bit), real time.
            ev.flags = sys::SND_SEQ_TIME_STAMP_REAL;
            // A queue's real time counts seconds in a `c_uint`: saturate at
            // its end rather than wrap to the past.
            let secs = u32::try_from(at.as_secs()).unwrap_or(u32::MAX);
            ev.time[..4].copy_from_slice(&secs.to_ne_bytes());
            ev.time[4..].copy_from_slice(&at.subsec_nanos().to_ne_bytes());
        }
        // The message's own words, the rest of the event's fixed field zero.
        let words = ump.words();
        ev.ump[..words.len()].copy_from_slice(words);

        // SAFETY: `self.seq` is open; `ev` is a fully initialised event of the
        // measured layout (the compile-time asserts in `sys`), which alsa-lib
        // reads and does not retain. `output_direct` hands it to the kernel
        // without buffering, and the kernel holds it until `ev.time` on the
        // queue named in `ev.queue`.
        let rc = unsafe { sys::snd_seq_ump_event_output_direct(self.seq, &mut ev) };
        check(rc, operation).map(drop)
    }

    /// How many more events this client can hold in its own output pool: what
    /// a scheduled send needs a cell of, until the queue delivers it. A send
    /// that finds the pool full is refused.
    ///
    /// # Errors
    ///
    /// `snd_seq_get_client_pool`'s code.
    pub fn output_free(&self) -> Result<usize> {
        let pool = PoolRecord::new()?;
        // SAFETY: `self.seq` is open and `pool` is a live record.
        let rc = unsafe { sys::snd_seq_get_client_pool(self.seq, pool.as_ptr()) };
        check(rc, Operation::GetClientPool)?;
        // SAFETY: `pool` was just filled and is live.
        Ok(unsafe { sys::snd_seq_client_pool_get_output_free(pool.as_ptr()) })
    }

    /// Allocates and starts a queue of this client's own: a real-time clock
    /// that counts from zero now, read with [`QueueClock::now`] and used to
    /// schedule sends ([`send_to_subscribers_at`](Self::send_to_subscribers_at)).
    /// It closes with this client.
    ///
    /// # Errors
    ///
    /// The code of the call that failed.
    pub fn start_queue(&self) -> Result<QueueClock> {
        // SAFETY: `self.seq` is open.
        let rc = unsafe { sys::snd_seq_alloc_queue(self.seq) };
        let queue = address_byte("queue", check(rc, Operation::AllocQueue)?)?;
        // SAFETY: `self.seq` is open; a NULL event is the call's documented
        // "send directly".
        let rc = unsafe {
            sys::snd_seq_control_queue(
                self.seq,
                i32::from(queue),
                sys::SND_SEQ_EVENT_START,
                0,
                std::ptr::null_mut(),
            )
        };
        check(rc, Operation::ControlQueue)?;
        // The control event is buffered, not sent, until the buffer is drained.
        // SAFETY: `self.seq` is open.
        let rc = unsafe { sys::snd_seq_drain_output(self.seq) };
        check(rc, Operation::DrainOutput)?;
        Ok(QueueClock {
            queue,
            status: StatusRecord::new()?,
        })
    }

    /// Starts a queue of this client's own and makes `port` stamp each event it
    /// receives with that queue's real time, at the moment the kernel delivers
    /// it. The returned clock reads that queue, and the queue closes with this
    /// client.
    ///
    /// # Errors
    ///
    /// The code of the call that failed.
    pub fn stamp_port(&self, port: PortId) -> Result<QueueClock> {
        let clock = self.start_queue()?;
        let queue = clock.queue;
        self.edit_port(port, |info| {
            // SAFETY: `info` is a live record, filled from this client's port.
            unsafe {
                sys::snd_seq_port_info_set_timestamping(info, 1);
                sys::snd_seq_port_info_set_timestamp_real(info, 1);
                sys::snd_seq_port_info_set_timestamp_queue(info, i32::from(queue));
            }
        })?;
        Ok(clock)
    }

    /// Reads one of this client's ports into a record, lets `edit` change it,
    /// and writes it back; the sequencer announces the change.
    fn edit_port(
        &self,
        port: PortId,
        edit: impl FnOnce(*mut sys::snd_seq_port_info_t),
    ) -> Result<()> {
        let info = PortRecord::new()?;
        // SAFETY: `self.seq` is open and `info` is a live record.
        let rc =
            unsafe { sys::snd_seq_get_port_info(self.seq, i32::from(port.get()), info.as_ptr()) };
        check(rc, Operation::GetPortInfo)?;
        edit(info.as_ptr());
        // SAFETY: `self.seq` is open and `info` is a live record.
        let rc =
            unsafe { sys::snd_seq_set_port_info(self.seq, i32::from(port.get()), info.as_ptr()) };
        check(rc, Operation::SetPortInfo).map(drop)
    }

    /// The sequencer handle, for the crate's other modules.
    pub(crate) fn raw(&self) -> *mut sys::snd_seq_t {
        self.seq
    }

    /// Opens a **legacy** client (no UMP version: the kernel treats it as a
    /// MIDI 1.0 byte-event application), the kind of source a MIDI 1.0 device
    /// or an old application is. Its events are sent with
    /// [`send_legacy_to_subscribers`](Self::send_legacy_to_subscribers); reading
    /// UMP from it is an error.
    ///
    /// # Errors
    ///
    /// The step that failed, with alsa-lib's code.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub fn open_legacy(name: &str) -> Result<Self> {
        Self::open_plain(name)
    }

    /// Sends one legacy channel event to everything subscribed to `port`.
    ///
    /// # Errors
    ///
    /// `snd_seq_event_output_direct`'s code.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub fn send_legacy_to_subscribers(&self, port: PortId, event: LegacyEvent) -> Result<()> {
        let operation = Operation::EventOutputDirect;
        let mut ev = sys::snd_seq_event_t {
            type_: 0,
            flags: 0,
            tag: 0,
            queue: sys::SND_SEQ_QUEUE_DIRECT,
            time: [0; 8],
            source: sys::snd_seq_addr_t {
                client: self.id.get(),
                port: port.get(),
            },
            dest: sys::snd_seq_addr_t {
                client: sys::SND_SEQ_ADDRESS_SUBSCRIBERS,
                port: 0,
            },
            data: [0; 12],
        };
        match event {
            LegacyEvent::NoteOn {
                channel,
                note,
                velocity,
            } => {
                ev.type_ = sys::SND_SEQ_EVENT_NOTEON;
                ev.data[..3].copy_from_slice(&[channel, note, velocity]);
            }
            LegacyEvent::Controller {
                channel,
                param,
                value,
            } => {
                ev.type_ = sys::SND_SEQ_EVENT_CONTROLLER;
                ev.data[0] = channel;
                ev.data[4..8].copy_from_slice(&param.to_ne_bytes());
                ev.data[8..12].copy_from_slice(&value.to_ne_bytes());
            }
        }
        // SAFETY: `self.seq` is open; `ev` is a fully initialised event of the
        // measured layout (the compile-time asserts in `sys`), which alsa-lib
        // reads and does not retain.
        let rc = unsafe { sys::snd_seq_event_output_direct(self.seq, &mut ev) };
        check(rc, operation).map(drop)
    }

    /// Renames this client; the sequencer announces the change.
    ///
    /// # Errors
    ///
    /// [`Error::NulInName`] when `name` has a NUL.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub fn rename(&self, name: &str) -> Result<()> {
        let cname = c_name(name)?;
        // SAFETY: `self.seq` is open; `cname` outlives the call.
        unsafe { sys::snd_seq_set_client_name(self.seq, cname.as_ptr()) };
        Ok(())
    }

    /// Sets the capability bits of one of this client's ports; the sequencer
    /// announces the change.
    ///
    /// # Errors
    ///
    /// The code of the call that failed.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub fn set_port_caps(&self, port: PortId, caps: PortCaps) -> Result<()> {
        self.edit_port(port, |info| {
            // SAFETY: `info` is a live record, filled from this client's port.
            unsafe { sys::snd_seq_port_info_set_capability(info, caps.bits()) };
        })
    }
}

/// A legacy (byte-oriented) channel event a [`Client::open_legacy`] client sends.
#[cfg(feature = "testing")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyEvent {
    /// A Note On.
    NoteOn {
        /// Channel, 0 to 15.
        channel: u8,
        /// Note number.
        note: u8,
        /// Velocity.
        velocity: u8,
    },
    /// A control change.
    Controller {
        /// Channel, 0 to 15.
        channel: u8,
        /// Controller number.
        param: u32,
        /// Value.
        value: i32,
    },
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: opened by `open`, closed exactly once: `seq` is private and
        // never reassigned.
        unsafe { sys::snd_seq_close(self.seq) };
    }
}

/// A queue's real-time clock, read through a status record kept for the
/// purpose. Made by [`Client::stamp_port`].
#[derive(Debug)]
pub struct QueueClock {
    queue: u8,
    status: StatusRecord,
}

impl QueueClock {
    /// The queue's real time now, as the time since it started. `client` is the
    /// one that made the queue: another client has no such queue, and the call
    /// fails.
    ///
    /// # Errors
    ///
    /// `snd_seq_get_queue_status`'s code, or [`Error::NoQueueTime`] when the
    /// sequencer will not say.
    pub fn now(&mut self, client: &Client) -> Result<Duration> {
        // SAFETY: `client.seq` is open and `status` is a live record.
        let rc = unsafe {
            sys::snd_seq_get_queue_status(client.seq, i32::from(self.queue), self.status.as_ptr())
        };
        check(rc, Operation::GetQueueStatus)?;
        // SAFETY: `status` was just filled; the pointer is into it and read at
        // once, before the record is used again.
        let time =
            unsafe { sys::snd_seq_queue_status_get_real_time(self.status.as_ptr()).as_ref() }
                .ok_or(Error::NoQueueTime)?;
        Ok(Duration::new(u64::from(time.tv_sec), time.tv_nsec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name with a NUL is refused, not renamed.
    #[test]
    fn a_name_with_a_nul_is_refused() {
        assert!(matches!(c_name("a\0b"), Err(Error::NulInName { .. })));
        assert_eq!(c_name("ok").unwrap().to_str(), Ok("ok"));
    }
}
