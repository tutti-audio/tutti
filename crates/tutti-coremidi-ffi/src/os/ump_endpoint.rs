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
//! main-thread only. [`UmpEndpoint::new`] takes a [`MainThreadMarker`] and the
//! endpoint is `!Send` and `!Sync`, so creating, enabling, disabling and
//! releasing it all happen on the main thread. What may cross threads is the
//! [`UmpSender`]: the source's `u32` reference and the callback's counters.

use super::error::{Error, Operation};
use super::objc_exception::catch;
use super::receive::{Counts, Receiver};
use super::source::{protocol_id, publish};
use crate::blocks::{PlatformBlock, platform_blocks};
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tutti_platform::MainThreadMarker;
use tutti_ump::{DeviceIdentity, FunctionBlockId, FunctionBlockInfo, Protocol, U7, Ump};

/// The longest Product Instance Id CoreMIDI accepts, in characters
/// (`MIDIUMPMutableEndpoint`'s header: a longer one is an error).
const MAX_INSTANCE_ID: usize = 42;

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
/// ```compile_fail
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
/// rest is atomics. A send after the endpoint is dropped is
/// [`Error::EndpointGone`]; one that races the drop reaches CoreMIDI with the
/// reference of a disposed endpoint, which looks it up, finds nothing and returns
/// status 0, so such a send is accepted and delivered nowhere (never undefined).
#[derive(Clone, Debug)]
pub struct UmpSender {
    source: MIDIEndpointRef,
    protocol: MIDIProtocolID,
    live: Arc<AtomicBool>,
    counts: Counts,
}

