//! A virtual UMP Endpoint that declares Function Blocks (macOS 15's
//! `MIDIUMPMutableEndpoint`).
//!
//! `MIDISourceCreateWithProtocol` and `MIDIDestinationCreateWithProtocol` make an
//! endpoint with no blocks, so another app sees a bare port. A
//! `MIDIUMPMutableEndpoint` is one UMP Endpoint with a source (what this process
//! sends from), a destination (what it receives at) and the blocks it registers,
//! which CoreMIDI then reports to other apps. The class is Objective-C, so this
//! module reaches it through `objc2-core-midi`; every send goes through
//! [`catch`], so an Objective-C exception comes back as [`Error::Exception`].
//!
//! # Main thread
//!
//! Apple documents every interaction with a `MIDIUMPMutableEndpoint` as
//! main-thread only. A [`CreateUmpEndpoint`] takes a [`MainThreadMarker`] (and the
//! process's [`SharedClient`], see its docs) at each poll, and the creation and the
//! endpoint are `!Send` and `!Sync`, so creating, enabling, disabling and
//! releasing it all happen on the main thread. What may cross threads is the
//! [`UmpSender`]: the source's `u32` reference and the callback's counters.

use super::client::SharedClient;
use super::error::{Error, Operation};
use super::objc_exception::catch;
use super::receive::{Counts, Receiver};
use super::source::{protocol_id, publish};
use crate::checks::backoff::Failure;
use crate::checks::blocks::{PlatformBlock, numbers_in_registered_order, platform_blocks};
use crate::checks::endpoint_phases::{Phases, RegisterFailure, Stage, TakeDown, settle_register};
use block2::DynBlock;
use coremidi_sys::{MIDIEndpointRef, MIDIProtocolID};
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyClass;
use objc2_core_midi::{
    MIDI2DeviceInfo, MIDI2DeviceManufacturer, MIDI2DeviceRevisionLevel,
    MIDIProtocolID as Protocol_, MIDIUMPFunctionBlockDirection, MIDIUMPFunctionBlockMIDI1Info,
    MIDIUMPFunctionBlockUIHint, MIDIUMPMutableEndpoint, MIDIUMPMutableFunctionBlock,
};
use objc2_foundation::{NSArray, NSError, NSString};
use std::marker::PhantomData;
use std::sync::{Arc, PoisonError, RwLock};
use std::task::Poll;
use std::time::Duration;
use tutti_platform::{Instant, MainThreadMarker};
use tutti_ump::{
    DeviceIdentity, FunctionBlockId, FunctionBlockInfo, ProductInstanceId, Protocol, U7, Ump,
};

/// A 14-bit value as the two 7-bit bytes the wire holds it in, LSB first (M2-104
/// §7.1.3): `MIDIUInteger14`.
fn uinteger14(bytes: [U7; 2]) -> u16 {
    u16::from(bytes[0].get()) | (u16::from(bytes[1].get()) << 7)
}

/// A CoreMIDI virtual UMP Endpoint that declares its Function Blocks.
///
/// Other apps see one UMP Endpoint with the blocks it was created with, a source
/// they read from and a destination they write to. Its [`sender`](Self::sender)
/// publishes on the source, and `on_ump` receives what is written to the
/// destination, as [`UmpDestination`](crate::UmpDestination)'s callback does (the
/// same receive block, panic handling and drop order). Dropping it disables the
/// endpoint, which removes it from other apps, and then releases it.
///
/// It stays on the main thread that made it:
///
/// ```compile_fail,E0277
/// fn send<T: Send>() {}
/// send::<tutti_coremidi_ffi::UmpEndpoint>();
/// ```
#[must_use = "dropping it removes the endpoint"]
pub struct UmpEndpoint {
    // `None` only while `Drop` releases it.
    endpoint: Option<Retained<MIDIUMPMutableEndpoint>>,
    sender: UmpSender,
    blocks: Vec<FunctionBlockId>,
    // Makes the type `!Send` and `!Sync` whatever its fields are.
    _main: MainThreadMarker,
    // Dropped after the endpoint is disabled and released (see `Drop`).
    _receiver: Receiver,
}

