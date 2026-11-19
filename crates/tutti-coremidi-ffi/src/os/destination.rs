//! A **native-UMP** virtual MIDI destination, the inbound counterpart to
//! [`UmpSource`](crate::UmpSource).
//!
//! The safe `coremidi` wrapper has `Client::virtual_destination_with_protocol`,
//! but its receive block reads the connection's reference constant as a
//! `&mut ()`. A virtual destination has no connection, so CoreMIDI passes a
//! null one, and the wrapper makes a null reference out of it on the first
//! packet ("null reference produced" in a debug build, undefined behaviour in
//! a release one). [`UmpDestination`] creates the endpoint with
//! `MIDIDestinationCreateWithProtocol` itself, with a block that never reads
//! the reference constant, and hands the caller each UMP message of each packet
//! with its time.

use block2::RcBlock;
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{
    MIDIDestinationCreateWithProtocol, MIDIEndpointDispose, MIDIEndpointRef, MIDIEventList,
    MIDIEventPacket, MIDIEventPacketNext, MIDIReceiveBlock, MIDITimeStamp,
};
use std::ffi::c_void;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::client::SharedClient;
use super::error::{Error, Operation, OsStatus};
use super::host_clock::Timebase;
use super::source::protocol_id;
use tutti_ump::{Protocol, Ump, split_stream};

/// The receive block CoreMIDI calls with each event list: the list, and the
/// connection's reference constant, which this block never reads. The list is
/// typed `*const c_void` here, since a block's arguments must have an
/// Objective-C encoding and `coremidi-sys` declares none for `MIDIEventList`;
/// a pointer is a pointer at the ABI, and the block casts it back.
type ReceiveBlock = RcBlock<dyn Fn(*const c_void, *mut c_void)>;

type OnUmp = Box<dyn FnMut(Option<Duration>, Ump) + Send>;

/// What the receive block and its [`UmpDestination`] share: the callback, which
/// the destination takes on drop or the block takes after a panic, and the
/// count of panics caught and of packets that ended in a truncated message.
struct Callback {
    on_ump: Mutex<Option<OnUmp>>,
    faults: AtomicU64,
    truncated: AtomicU64,
}

/// A CoreMIDI virtual destination that speaks the **MIDI 2.0 (UMP) protocol**:
/// other apps see a UMP-capable destination, and every UMP message sent to it is
/// handed to the callback given at creation, on CoreMIDI's delivery thread, with
/// its time. A packet carries several messages back to back, each one's length
/// in its message type; the callback gets one call per message, in order, with
/// its packet's time. The time is the Mach host clock's reading (the one
/// [`host_ticks`](crate::host_ticks) reads) as the duration since that clock's
/// origin, or `None` when the sender gave no time (a host time of 0: delivery
/// now). A packet that ends inside a message drops that tail and counts it
/// ([`truncated`](Self::truncated)). Owns its endpoint, which is disposed on
/// drop.
///
/// # A panic in the callback
///
/// The receive block's invoke function is `extern "C-unwind"`, so a panic
/// leaving it would unwind through CoreMIDI's frames. The callback runs under
/// `catch_unwind` instead: a panic drops the callback, so every later packet
/// delivers nothing, and [`faults`](Self::faults) counts it. The crate logs
/// nothing, but a caught panic still runs the process's panic hook on CoreMIDI's
/// thread (the default one prints to stderr, which allocates and locks): an app
/// that cares installs a quiet hook, which a library never does.
///
/// # Drop
///
/// `MIDIServices.h` does not say whether a call already running on CoreMIDI's
/// thread has returned when `MIDIEndpointDispose` does, nor whether one can
/// start after it. The drop therefore disposes the endpoint, then takes the
/// callback out of the state it shares with the block under the callback's lock:
/// it waits for a call in progress, and afterwards the callback is neither
/// running nor callable, and is dropped on the dropping thread. CoreMIDI's own
/// copy of the block may outlive the drop and only finds the state empty.
///
/// Dropping a destination from inside its own callback deadlocks, since the
/// callback holds that lock.
pub struct UmpDestination {
    endpoint: MIDIEndpointRef,
    callback: Arc<Callback>,
    // Our reference to the block; CoreMIDI holds its own copy.
    _block: ReceiveBlock,
}