impl core::fmt::Debug for Counts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Counts")
            .field("faults", &self.faults())
            .field("truncated", &self.truncated())
            .finish()
    }
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
        if !self.live.load(Ordering::Acquire) {
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

impl UmpEndpoint {
    /// Creates the endpoint `name` carrying `protocol`, identified as `device`
    /// (the Device Identity, M2-104 §7.1.3) with Product Instance Id `instance`
    /// (§7.1.5, empty for none), registers `blocks` as its static Function Blocks
    /// and enables it. `on_ump` gets each message sent to it, on CoreMIDI's
    /// delivery thread. `main` proves the caller is on the main thread.
    ///
    /// CoreMIDI numbers the blocks itself, in registration order. `blocks` are
    /// given in number order with their numbers running from 0 without a gap, and
    /// [`blocks`](Self::blocks) reads back the numbers CoreMIDI gave them: those
    /// are the numbers other apps read.
    ///
    /// # Errors
    ///
    /// Before any call into CoreMIDI: [`Error::ReservedProtocol`] for a reserved
    /// `protocol`, [`Error::Block`] for a block CoreMIDI cannot carry (checked
    /// before the class lookup, so it is the same on every macOS), and
    /// [`Error::InstanceIdTooLong`]. Then [`Error::UmpEndpointUnsupported`] before
    /// macOS 15, [`Error::Exception`] when CoreMIDI raises an Objective-C
    /// exception, [`Error::Objc`] or [`Error::NoObject`] when CoreMIDI refuses
    /// the endpoint, and [`Error::BlockNumber`] for a number the spec cannot hold.
    /// An `Err` leaves no endpoint published.
    pub fn new<F>(
        main: MainThreadMarker,
        name: &str,
        protocol: Protocol,
        device: &DeviceIdentity,
        instance: &str,
        blocks: &[(FunctionBlockInfo, Option<&str>)],
        on_ump: F,
    ) -> Result<Self, Error>
    where
        F: FnMut(Option<Duration>, Ump) + Send + 'static,
    {
        let protocol = protocol_id(protocol)?;
        let blocks = platform_blocks(blocks)?;
        if instance.len() > MAX_INSTANCE_ID {
            return Err(Error::InstanceIdTooLong {
                len: instance.len(),
            });
        }
        // `ClassType::class` panics on a missing class; the lookup by name is the
        // availability check.
        if AnyClass::get(c"MIDIUMPMutableEndpoint").is_none()
            || AnyClass::get(c"MIDIUMPMutableFunctionBlock").is_none()
        {
            return Err(Error::UmpEndpointUnsupported);
        }
        let receiver = Receiver::ump(on_ump);
        let mut made = None;
        // Nothing autoreleased escapes the pool: the endpoint is owned (+1, kept in
        // `made`), and what leaves is a `u32` and bytes.
        let built = autoreleasepool(|_| {
            catch(|| {
                create(
                    name, protocol, device, instance, &blocks, &receiver, &mut made,
                )
            })
        });
        let (source, numbers) = match built.map_err(Error::from).and_then(|inner| inner) {
            Ok(built) => built,
            Err(error) => {
                // The endpoint may be enabled: take it down before it is released.
                if let Some(endpoint) = made {
                    disable_and_release(endpoint);
                }
                return Err(error);
            }
        };
        let live = Arc::new(AtomicBool::new(true));
        Ok(Self {
            endpoint: made,
            sender: UmpSender {
                source,
                protocol,
                live,
                counts: receiver.counts(),
            },
            blocks: numbers,
            _main: main,
            _receiver: receiver,
        })
    }

    /// A handle that sends on the endpoint's source from any thread.
    pub fn sender(&self) -> UmpSender {
        self.sender.clone()
    }

    /// The numbers CoreMIDI gave the blocks, in the order they were registered.
    #[must_use]
    pub fn blocks(&self) -> &[FunctionBlockId] {
        &self.blocks
    }
}

/// The endpoint, registered and enabled, its source and its blocks' numbers. The
/// endpoint is put in `made` as soon as it exists, so a caller that gets an error
/// (or an exception, which unwinds past this frame's destructors) can still take
/// it down.
fn create(
    name: &str,
    protocol: MIDIProtocolID,
    device: &DeviceIdentity,
    instance: &str,
    blocks: &[PlatformBlock],
    receiver: &Receiver,
    made: &mut Option<Retained<MIDIUMPMutableEndpoint>>,
) -> Result<(MIDIEndpointRef, Vec<FunctionBlockId>), Error> {
    let function_blocks = blocks
        .iter()
        .map(function_block)
        .collect::<Result<Vec<_>, _>>()?;
    let function_blocks = NSArray::from_retained_slice(&function_blocks);
    // SAFETY: every initialiser is sent to a freshly allocated object with valid
    // arguments, the strings and `info` live across their calls, and the callback
    // block is the receiver's, which CoreMIDI copies before `init` returns. The
    // block outlives the endpoint: the receiver outlives it (`Drop`), and
    // `Receiver::new` also leaks one reference to the block.
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
            &NSString::from_str(instance),
            Protocol_(protocol),
            receiver.block().cast::<DynBlock<_>>(),
        )
    }
    .ok_or(Error::NoObject(Operation::CreateUmpEndpoint))?;
    let endpoint = made.insert(endpoint);
    // SAFETY: plain messages to the live endpoint with an array of live blocks.
    unsafe { endpoint.registerFunctionBlocks_markAsStatic_error(&function_blocks, true) }
        .map_err(|e| objc(Operation::RegisterFunctionBlocks, &e))?;
    // SAFETY: a plain message to the live endpoint.
    unsafe { endpoint.setEnabled_error(true) }
        .map_err(|e| objc(Operation::EnableUmpEndpoint, &e))?;
    // SAFETY: plain getters on the live endpoint and its blocks.
    let (source, numbers) = unsafe {
        (
            endpoint.MIDISource(),
            endpoint
                .functionBlocks()
                .iter()
                .map(|block| block.functionBlockID())
                .collect::<Vec<_>>(),
        )
    };
    let numbers = numbers
        .into_iter()
        .map(|number| FunctionBlockId::new(number).ok_or(Error::BlockNumber(number)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((source, numbers))
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
    Error::Objc {
        operation,
        code: error.code(),
        domain: error.domain().to_string(),
        description: error.localizedDescription().to_string(),
    }
}

/// Disables `endpoint` and releases it, each inside [`catch`] since releasing runs
/// the object's dealloc. A refusal or an exception here has nowhere to go: the
/// object is released either way, which removes the endpoint.
fn disable_and_release(endpoint: Retained<MIDIUMPMutableEndpoint>) {
    let _ = autoreleasepool(|_| {
        catch(|| {
            // SAFETY: a plain message to the live endpoint, which this owns.
            unsafe { endpoint.setEnabled_error(false) }.ok();
            drop(endpoint);
        })
    });
}

impl Drop for UmpEndpoint {
    fn drop(&mut self) {
        // Sends that start after this are refused by name.
        self.sender.live.store(false, Ordering::Release);
        if let Some(endpoint) = self.endpoint.take() {
            disable_and_release(endpoint);
        }
        // `receiver` drops after this body, with its callback emptied.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{
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
}