impl core::fmt::Debug for UmpEndpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UmpEndpoint")
            .field("source", &self.sender.source)
            .field("blocks", &self.blocks)
            .finish_non_exhaustive()
    }
}

/// What may leave the main thread of an [`UmpEndpoint`]: a send on its source and
/// its callback's counters.
///
/// ```
/// fn crosses_threads<T: Clone + Send + Sync>() {}
/// crosses_threads::<tutti_coremidi_ffi::UmpSender>();
/// ```
///
/// `Clone`, `Send` and `Sync` without an `unsafe impl`: the source is an opaque
/// `u32` handle into CoreMIDI, whose objects are internally synchronised, and the
/// rest is a lock and atomics. A send holds a read lock on whether the endpoint is
/// live for the whole call, and dropping the endpoint takes the write lock before
/// it disposes anything, so no send is in flight when the endpoint goes and none
/// that follows reaches CoreMIDI: it is [`Error::EndpointGone`]. A send blocks for
/// that drop only while it runs, and a sender is not used on the audio thread.
#[derive(Clone, Debug)]
pub struct UmpSender {
    source: MIDIEndpointRef,
    protocol: MIDIProtocolID,
    live: Arc<RwLock<bool>>,
    counts: Counts,
}

impl UmpSender {
    /// Publishes one UMP message to the endpoint's subscribers, delivered now, in
    /// the protocol the endpoint was created with.
    ///
    /// # Errors
    ///
    /// [`Error::EndpointGone`] once the endpoint is dropped, and [`Error::Call`]
    /// when `MIDIReceivedEventList` returns a non-zero status.
    pub fn send(&self, ump: &Ump) -> Result<(), Error> {
        let live = self.live.read().unwrap_or_else(PoisonError::into_inner);
        if !*live {
            return Err(Error::EndpointGone);
        }
        publish(self.source, self.protocol, ump)
    }

    /// How many panics in the endpoint's callback have been caught: 0, or 1 once
    /// the callback panicked and was dropped.
    #[must_use]
    pub fn faults(&self) -> u64 {
        self.counts.faults()
    }

    /// How many packets ended inside a message, whose tail was dropped.
    #[must_use]
    pub fn truncated(&self) -> u64 {
        self.counts.truncated()
    }
}

/// A virtual UMP Endpoint being made: a value polled until the endpoint is
/// registered, enabled and read back.
///
/// CoreMIDI has been seen to refuse the registration of the blocks and the enable
/// with `kMIDINotPermitted` while other processes create endpoints, and to accept
/// the same call a few milliseconds later. A poll runs the attempt of the current
/// phase only when one is due (waits of 10 ms doubling to 200 ms, a bound of two
/// seconds from the phase's first attempt), and [`deadline`](Self::deadline) is the
/// instant the next poll can make progress. It waits for nothing itself.
///
/// It stays on the main thread that made it:
///
/// ```compile_fail,E0277
/// fn send<T: Send>() {}
/// send::<tutti_coremidi_ffi::CreateUmpEndpoint>();
/// ```
///
/// Dropping it before it finished takes the endpoint down: it is disabled and
/// released, so no endpoint is left published.
#[must_use = "a creation that is not polled makes no endpoint"]
pub struct CreateUmpEndpoint {
    // Declared first, so it is taken down before the receiver its callback reads
    // goes (fields drop in order).
    endpoint: TakeDown<Retained<MIDIUMPMutableEndpoint>>,
    function_blocks: Retained<NSArray<MIDIUMPMutableFunctionBlock>>,
    blocks: Vec<PlatformBlock>,
    protocol: MIDIProtocolID,
    // `None` once the finished endpoint owns it.
    receiver: Option<Receiver>,
    phases: Phases,
    // What the read-back found, taken when the creation finishes.
    built: Option<(MIDIEndpointRef, Vec<FunctionBlockId>)>,
    // Makes the type `!Send` and `!Sync` whatever its fields are.
    _main: MainThreadMarker,
    _thread: PhantomData<*const ()>,
}