// SAFETY: the endpoint is an opaque `u32` handle into CoreMIDI, whose objects are
// internally synchronised, and `MIDIEndpointDispose` may run on any thread. The
// block is never called from Rust: it is only retained by CoreMIDI and released
// by the drop, both atomic on a block's reference count. `Callback` is `Send +
// Sync`: its closure is `Send` (`new` requires it) and sits behind a mutex. The
// one assumption beyond the header is that CoreMIDI retains the block for the
// duration of a call; `MIDIServices.h` does not document it.
unsafe impl Send for UmpDestination {}

impl fmt::Debug for UmpDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UmpDestination")
            .field("endpoint", &self.endpoint)
            .field("faults", &self.faults())
            .finish_non_exhaustive()
    }
}

impl UmpDestination {
    /// Publishes a virtual destination named `name` carrying `protocol` on
    /// `client`; `on_ump` gets each message sent to it.
    ///
    /// # Errors
    ///
    /// [`Error::ReservedProtocol`] for a reserved `protocol`, and
    /// [`Error::Call`] when the endpoint cannot be created.
    pub fn new<F>(
        client: SharedClient,
        name: &str,
        protocol: Protocol,
        on_ump: F,
    ) -> Result<Self, Error>
    where
        F: FnMut(Option<Duration>, Ump) + Send + 'static,
    {
        let protocol = protocol_id(protocol)?;
        let client = client.get();
        let callback = Arc::new(Callback {
            on_ump: Mutex::new(Some(Box::new(on_ump))),
            faults: AtomicU64::new(0),
            truncated: AtomicU64::new(0),
        });
        let shared = Arc::clone(&callback);
        let block: ReceiveBlock = RcBlock::new(move |list: *const c_void, _: *mut c_void| {
            // SAFETY: CoreMIDI passes a valid event list for the length of the
            // call (MIDIServices.h, `MIDIReceiveBlock`), or null, which
            // `packets` reads as empty.
            unsafe { receive(&shared, list.cast::<MIDIEventList>()) };
        });
        let name = CFString::new(name);
        let mut endpoint: MIDIEndpointRef = 0;
        // SAFETY: `client` is a live client, `name` outlives the call, `endpoint`
        // is a valid out-pointer, and the block is a valid Objective-C block
        // CoreMIDI copies (and so retains) before the call returns; the block
        // never reads its reference-constant argument.
        let status = unsafe {
            MIDIDestinationCreateWithProtocol(
                client,
                name.as_concrete_TypeRef(),
                protocol,
                &mut endpoint,
                RcBlock::as_ptr(&block).cast::<c_void>() as MIDIReceiveBlock,
            )
        };
        OsStatus::check(status)
            .map_err(|status| Error::call(Operation::CreateDestination, status))?;
        Ok(Self {
            endpoint,
            callback,
            _block: block,
        })
    }

    /// How many panics in the callback have been caught: 0, or 1 once the
    /// callback panicked and was dropped.
    #[must_use]
    pub fn faults(&self) -> u64 {
        self.callback.faults.load(Ordering::Relaxed)
    }

    /// How many packets ended inside a message, whose tail was dropped.
    #[must_use]
    pub fn truncated(&self) -> u64 {
        self.callback.truncated.load(Ordering::Relaxed)
    }
}

impl Drop for UmpDestination {
    fn drop(&mut self) {
        // SAFETY: the endpoint is ours, created above and disposed once.
        unsafe {
            MIDIEndpointDispose(self.endpoint);
        }
        // The lock is never poisoned: `receive` catches every panic under it.
        let taken = self
            .callback
            .on_ump
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        drop(taken);
    }
}