impl core::fmt::Debug for CreateUmpEndpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CreateUmpEndpoint")
            .field("stage", &self.phases.stage())
            .finish_non_exhaustive()
    }
}

impl CreateUmpEndpoint {
    /// Starts creating the endpoint `name` on `client`, carrying `protocol`,
    /// identified as `device` (the Device Identity, M2-104 §7.1.3) with Product
    /// Instance Id `instance` (§7.1.5, `None` for none), to register `blocks` as its
    /// static Function Blocks and enable. `on_ump` gets each message sent to it, on
    /// CoreMIDI's delivery thread. `main` proves the caller is on the main thread.
    /// The first registration is due at `now`.
    ///
    /// This does the part that is not retried: the checks on the arguments, the
    /// platform's blocks and the allocation of the endpoint. Nothing is published
    /// until the polls finish.
    ///
    /// `client` proves that the process's [`StartClient`](crate::StartClient)
    /// finished, so the process has connected to the MIDI server. The endpoint API
    /// takes no client, and with no connection and the server gone, CoreMIDI raises
    /// an exception that the crate's exception catch cannot catch: the process
    /// aborts. A process whose start failed has no way to call this. The proof does
    /// not cover a server that dies after the start.
    ///
    /// CoreMIDI numbers the blocks itself, in registration order. `blocks` are
    /// given in number order with their numbers running from 0 without a gap, and
    /// [`UmpEndpoint::blocks`] reads back the numbers CoreMIDI gave them: those are
    /// the numbers other apps read.
    ///
    /// # Errors
    ///
    /// [`Error::ReservedProtocol`] for a reserved `protocol`, [`Error::Block`] for a
    /// block CoreMIDI cannot carry (checked before the class lookup, so it is the
    /// same on every macOS), [`Error::UmpEndpointUnsupported`] before macOS 15,
    /// [`Error::Exception`] when CoreMIDI raises an Objective-C exception, and
    /// [`Error::NoObject`] when it refuses to make the endpoint.
    #[allow(
        clippy::too_many_arguments,
        reason = "every one is a field of the endpoint's creation"
    )]
    pub fn new<F>(
        main: MainThreadMarker,
        _client: SharedClient,
        name: &str,
        protocol: Protocol,
        device: &DeviceIdentity,
        instance: Option<&ProductInstanceId>,
        blocks: &[(FunctionBlockInfo, Option<&str>)],
        on_ump: F,
        now: Instant,
    ) -> Result<Self, Error>
    where
        F: FnMut(Option<Duration>, Ump) + Send + 'static,
    {
        let protocol = protocol_id(protocol)?;
        let blocks = platform_blocks(blocks)?;
        // `ClassType::class` panics on a missing class; the lookup by name is the
        // availability check.
        if AnyClass::get(c"MIDIUMPMutableEndpoint").is_none()
            || AnyClass::get(c"MIDIUMPMutableFunctionBlock").is_none()
        {
            return Err(Error::UmpEndpointUnsupported);
        }
        let receiver = Receiver::ump(on_ump);
        // Nothing autoreleased escapes the pool: the endpoint and the array are
        // owned (+1).
        let (endpoint, function_blocks) = autoreleasepool(|_| {
            catch(|| allocate(name, protocol, device, instance, &blocks, &receiver))
        })
        .map_err(Error::from)
        .and_then(|inner| inner)?;
        Ok(Self {
            endpoint: TakeDown::new(endpoint, disable_and_release),
            function_blocks,
            blocks,
            protocol,
            receiver: Some(receiver),
            phases: Phases::new(now),
            built: None,
            _main: main,
            _thread: PhantomData,
        })
    }

    /// When the next poll can make progress: the next attempt's instant.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.phases.deadline()
    }

    /// One poll at `now`: runs the attempt of the current phase when one is due.
    /// A phase that succeeds starts the next with a fresh bound; the last is the
    /// read-back of the blocks.
    ///
    /// `Pending` means try again at [`deadline`](Self::deadline). A poll after the
    /// creation finished, or failed, is [`Error::EndpointGone`].
    ///
    /// # Errors
    ///
    /// [`Error::Exception`] when CoreMIDI raises an Objective-C exception,
    /// [`Error::RetryTimedOut`] when a refusal with `kMIDINotPermitted` (the
    /// registration and the enable) lasted until the phase's bound,
    /// [`Error::Objc`] or [`Error::NoObject`] when it refuses a call otherwise,
    /// [`Error::BlockNumber`] for a number the spec cannot hold, and
    /// [`Error::BlockReadBack`] when CoreMIDI lists blocks other than the
    /// registered ones. An `Err` leaves no endpoint published.
    ///
    /// On macOS 15.8.2 the registration has been seen to report failure without an
    /// error after registering the blocks; that is accepted when the endpoint holds
    /// as many blocks as were registered.
    pub fn poll(
        &mut self,
        main: MainThreadMarker,
        now: Instant,
    ) -> Poll<Result<UmpEndpoint, Error>> {
        let Self {
            endpoint,
            function_blocks,
            blocks,
            phases,
            built,
            ..
        } = self;
        let Some(live) = endpoint.get() else {
            return Poll::Ready(Err(Error::EndpointGone));
        };
        let count = blocks.len();
        // Nothing autoreleased escapes the pool: what leaves is a `u32` and bytes.
        let polled = autoreleasepool(|_| {
            catch(|| {
                phases.poll(
                    now,
                    |stage| match stage {
                        Stage::Register => register(live, function_blocks, count),
                        Stage::Enable => enable(live),
                        Stage::ReadBack => read_back(live, blocks).map(|done| *built = Some(done)),
                    },
                    Error::os_status,
                )
            })
        });
        let failure = match polled {
            Err(exception) => Error::from(exception),
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(Ok(()))) => return Poll::Ready(Ok(self.finish(main))),
            Ok(Poll::Ready(Err((_, Failure::Failed(error))))) => error,
            Ok(Poll::Ready(Err((stage, Failure::TimedOut { last })))) => Error::RetryTimedOut {
                operation: match stage {
                    Stage::Register => Operation::RegisterFunctionBlocks,
                    Stage::Enable | Stage::ReadBack => Operation::EnableUmpEndpoint,
                },
                last,
            },
        };
        // The endpoint may be enabled: take it down before it is released.
        self.endpoint.dispose();
        Poll::Ready(Err(failure))
    }

    /// The finished endpoint, once the read-back has succeeded.
    fn finish(&mut self, main: MainThreadMarker) -> UmpEndpoint {
        let (source, numbers) = self.built.take().expect("the read-back ran");
        let receiver = self.receiver.take().expect("the creation has not finished");
        UmpEndpoint {
            endpoint: self.endpoint.release(),
            sender: UmpSender {
                source,
                protocol: self.protocol,
                live: Arc::new(RwLock::new(true)),
                counts: receiver.counts(),
            },
            blocks: numbers,
            _main: main,
            _receiver: receiver,
        }
    }
}

impl UmpEndpoint {
    /// How many panics in the endpoint's callback have been caught: 0, or 1 once
    /// the callback panicked and was dropped.
    #[must_use]
    pub fn faults(&self) -> u64 {
        self.sender.faults()
    }

    /// A handle that sends on the endpoint's source from any thread.
    pub fn sender(&self) -> UmpSender {
        self.sender.clone()
    }

    /// The numbers CoreMIDI gave the blocks, indexed by the number each was given:
    /// `blocks()[n]` is the number other apps read for the block created as number
    /// `n` (the given numbers run 0, 1, 2 without a gap).
    #[must_use]
    pub fn blocks(&self) -> &[FunctionBlockId] {
        &self.blocks
    }
}

/// The endpoint, allocated and initialised and not yet registered, with its blocks
/// as the array to register.
fn allocate(
    name: &str,
    protocol: MIDIProtocolID,
    device: &DeviceIdentity,
    instance: Option<&ProductInstanceId>,
    blocks: &[PlatformBlock],
    receiver: &Receiver,
) -> Result<
    (
        Retained<MIDIUMPMutableEndpoint>,
        Retained<NSArray<MIDIUMPMutableFunctionBlock>>,
    ),
    Error,
> {
    let function_blocks = blocks
        .iter()
        .map(function_block)
        .collect::<Result<Vec<_>, _>>()?;
    let function_blocks = NSArray::from_retained_slice(&function_blocks);
    // SAFETY: every initialiser is sent to a freshly allocated object with valid
    // arguments, the strings and `info` live across their calls, and the callback
    // block is the receiver's, which CoreMIDI copies before `init` returns. The
    // block outlives the endpoint: the receiver outlives it (`CreateUmpEndpoint`
    // and `UmpEndpoint` take the endpoint down first), and `Receiver::new` also
    // leaks one reference to the block.
    let endpoint = unsafe {
        let info = MIDI2DeviceInfo::initWithManufacturerID_family_modelNumber_revisionLevel(
            MIDI2DeviceInfo::alloc(),
            MIDI2DeviceManufacturer {
                sysExIDByte: device.manufacturer.sysex_bytes().map(U7::get),
            },
            uinteger14(device.family),
            uinteger14(device.model),
            MIDI2DeviceRevisionLevel {
                revisionLevel: device.revision.get().map(U7::get),
            },
        );
        MIDIUMPMutableEndpoint::initWithName_deviceInfo_productInstanceID_MIDIProtocol_destinationCallback(
            MIDIUMPMutableEndpoint::alloc(),
            &NSString::from_str(name),
            &info,
            &NSString::from_str(instance.map_or("", ProductInstanceId::as_str)),
            Protocol_(protocol),
            receiver.block().cast::<DynBlock<_>>(),
        )
    }
    .ok_or(Error::NoObject(Operation::CreateUmpEndpoint))?;
    Ok((endpoint, function_blocks))
}