/// Hands one delivered event list to the callback, if it is still there: a panic
/// in the callback is caught, drops it, and counts a fault.
///
/// # Safety
///
/// `list` satisfies [`packets`]'s contract.
unsafe fn receive(callback: &Callback, list: *const MIDIEventList) {
    let Ok(mut slot) = callback.on_ump.lock() else {
        return;
    };
    let Some(on_ump) = slot.as_mut() else {
        return;
    };
    let base = Timebase::get();
    // SAFETY: the caller's contract.
    let ran = catch_unwind(AssertUnwindSafe(|| unsafe {
        packets(list, &mut |stamp, words| {
            if !split_packet(base, stamp, words, on_ump) {
                callback.truncated.fetch_add(1, Ordering::Relaxed);
            }
        })
    }));
    if let Err(payload) = ran {
        callback.faults.fetch_add(1, Ordering::Relaxed);
        let dead = slot.take();
        // Neither the payload's drop nor the callback's may panic out.
        drop_guarded(payload);
        drop_guarded(dead);
    }
}

/// Drops `value`; a panic in its `Drop` is caught, and its payload is leaked
/// rather than dropped, since that drop could panic again.
fn drop_guarded<T>(value: T) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        std::mem::forget(payload);
    }
}

/// Hands `on_ump` each message of a packet's `words`, in order, at `stamp`'s time
/// by `base`. `false` when `words` ends inside a message: the tail is dropped.
fn split_packet(
    base: Timebase,
    stamp: MIDITimeStamp,
    words: &[u32],
    on_ump: &mut dyn FnMut(Option<Duration>, Ump),
) -> bool {
    let time = base.stamp(stamp);
    let mut used = 0;
    for ump in split_stream(words) {
        used += ump.words().len();
        on_ump(time, ump);
    }
    used == words.len()
}