/// The source of the enabled `endpoint` and the numbers CoreMIDI gave its blocks,
/// in the order of `blocks`. Called inside [`catch`].
fn read_back(
    endpoint: &MIDIUMPMutableEndpoint,
    blocks: &[PlatformBlock],
) -> Result<(MIDIEndpointRef, Vec<FunctionBlockId>), Error> {
    // SAFETY: plain getters on the live endpoint and its blocks.
    let (source, listed) = unsafe {
        (
            endpoint.MIDISource(),
            endpoint
                .functionBlocks()
                .iter()
                .map(|block| (block.name().to_string(), block.functionBlockID()))
                .collect::<Vec<_>>(),
        )
    };
    let numbers = numbers_in_registered_order(blocks, &listed).ok_or(Error::BlockReadBack {
        registered: blocks.len(),
        listed: listed.len(),
    })?;
    let numbers = numbers
        .into_iter()
        .map(|number| FunctionBlockId::new(number).ok_or(Error::BlockNumber(number)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((source, numbers))
}

/// The failure of `registerFunctionBlocks:markAsStatic:error:` for an `NSError`
/// with these fields: `Unreported` for the one objc2 makes when the method returns
/// `NO` and leaves its error out, a refusal otherwise. The refusal keeps its error,
/// [`Error::os_status`] is the status the retry rule filters on.
fn classify(
    operation: Operation,
    code: isize,
    domain: String,
    description: String,
) -> RegisterFailure<Error> {
    if is_missing_error(code, &domain) {
        RegisterFailure::Unreported
    } else {
        RegisterFailure::Refused(Error::from_ns_error(operation, code, domain, description))
    }
}

/// Whether the error is the one objc2 makes when a method returns `NO` and leaves
/// its error out. objc2 exposes no other signal for it: the object is a plain
/// `NSError` with code 0 and the domain `__objc2.missingError`, so this is the one
/// place that names the domain. The match is pinned to objc2's
/// `__macro_helpers/null_error.rs` (0.6.x) and is re-checked when objc2 is bumped;
/// if it stops matching, a register without an error fails closed as
/// `Objc { code: 0 }`.
fn is_missing_error(code: isize, domain: &str) -> bool {
    code == 0 && domain == "__objc2.missingError"
}

/// One attempt to register `function_blocks` (`count` of them) as static. A `NO`
/// without an `NSError` is accepted when the endpoint holds `count` blocks, and is
/// [`Error::NoObject`] otherwise. A refusal keeps its `NSError`, and
/// [`Error::os_status`] says whether it is retried. Called inside [`catch`].
fn register(
    endpoint: &MIDIUMPMutableEndpoint,
    function_blocks: &NSArray<MIDIUMPMutableFunctionBlock>,
    count: usize,
) -> Result<(), Error> {
    let operation = Operation::RegisterFunctionBlocks;
    // SAFETY: plain messages to the live endpoint with an array of live blocks.
    let result =
        unsafe { endpoint.registerFunctionBlocks_markAsStatic_error(function_blocks, true) }
            .map_err(|e| {
                classify(
                    operation,
                    e.code(),
                    e.domain().to_string(),
                    e.localizedDescription().to_string(),
                )
            });
    settle_register(
        result,
        count,
        // SAFETY: a plain getter on the live endpoint.
        || unsafe { endpoint.mutableFunctionBlocks() }.count(),
        Error::NoObject(operation),
    )
}

/// One attempt to enable `endpoint`. An attempt fails with the whole [`Error`], so
/// a refusal keeps its `NSError` domain and description; [`Error::os_status`] is
/// the status the retry rule filters on. Called inside [`catch`].
fn enable(endpoint: &MIDIUMPMutableEndpoint) -> Result<(), Error> {
    // SAFETY: a plain message to the live endpoint.
    unsafe { endpoint.setEnabled_error(true) }.map_err(|e| objc(Operation::EnableUmpEndpoint, &e))
}

/// The Objective-C Function Block for `block`.
fn function_block(block: &PlatformBlock) -> Result<Retained<MIDIUMPMutableFunctionBlock>, Error> {
    // SAFETY: the initialiser is sent to a freshly allocated object with valid
    // arguments, and the name outlives the call.
    unsafe {
        MIDIUMPMutableFunctionBlock::initWithName_direction_firstGroup_totalGroupsSpanned_maxSysEx8Streams_MIDI1Info_UIHint_isEnabled(
            MIDIUMPMutableFunctionBlock::alloc(),
            &NSString::from_str(&block.name),
            MIDIUMPFunctionBlockDirection(block.direction),
            block.first_group,
            block.groups_spanned,
            block.max_sysex8_streams,
            MIDIUMPFunctionBlockMIDI1Info(block.midi1),
            MIDIUMPFunctionBlockUIHint(block.ui_hint),
            block.enabled,
        )
    }
    .ok_or(Error::NoObject(Operation::CreateFunctionBlock))
}

/// `operation` failed with `error`. Called inside [`catch`].
fn objc(operation: Operation, error: &NSError) -> Error {
    Error::from_ns_error(
        operation,
        error.code(),
        error.domain().to_string(),
        error.localizedDescription().to_string(),
    )
}

/// Disables `endpoint` and releases it, each in its own [`catch`] (releasing runs the
/// object's dealloc), so a disable that raises still releases. A refusal or an
/// exception has nowhere to go: the object is released either way, which removes
/// the endpoint.
fn disable_and_release(endpoint: Retained<MIDIUMPMutableEndpoint>) {
    let _ = autoreleasepool(|_| {
        catch(|| {
            // SAFETY: a plain message to the live endpoint, which this owns.
            unsafe { endpoint.setEnabled_error(false) }.ok();
        })
    });
    let _ = autoreleasepool(|_| catch(|| drop(endpoint)));
}

impl Drop for UmpEndpoint {
    fn drop(&mut self) {
        // Waits for a send in flight, then sends are refused by name: nothing
        // reaches CoreMIDI once this releases the lock.
        *self
            .sender
            .live
            .write()
            .unwrap_or_else(PoisonError::into_inner) = false;
        if let Some(endpoint) = self.endpoint.take() {
            disable_and_release(endpoint);
        }
        // `receiver` drops after this body, with its callback emptied.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::blocks::{
        DIRECTION_BIDIRECTIONAL, DIRECTION_INPUT, DIRECTION_OUTPUT, MIDI1_NOT_A_PORT,
        MIDI1_RESTRICTED, MIDI1_UNRESTRICTED, UI_HINT_BOTH, UI_HINT_RECEIVER, UI_HINT_SENDER,
        UI_HINT_UNKNOWN,
    };

    /// The numbers the spec's fields map to are the values of CoreMIDI's own
    /// enums, so a block created from them is the block the spec describes.
    #[test]
    fn the_mapped_numbers_are_the_platform_enum_values() {
        assert_eq!(MIDIUMPFunctionBlockDirection::Input.0, DIRECTION_INPUT);
        assert_eq!(MIDIUMPFunctionBlockDirection::Output.0, DIRECTION_OUTPUT);
        assert_eq!(
            MIDIUMPFunctionBlockDirection::Bidirectional.0,
            DIRECTION_BIDIRECTIONAL
        );
        assert_eq!(MIDIUMPFunctionBlockMIDI1Info::NotMIDI1.0, MIDI1_NOT_A_PORT);
        assert_eq!(
            MIDIUMPFunctionBlockMIDI1Info::UnrestrictedBandwidth.0,
            MIDI1_UNRESTRICTED
        );
        assert_eq!(
            MIDIUMPFunctionBlockMIDI1Info::RestrictedBandwidth.0,
            MIDI1_RESTRICTED
        );
        assert_eq!(MIDIUMPFunctionBlockUIHint::Unknown.0, UI_HINT_UNKNOWN);
        assert_eq!(MIDIUMPFunctionBlockUIHint::Receiver.0, UI_HINT_RECEIVER);
        assert_eq!(MIDIUMPFunctionBlockUIHint::Sender.0, UI_HINT_SENDER);
        assert_eq!(MIDIUMPFunctionBlockUIHint::SenderReceiver.0, UI_HINT_BOTH);
    }

    const OP: Operation = Operation::RegisterFunctionBlocks;

    fn classified(code: isize, domain: &str) -> RegisterFailure<Error> {
        classify(OP, code, domain.into(), "d".into())
    }

    #[test]
    fn objc2s_missing_error_is_an_unreported_failure() {
        assert_eq!(
            classified(0, "__objc2.missingError"),
            RegisterFailure::Unreported
        );
    }

    #[test]
    fn a_real_ns_error_is_refused_even_with_the_missing_error_domain_or_code() {
        assert_eq!(
            classified(-10844, "NSOSStatusErrorDomain"),
            RegisterFailure::Refused(Error::NotPermitted(OP))
        );
        assert!(matches!(
            classified(-10843, "NSOSStatusErrorDomain"),
            RegisterFailure::Refused(Error::Objc { code: -10843, .. })
        ));
        assert!(matches!(
            classified(0, "NSOSStatusErrorDomain"),
            RegisterFailure::Refused(Error::Objc { code: 0, .. })
        ));
        assert!(matches!(
            classified(-10843, "__objc2.missingError"),
            RegisterFailure::Refused(_)
        ));
    }
}