/// Hands each packet of `list` to `on_packet` as its timestamp and words.
///
/// # Safety
///
/// `list` is null or points to a valid `MIDIEventList` whose `numPackets`
/// packets each hold `wordCount` words, as CoreMIDI delivers it.
unsafe fn packets(list: *const MIDIEventList, on_packet: &mut dyn FnMut(MIDITimeStamp, &[u32])) {
    if list.is_null() {
        return;
    }
    // SAFETY: `list` is valid (the caller's contract); the list is packed to
    // four bytes, so its fields are read unaligned.
    let count = unsafe { std::ptr::addr_of!((*list).numPackets).read_unaligned() };
    // SAFETY: the first packet is inside the list.
    let mut packet: *const MIDIEventPacket = unsafe { std::ptr::addr_of!((*list).packet).cast() };
    for _ in 0..count {
        // SAFETY: `packet` is one of the list's `count` packets (the first, or
        // `MIDIEventPacketNext` of the one before); its fields are read unaligned
        // for the four-byte packing, and `wordCount` is clamped to the 64 words
        // of the packet's `words` array, so the slice stays inside it.
        let (stamp, words) = unsafe {
            let stamp = std::ptr::addr_of!((*packet).timeStamp).read_unaligned();
            let len = (std::ptr::addr_of!((*packet).wordCount).read_unaligned() as usize).min(64);
            let words = std::ptr::addr_of!((*packet).words).cast::<u32>();
            (stamp, std::slice::from_raw_parts(words, len))
        };
        on_packet(stamp, words);
        // SAFETY: `packet` is a valid packet of the list (above).
        packet = unsafe { MIDIEventPacketNext(packet) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coremidi_sys::{ByteCount, MIDIEventListAdd, MIDIEventListInit};
    use std::mem::MaybeUninit;
    use std::sync::atomic::AtomicUsize;

    /// A panic payload whose `Drop` panics.
    struct PanicsOnDrop;

    impl Drop for PanicsOnDrop {
        fn drop(&mut self) {
            panic!("the payload's drop panics");
        }
    }

    /// Apple Silicon's host clock base: 3 ticks are 125 ns.
    fn apple_base() -> Timebase {
        Timebase::new(125, 3).unwrap()
    }

    /// Runs `f` on an event list holding one packet of `words`.
    fn with_list(words: &[u32], f: impl FnOnce(&MIDIEventList)) {
        let mut list = MaybeUninit::<MIDIEventList>::uninit();
        // SAFETY: `Init` initialises `list`, which `Add` then fills inside its
        // real size; `words` outlives the call, and `assume_init_ref` follows both.
        let list = unsafe {
            let packet = MIDIEventListInit(
                list.as_mut_ptr(),
                protocol_id(Protocol::Midi2).expect("MIDI 2.0 has an id"),
            );
            let packet = MIDIEventListAdd(
                list.as_mut_ptr(),
                size_of::<MIDIEventList>() as ByteCount,
                packet,
                0,
                words.len() as ByteCount,
                words.as_ptr(),
            );
            assert!(!packet.is_null());
            list.assume_init_ref()
        };
        f(list);
    }

    fn callback_of(on_ump: OnUmp) -> Callback {
        Callback {
            on_ump: Mutex::new(Some(on_ump)),
            faults: AtomicU64::new(0),
            truncated: AtomicU64::new(0),
        }
    }

    /// Delivers one packet to `callback` three times.
    fn deliver_thrice(callback: &Callback) {
        with_list(&[0x2090_3C64], |list| {
            for _ in 0..3 {
                // SAFETY: `list` is a valid event list.
                unsafe { receive(callback, list) };
            }
        });
    }

    /// A callback that panics on its first packet, with `payload`, is never
    /// called again, the panic is counted once, and nothing leaves `receive`.
    fn check_panicking_callback(payload: fn() -> ()) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let callback = callback_of(Box::new(move |_, _| {
            seen.fetch_add(1, Ordering::Relaxed);
            payload();
        }));
        deliver_thrice(&callback);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(callback.faults.load(Ordering::Relaxed), 1);
        assert!(callback.on_ump.lock().unwrap().is_none());
    }

    #[test]
    fn a_panicking_callback_is_dropped_after_one_counted_fault() {
        check_panicking_callback(|| panic!("the callback panics"));
    }

    #[test]
    fn a_payload_whose_drop_panics_does_not_leave_receive() {
        check_panicking_callback(|| std::panic::panic_any(PanicsOnDrop));
    }

    /// A 64-bit MIDI 2.0 Note On followed by a 32-bit MIDI 1.0 Channel Voice
    /// message in one packet are two messages (M2-104 Table 4: type 0x4 is two
    /// words, type 0x2 one), both at the packet's time: 24 000 000 ticks at
    /// 125/3 are exactly 1 s.
    #[test]
    fn a_packet_of_a_64_bit_and_a_32_bit_message_is_two_messages_at_its_time() {
        let words = [0x4090_3C00, 0x8000_0000, 0x2090_3C64];
        let mut got = Vec::new();
        let whole = split_packet(apple_base(), 24_000_000, &words, &mut |time, ump| {
            got.push((time, ump));
        });
        assert!(whole);
        assert_eq!(
            got,
            [
                (
                    Some(Duration::from_secs(1)),
                    Ump([0x4090_3C00, 0x8000_0000, 0, 0])
                ),
                (Some(Duration::from_secs(1)), Ump([0x2090_3C64, 0, 0, 0])),
            ]
        );
    }

    /// A packet that ends inside a 64-bit message drops that tail and reports it;
    /// the whole message before it is delivered.
    #[test]
    fn a_truncated_tail_is_dropped_and_reported() {
        let mut got = Vec::new();
        let whole = split_packet(
            apple_base(),
            0,
            &[0x2090_3C64, 0x4090_3C00],
            &mut |_, ump| {
                got.push(ump);
            },
        );
        assert!(!whole);
        assert_eq!(got, [Ump([0x2090_3C64, 0, 0, 0])]);
    }

    /// A stamp of 3 ticks at 125/3 is 125 ns, and no stamp is `None`.
    #[test]
    fn a_stamp_is_its_ticks_by_the_timebase() {
        let mut times = Vec::new();
        for stamp in [3, 0] {
            split_packet(apple_base(), stamp, &[0x2090_3C64], &mut |time, _| {
                times.push(time);
            });
        }
        assert_eq!(times, [Some(Duration::from_nanos(125)), None]);
    }

    /// A delivered list with a truncated message is counted once and still
    /// delivers the message before it.
    #[test]
    fn a_truncated_packet_in_a_list_is_counted() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let callback = callback_of(Box::new(move |_, _| {
            seen.fetch_add(1, Ordering::Relaxed);
        }));
        with_list(&[0x2090_3C64, 0x4090_3C00], |list| {
            // SAFETY: `list` is a valid event list.
            unsafe { receive(&callback, list) };
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(callback.truncated.load(Ordering::Relaxed), 1);
    }
}
