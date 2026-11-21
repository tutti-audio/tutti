//! MIDI *from* the AU: `kAudioUnitProperty_MIDIOutputCallback` (48) and its
//! discovery half, `kAudioUnitProperty_MIDIOutputCallbackInfo` (47).
//!
//! `send_midi` pushes MIDI *into* an instrument.
//! This module is the other direction: an arpeggiator, step sequencer, or chord
//! generator hosted as an AU emits notes during its render call, and with no
//! callback installed those notes have nowhere to go. The plugin still plays,
//! but the host cannot record the performance, route it to a second instrument,
//! or show it on a piano roll.

use std::mem::align_of;
use std::os::raw::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use coreaudio_sys::{AudioTimeStamp, MIDIPacket, MIDIPacketList, OSStatus};
use tutti_midi_protocol::Midi2Translator;
#[cfg(feature = "testing")]
use tutti_midi_protocol::midi1;
use tutti_plugin_types::{MidiEventVec, WireMidi, group_of_port};
use tutti_rt::{Tally, TallyWriter};
use tutti_ump::{Message, Midi1Parser, MidiGroup, Ump};

use crate::callback::with_state_mut;
use crate::error::{Error, Result};
use crate::property::{Element, MidiOutputCallbackInfo, MidiOutputCallbackProperty, Scope};
use crate::status::{CANNOT_DO_IN_CURRENT_CONTEXT, NO_ERR};
use crate::unit::Unit;

/// The answer to a call with no state or no packet list behind it.
const NOT_INSTALLED: OSStatus = -1;

/// The `AUMIDIOutputCallbackStruct` from `AudioUnitProperties.h`.
///
/// Hand-declared for the same reason `bus.rs::AuChannelInfo` is: `coreaudio-sys`
/// does not export it. The layout is pinned by
/// `tests::the_callback_struct_matches_the_c_abi` against Apple's declaration —
/// function pointer first, `userData` second, 16 bytes on a 64-bit target — so a
/// wrong guess fails a test rather than handing the AU a `userData` it reads as a
/// code pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AuMidiOutputCallbackStruct {
    midi_output_callback: Option<AuMidiOutputCallbackFn>,
    user_data: *mut c_void,
}

/// Apple's `AUMIDIOutputCallback` signature.
///
/// `pktlist` is a `MIDIPacketList` whose packet timestamps are **sample offsets**
/// from `timeStamp`, not host times — Apple's header is explicit about this, and
/// it is what makes the offsets directly usable as
/// [`WireMidi::offset`] without a clock conversion.
impl crate::plain::Sealed for AuMidiOutputCallbackStruct {}
// SAFETY: an `Option<unsafe extern "C" fn>` and a raw pointer in a
// `#[repr(C)]` struct: every bit pattern, zero (`None`, null) included, is a
// valid value.
unsafe impl crate::plain::Plain for AuMidiOutputCallbackStruct {}

type AuMidiOutputCallbackFn = unsafe extern "C" fn(
    user_data: *mut c_void,
    time_stamp: *const AudioTimeStamp,
    midi_out_num: u32,
    pktlist: *const MIDIPacketList,
) -> OSStatus;

/// What a host does with MIDI the AU emitted.
///
/// # The no-allocation contract
pub type MidiOutSink = Box<dyn FnMut(u32, &[WireMidi]) + Send>;

/// The state the AU's `userData` points at, and the thing whose lifetime the
/// removal ordering protects.
struct MidiOutState {
    sink: MidiOutSink,
    /// One reader per output stream the AU published, by the `midi_out_num` the
    /// AU calls with: a stream is a Group of its own number, and each keeps
    /// its own parameter-number and bank runs. Fixed at install, so the render
    /// thread never grows it.
    decoders: Box<[Decoder]>,
    /// Packets of the lists the AU delivered for a stream with no decoder (it
    /// published fewer streams than it calls with, or more than the sixteen
    /// Groups): dropped and counted here, read through
    /// [`AuMidiOutput::unrouted_packets`].
    unrouted: TallyWriter<u64>,
    /// Set by [`AuMidiOutput::reset`]: the next list finds every reader forgotten
    /// before it is read.
    reset: Arc<AtomicBool>,
}

/// A plugin's MIDI 1.0 byte stream as engine events.
///
/// A `MIDIPacketList` is a stream, not a list of messages: a packet may carry
/// several messages and real-time bytes inside another message, and a SysEx
/// may start in one packet and end in a later one. [`Midi1Parser`] reads it;
/// a packet's end ends a short message (`end_of_message`), and a SysEx goes on
/// as SysEx7 packets in the order the parser completes them, each at the
/// offset of the packet that held its `0xF0` (offset 0 for one opened in an
/// earlier list). A channel voice message goes through the stream's
/// [`Midi2Translator`] to MIDI 2.0 (M2-104 Appendix D.3), as every plugin's output
/// does: its RPN, NRPN and Bank Select runs are assembled across messages and
/// lists. A run (select, Data Entry MSB) with no LSB waits for one: the translator
/// never flushes a live output, which has no end (M2-104 Appendix D.3.3). Events are handed to the sink in batches of a [`MidiEventVec`];
/// nothing allocates (the parser holds one SysEx7 packet's payload inline, and
/// the batch and the translator are fixed-size).
#[derive(Debug)]
struct Decoder {
    group: MidiGroup,
    parser: Midi1Parser,
    translator: Midi2Translator,
    batch: MidiEventVec,
    /// The offset of the SysEx being read.
    sysex_at: u32,
}

impl Decoder {
    /// A reader of the stream that is `group`.
    fn new(group: MidiGroup) -> Self {
        Self {
            group,
            parser: Midi1Parser::new(group),
            translator: Midi2Translator::new(),
            batch: MidiEventVec::new(),
            sysex_at: 0,
        }
    }

    /// Forgets the stream: the open SysEx, the running status and every
    /// parameter-number and bank run. Nothing pending is sent.
    fn reset(&mut self) {
        self.parser = Midi1Parser::new(self.group);
        self.translator.reset();
        self.batch.clear();
        self.sysex_at = 0;
    }

    /// A new list: events staged and not flushed (a list cut short by a
    /// panic) belong to a block that is gone, and an open SysEx goes on at
    /// offset 0.
    fn begin_list(&mut self) {
        self.batch.clear();
        self.sysex_at = 0;
    }

    /// Reads one packet's `bytes`. Whenever the batch is full, `flush` gets
    /// it first.
    fn decode(&mut self, bytes: &[u8], frame_offset: u32, flush: &mut impl FnMut(&[WireMidi])) {
        for &byte in bytes {
            if byte == 0xF0 {
                self.sysex_at = frame_offset;
            }
            let Some(message) = self.parser.feed(byte) else {
                continue;
            };
            let offset = match message {
                Message::Sysex7(..) => self.sysex_at,
                _ => frame_offset,
            };
            if self.batch.is_full() {
                self.flush(flush);
            }
            // A message that ends no run (a select, a Data Entry MSB) is absorbed.
            let Some(ump) = self.translator.translate(Ump::from(message)) else {
                continue;
            };
            // Room: the batch is not full, or was just flushed.
            let _ = self.batch.try_push(WireMidi { offset, ump });
        }
        self.parser.end_of_message();
    }

    /// Hands on what is staged.
    fn flush(&mut self, flush: &mut impl FnMut(&[WireMidi])) {
        if !self.batch.is_empty() {
            flush(&self.batch);
            self.batch.clear();
        }
    }
}

/// A host's registration of a MIDI-output callback on one AU.
///
/// Holding this is what keeps the callback installed. A unit has at most one
/// registration at a time ([`install`] refuses a second). Dropping it writes the
/// null callback, so the AU stops calling, and **leaks** the state the AU's
/// `userData` pointed at: a render may be running on another thread, and nothing
/// in a `Drop` can know it is not. Freeing is [`withdraw`](Self::withdraw), which
/// frees it when no render is in flight.
#[must_use = "dropping it uninstalls the output and leaks its state; `withdraw` frees it"]
pub struct AuMidiOutput {
    unit: Unit,
    /// The state, owned through the raw pointer `Box::into_raw` made of it: the
    /// AU retains a `userData` that *is* this pointer, so no `Box` (whose moves
    /// assert uniqueness) or reference is ever formed from it while the AU may
    /// call. It is freed by `Box::from_raw` only in [`withdraw`](Self::withdraw),
    /// or before the AU ever held it; never by `Drop`.
    state: Option<std::ptr::NonNull<MidiOutState>>,
    /// [`MidiOutState::unrouted`], shared so the handle reads it without
    /// touching the state the render thread holds.
    unrouted: Tally<u64>,
    /// [`MidiOutState::reset`], shared for the same reason.
    reset: Arc<AtomicBool>,
}

impl AuMidiOutput {
    /// How many packets the AU delivered, in lists for an output stream this
    /// registration has no reader for, since it was installed: dropped, never
    /// routed to another stream's Group. A packet carries one or more messages,
    /// so this is the least the events lost came to.
    #[must_use]
    pub fn unrouted_packets(&self) -> u64 {
        self.unrouted.get()
    }

    /// Forgets what the output streams have read: a SysEx open across a
    /// discontinuity (a reset, a locate) must not complete on the far side of
    /// it, nor an RPN, NRPN or Bank Select run. Nothing pending is sent. Takes
    /// effect at the next list the AU delivers; call it between blocks, from
    /// the thread that renders.
    pub fn reset(&self) {
        self.reset.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for AuMidiOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuMidiOutput").finish_non_exhaustive()
    }
}

/// What the AU says about its MIDI output streams.
///
/// The stream count is `names.len()`. It is not stored separately: Apple defines
/// the array's length *as* the number of outputs, so a second field would be a
/// derivable value stored beside its source and could disagree with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiOutputInfo {
    /// One name per MIDI output stream, in AU-assigned order. The index into this
    /// vec is the `midi_out_num` the AU passes to the callback.
    ///
    /// An entry is `None` when the AU published a value at that index that is not
    /// a usable `CFString` — a `None` rather than an empty string, so a host can
    /// tell "the AU named this output the empty string" from "the AU returned
    /// something that was not a name". The slot is kept rather than skipped
    /// because dropping it would renumber every output after it, and those
    /// numbers are what the callback is keyed by.
    pub names: Vec<Option<String>>,
}

impl MidiOutputInfo {
    /// How many MIDI output streams the AU offers.
    pub fn stream_count(&self) -> usize {
        self.names.len()
    }
}

/// What the AU publishes about its MIDI output streams, or `None` if it publishes
/// nothing.
///
/// # `None` means "not a MIDI source", and that is the real capability gate
///
/// An AU that answers with an empty array is reported as
/// `Some(MidiOutputInfo { names: [] })`, distinct from `None`: it implements the
/// property and is telling you it currently has no output streams.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub fn midi_output_info(unit: &Unit) -> Option<MidiOutputInfo> {
    // The property's value is a `CFArrayRef` the AU *copies* for the caller — Apple's
    // header says outright "The host owns this array and its elements and should
    // release them". `copy_string_array` takes that +1 under the Create rule so
    // the release happens on drop, on every path out.
    //
    // GET rule for the elements, not Create: the array copy did not add a
    // retain to each element, so the elements belong to the array and are
    // released with it. Wrapping one in `CfString::from_copied` (Create) would
    // release a string the host never owned — the over-release that corrupts the
    // AU's own table and crashes on the *next* read, far from the cause.
    //
    // The checked converter, because the AU supplies these pointers: an array
    // that is not made of CFStrings hands back non-null values that are not
    // names, and the unchecked converter takes the process down with SIGBUS on
    // one. Element and output names are also short strings, i.e. arm64 *tagged
    // pointers* — legitimately misaligned — which is why the checked
    // converter's tag-bit allowance, not a plain alignment test, is what admits
    // them.
    let names = unit
        .copy_string_array::<MidiOutputCallbackInfo>(Scope::GLOBAL, Element::ZERO)
        .ok()?;
    Some(MidiOutputInfo { names })
}

/// Installs `sink` as this AU's MIDI-output destination.
///
/// One registration per unit: `kAudioUnitProperty_MIDIOutputCallback` is write
/// only (`AudioUnitProperties.h`), so a callback already installed cannot be read
/// back, and the unit carries a claim instead that its registration holds until
/// it is withdrawn or dropped. A second install on a unit that holds one is
/// refused, which keeps a dropped handle from nulling another's callback.
///
/// Not gated on [`midi_output_info`]: whether the unit publishes streams is the
/// caller's question, and 45 units accept the property while publishing none.
///
/// # Errors
///
/// [`Error::Invalid`] when the unit already has a registration (or an earlier
/// one's withdrawal was refused by the AU, which leaves the claim); the AU's own
/// status when it refuses the property (the claim is then released).
///
/// The registration holds a share of the unit, so the unit outlives it.
pub fn install(unit: &Unit, sink: MidiOutSink) -> Result<AuMidiOutput> {
    if !unit.claim_midi_output() {
        return Err(Error::Invalid(
            "the unit already has a MIDI-output registration, or the AU refused \
             the withdrawal of an earlier one and keeps the claim"
                .into(),
        ));
    }
    // Boxed once, here, on the control thread — the only allocation in the whole
    // path. The state stays at its address until the registration is withdrawn
    // (and is never freed by a drop), so the `userData` the AU retains stays
    // valid however the returned handle is moved.
    // One reader for each stream the AU publishes (one when it publishes
    // none, which every AU that accepts the callback does not), at most one
    // for each of the sixteen Groups.
    let streams = midi_output_info(unit).map_or(1, |info| info.stream_count().max(1));
    let decoders: Box<[Decoder]> = (0..streams)
        .map_while(|stream| group_of_port(u16::try_from(stream).ok()?))
        .map(Decoder::new)
        .collect();
    let unrouted = tutti_rt::tally::<u64>();
    let reset = Arc::new(AtomicBool::new(false));
    let state = Box::into_raw(Box::new(MidiOutState {
        sink,
        decoders,
        unrouted: unrouted.writer(),
        reset: Arc::clone(&reset),
    }));
    let user_data: *mut c_void = state.cast();

    let callback = AuMidiOutputCallbackStruct {
        midi_output_callback: Some(au_midi_output_callback),
        user_data,
    };
    // Build the handle BEFORE the property write, so that if the write succeeds
    // and anything after it fails, the handle's `Drop` is what withdraws the
    // callback. There is nothing after it today; the ordering is written this way
    // so adding something later cannot leave the AU holding a pointer no handle
    // owns.
    let mut handle = AuMidiOutput {
        unit: unit.clone(),
        // SAFETY: `Box::into_raw` never returns null.
        state: Some(unsafe { std::ptr::NonNull::new_unchecked(state) }),
        unrouted,
        reset,
    };
    if let Err(refused) =
        unit.set::<MidiOutputCallbackProperty>(Scope::GLOBAL, Element::ZERO, &callback)
    {
        // The AU refused the property, so it never held the pointer: the state
        // is the host's to free, and the unit's claim is released.
        handle.unit.release_midi_output();
        if let Some(state) = handle.state.take() {
            // SAFETY: made by `Box::into_raw` above and never shared with the AU.
            drop(unsafe { Box::from_raw(state.as_ptr()) });
        }
        return Err(refused);
    }
    Ok(handle)
}

impl AuMidiOutput {
    /// Withdraws the callback and frees the state the AU's `userData` pointed at,
    /// when nothing can still read it.
    ///
    /// What can read the state is a render calling the AU's callback. Two things
    /// must hold for that to be impossible after the free. The unit's lifecycle
    /// is checked first, before anything is written, and the lifecycle lock
    /// (`Unit::while_uninitialized`) is then held across the clearing write, the
    /// render check and the free:
    ///
    /// - **The unit is uninitialized**, and stays so until the free is done. An
    ///   AU may take its callbacks at `AudioUnitInitialize` and keep calling the
    ///   old pair after a later property write; an uninitialized unit renders
    ///   nothing, and an `initialize` that waits on the lock takes the cleared
    ///   callback. A unit whose `AudioUnitUninitialize` failed, or has not run,
    ///   counts as initialized.
    /// - **No render of this crate's is in flight** (`Unit::rendering`), read
    ///   after the clearing write and a `SeqCst` fence. A render that began
    ///   before the unit was uninitialized, or while a clone uninitialized it (it
    ///   must not: `Unit::uninitialize`), is counted; one the count does not
    ///   show starts after the check and reads the cleared callback. A render
    ///   made outside this crate's safe calls (`RenderUnit::get`'s `unsafe`
    ///   contract) is not counted: its caller owes that none overlaps a
    ///   withdrawal.
    ///
    /// A host calls it after `Unit::uninitialize` and before the next
    /// `Unit::initialize`. The lifecycle lock is control-side: no render takes
    /// it.
    ///
    /// # Errors
    /// The handle comes back with the error, still holding the unit's claim and
    /// the state, so the caller can call `withdraw` again; dropping it instead
    /// withdraws the callback and leaks the state.
    /// - `Error::Os` if the AU refuses the clearing write: the unit may still
    ///   call into the state.
    /// - `Error::Invalid` if the unit is initialized (nothing was written), or a
    ///   render is in flight (the callback is cleared, and the state stays
    ///   allocated because that render may still call it).
    ///
    /// An AU that refuses the clearing write of a handle that is then dropped
    /// keeps the unit's claim for good, so the unit cannot register again.
    #[must_use = "a refused withdrawal hands the registration back, with its state"]
    pub fn withdraw(mut self) -> std::result::Result<(), (Self, Error)> {
        let unit = self.unit.clone();
        // The lifecycle is checked before anything is written: a refusal on an
        // initialized unit changes nothing.
        let freed = unit.while_uninitialized(|| {
            self.clear_callback()?;
            if !unit.no_render_in_flight() {
                return Err(Self::left_allocated("a render of the unit is in flight"));
            }
            if let Some(state) = self.state.take() {
                // SAFETY: made by `Box::into_raw` in `install`. The withdrawing
                // write succeeded, the unit is uninitialized and `initialize`
                // waits on the lock this runs under, and no render was in flight
                // after the write (the fence in `no_render_in_flight` orders the
                // two), so the AU holds no pointer it will use and no render
                // reads the state; nothing else frees it.
                drop(unsafe { Box::from_raw(state.as_ptr()) });
            }
            Ok(())
        });
        match freed {
            Some(Ok(())) => {
                self.unit.release_midi_output();
                Ok(())
            }
            Some(Err(refused)) => Err((self, refused)),
            None => Err((
                self,
                Error::Invalid(
                    "the unit is initialized: nothing is withdrawn; uninitialize the \
                     unit, then withdraw"
                        .into(),
                ),
            )),
        }
    }

    fn left_allocated(why: &str) -> Error {
        Error::Invalid(format!(
            "{why}: the callback is withdrawn and its state is left allocated"
        ))
    }

    /// Writes the callback away, so the AU calls no more, and releases the unit's
    /// claim on success. The state is not touched.
    fn unhook(&mut self) -> Result<()> {
        // Nothing installed (already withdrawn): nothing to write.
        if self.state.is_none() {
            return Ok(());
        }
        self.clear_callback()?;
        self.unit.release_midi_output();
        Ok(())
    }

    /// Writes the withdrawn callback, keeping the claim.
    ///
    /// # How the withdrawal is spelled
    ///
    /// Not a null proc, which every unit refuses with -4 (measured on macOS
    /// 15.6): a real proc with a null `userData`, so the callback written is
    /// `au_midi_output_callback` with nothing to point at, which answers
    /// `NOT_INSTALLED`.
    fn clear_callback(&mut self) -> Result<()> {
        let withdrawn = AuMidiOutputCallbackStruct {
            midi_output_callback: Some(au_midi_output_callback),
            user_data: std::ptr::null_mut(),
        };
        self.unit
            .set::<MidiOutputCallbackProperty>(Scope::GLOBAL, Element::ZERO, &withdrawn)
    }
}

// SAFETY: the pointer owns a `MidiOutState` (a `MidiOutSink`, which is `Send`,
// decoders and atomics, all `Send`) that only the AU's render thread and
// `withdraw` reach, never at once (see `au_midi_output_callback`); moving the
// handle to another thread moves that ownership.
unsafe impl Send for AuMidiOutput {}

impl Drop for AuMidiOutput {
    /// Writes the null callback, so the AU calls no more, and leaks the state.
    ///
    /// A `Drop` cannot know a render is not running on another thread (this type
    /// is `Send` and public), and the callback reads the state during one: freeing
    /// here could be a use-after-free inside `AudioUnitRender`. The state is a
    /// few kilobytes per registration; a host that wants it back calls
    /// [`withdraw`](Self::withdraw), which frees it when no render is in flight.
    ///
    /// The status is ignored because a `Drop` has nowhere to report one.
    fn drop(&mut self) {
        let _ = self.unhook();
        self.state = None;
    }
}

/// The AU calls this on its **render thread** to hand the host MIDI it generated.
///
/// `extern "C"`, so a panic must never escape: unwinding across the FFI boundary
/// into AudioToolbox is undefined behaviour. The whole body runs under
/// [`with_state_mut`], which contains a panic and answers an error status, as
/// `render.rs`'s `au_input_render_callback` does and for the same reason.
/// A host sink is arbitrary user code; a `[]` index in it must degrade to a
/// dropped block, not to undefined behaviour.
///
/// # Safety
/// Called by AudioToolbox with the `userData` registered through
/// `kAudioUnitProperty_MIDIOutputCallback`. `user_data` must be null or point at
/// a live `MidiOutState` box, which the withdrawing property write outlives —
/// that write is what makes the pointer unreachable before the box is freed.
///
/// The body forms a `&mut MidiOutState` from it, so no other reference to the
/// state may exist for the duration of the call. That holds because the AU
/// invokes this only from inside its own render, one call at a time (an
/// assumption about AudioToolbox: Apple's header says the callback is called
/// from the render, and documents no concurrent calls), and the host side
/// touches the state only before installing and in [`AuMidiOutput::withdraw`],
/// which frees it only after the withdrawing write and with no render in flight.
///
/// That last step rests on a second assumption about AudioToolbox: once
/// `AudioUnitSetProperty(MIDIOutputCallback)` returns, a render that starts
/// after it reads the new `{proc, userData}`, even on an initialized unit. A
/// render already in flight may still read the old pair; it is counted
/// (`Unit::rendering`), and the withdrawal leaves the state allocated while it
/// is. Neither is documented by Apple; both hold for every unit measured.
///
/// `pktlist` must be null or a well-formed `MIDIPacketList` valid for the
/// duration of the call.
unsafe extern "C" fn au_midi_output_callback(
    user_data: *mut c_void,
    _time_stamp: *const AudioTimeStamp,
    midi_out_num: u32,
    pktlist: *const MIDIPacketList,
) -> OSStatus {
    // SAFETY: `user_data` is the raw pointer `install` made of the `MidiOutState`
    // box, which the withdrawing property write outlives. This `&mut MidiOutState` is the
    // only reference to it because of one invariant: the AU calls this only from
    // inside its own render (a render-time callback, never from another thread
    // or between renders), one call at a time, and the host touches the box only
    // before installing and after withdrawing. A render that starts after the
    // withdrawing `AudioUnitSetProperty` returns reads the cleared pair (the
    // second AudioToolbox assumption, in this function's `# Safety`); one that
    // began before is counted, and the box is left allocated for it.
    // The one `unsafe` block also covers the `dispatch` call in the closure:
    // `state` is the only reference (the callback's invariant above) and
    // `pktlist` is non-null (checked) and the AU's, valid for the call.
    unsafe {
        with_state_mut::<MidiOutState, _>(
            user_data,
            NOT_INSTALLED,
            CANNOT_DO_IN_CURRENT_CONTEXT,
            |state| {
                if pktlist.is_null() {
                    return NOT_INSTALLED;
                }
                dispatch(state, midi_out_num, pktlist)
            },
        )
    }
}

/// The actual decode-and-deliver body. Split out so
/// [`au_midi_output_callback`] is nothing but the panic guard around it.
///
/// # Safety
/// `state` must be the only reference to the state for the call, and `pktlist` must
/// be a well-formed `MIDIPacketList` valid for it.
unsafe fn dispatch(
    state: &mut MidiOutState,
    midi_out_num: u32,
    pktlist: *const MIDIPacketList,
) -> OSStatus {
    let MidiOutState {
        sink,
        decoders,
        unrouted,
        reset,
    } = &mut *state;
    if reset.swap(false, Ordering::Relaxed) {
        decoders.iter_mut().for_each(Decoder::reset);
    }
    let Some(decoder) = usize::try_from(midi_out_num)
        .ok()
        .and_then(|stream| decoders.get_mut(stream))
    else {
        // SAFETY: `pktlist` is non-null and well formed, per this function's
        // contract.
        unrouted.add(u64::from(unsafe { packet_count(pktlist) }));
        return NO_ERR;
    };
    // A list cut short by a panic last time leaves events staged; they belong
    // to a block that is gone.
    decoder.begin_list();
    let mut flush = |events: &[WireMidi]| (*sink)(midi_out_num, events);
    // SAFETY: `pktlist` is non-null (checked) and a valid packet list for the
    // call, which is `for_each_packet`'s contract.
    unsafe {
        for_each_packet(pktlist, |frame_offset, bytes| {
            decoder.decode(bytes, frame_offset, &mut flush);
        })
    };
    // The tail of the list is flushed whole, never held for the next one: a
    // note-off held back is a stuck voice.
    decoder.flush(&mut flush);
    NO_ERR
}

/// The packets a `MIDIPacketList` says it holds, capped at what fits.
///
/// The list is at most `MAX_LIST_BYTES` (`MIDIServices.h`: a `MIDIPacketList` is
/// never larger than 65536 bytes), and a packet takes at least its header, so a
/// count past what fits is a lie: it is capped here, and the walk stops at the
/// byte bound whatever the count says.
///
/// # Safety
/// `pktlist` must be a non-null, well-formed `MIDIPacketList`, whose
/// `numPackets` header field lies within it.
unsafe fn packet_count(pktlist: *const MIDIPacketList) -> u32 {
    // SAFETY: the field lies within the list (unaligned read, as the list may be
    // packed).
    let num_packets = unsafe {
        std::ptr::read_unaligned(
            (pktlist as *const u8).add(std::mem::offset_of!(MIDIPacketList, numPackets))
                as *const u32,
        )
    };
    num_packets.min(MAX_PACKETS)
}

/// Walks a `MIDIPacketList`, calling `f(frame_offset, packet_bytes)` once per
/// packet. A packet's bytes are a stream, not a message: [`Decoder`] reads
/// them (see below).
///
/// # Why the walk is by byte offset rather than by struct read
///
/// The binding's `MIDIPacket` declares its `data[256]` inline, so a by-value read
/// would run past the end of a real list, whose packets are as short as their
/// payload. Each field is read at its offset instead.
///
/// # Safety
/// The OS gives no byte size of the list, so the bound this walk keeps is the
/// nominal 64 KiB `MAX_LIST_BYTES`: a list the AU built smaller, with a
/// `numPackets` or lengths that run past what it allocated, is read to that
/// bound regardless. That risk is the AU's to not take (a well-formed list
/// satisfies it); the bound limits it, and the walk never reads past 64 KiB.
///
/// `pktlist` must be a well-formed, non-null `MIDIPacketList`: `numPackets`
/// packets laid out per `MIDIPacketNext`, each `length` bytes of `data` within the
/// allocation.
unsafe fn for_each_packet(pktlist: *const MIDIPacketList, mut f: impl FnMut(u32, &[u8])) {
    // SAFETY: `pktlist` is a valid packet list, which is `packet_count`'s contract.
    let num_packets = unsafe { packet_count(pktlist) };
    // The first packet begins at the `packet` field's offset — 4, immediately
    // after `numPackets`.
    let first = std::mem::offset_of!(MIDIPacketList, packet);
    // SAFETY: the first packet starts at `offset_of!(MIDIPacketList, packet)`, within the list.
    let mut p = unsafe { (pktlist as *const u8).add(first) };
    // Bytes of the list walked so far, from its start.
    let mut walked = first;

    for _ in 0..num_packets {
        // A packet's header must lie within the list's documented size.
        if walked + PACKET_HEADER_SIZE > MAX_LIST_BYTES {
            break;
        }
        // SAFETY: `p` points at a packet header inside the list: the walk advances only by each packet's own length, and stops at the documented size.
        let time_stamp = unsafe {
            std::ptr::read_unaligned(
                p.add(std::mem::offset_of!(MIDIPacket, timeStamp)) as *const u64
            )
        };
        // SAFETY: as for the timestamp read above: within the packet header.
        let length = unsafe {
            std::ptr::read_unaligned(p.add(std::mem::offset_of!(MIDIPacket, length)) as *const u16)
        };
        // A packet claiming more than Apple's own documented maximum (`Byte
        // data[256]`) is malformed, and where the next one starts cannot be
        // told from it: the walk ends here rather than read on from a length
        // it cannot trust. (A *legal* over-claim, a packet declaring 64 bytes
        // and meaning 3, is indistinguishable from an honest one, and the
        // bytes are inside the allocation the AU sized.)
        if length > MAX_PACKET_PAYLOAD
            || walked + PACKET_HEADER_SIZE + usize::from(length) > MAX_LIST_BYTES
        {
            break;
        }
        // SAFETY: the data follows the packet header, within the packet.
        let data = unsafe { p.add(PACKET_HEADER_SIZE) };

        // Apple's header: "The time stamp values contained within the MIDIPackets
        // in this list are **sample offsets** from the AudioTimeStamp provided."
        // So the value is already the frame offset this host wants — no host-time
        // conversion. It is a `u64` at the ABI and a `u32` in `WireMidi`; a value
        // past `u32::MAX` saturates (wrapping would place a late event at the
        // start of the block), and the consumer clamps any offset into its block.
        let frame_offset = u32::try_from(time_stamp).unwrap_or(u32::MAX);

        // SAFETY: `length` bytes follow the header (at most the maximum packet size), within the list the AU built.
        let bytes = unsafe { std::slice::from_raw_parts(data, usize::from(length)) };
        f(frame_offset, bytes);

        // `MIDIPacketNext`: advance past the header and payload, then align. On
        // arm64 Apple rounds up to 4; on x86_64 it does not round at all, and
        // rounding there would skip bytes of the next packet.
        // SAFETY: the end of this packet, from which the next header starts (rounded on ARM by `next_packet`).
        let raw_next = unsafe { p.add(PACKET_HEADER_SIZE + usize::from(length)) };
        let next = next_packet(raw_next);
        walked += next as usize - p as usize;
        p = next;
    }
}

/// The most bytes a `MIDIPacketList` holds: `MIDIServices.h` caps one at
/// 65536 bytes.
const MAX_LIST_BYTES: usize = 65_536;

/// The most packets such a list can hold: its count field, then a header each.
#[allow(clippy::cast_possible_truncation, reason = "65532 / 10 fits a u32")]
const MAX_PACKETS: u32 = ((MAX_LIST_BYTES - 4) / PACKET_HEADER_SIZE) as u32;

/// The `MIDIPacket` header size — `timeStamp` (8) + `length` (2) = 10 bytes,
/// with `data` starting immediately after.
///
/// Taken from the binding's own field offset rather than written as `10`, and
/// pinned by `tests::the_packet_abi_is_what_the_walk_assumes`: the whole walk is
/// arithmetic on this number, so a drift here mis-decodes every packet while
/// nothing else notices.
const PACKET_HEADER_SIZE: usize = std::mem::offset_of!(MIDIPacket, data);

/// The largest `length` a `MIDIPacket` may legally declare.
///
/// `MIDIServices.h` declares the payload as `Byte data[256]`, Apple's own bound
/// rather than a number picked here. Written as the literal, not derived — the
/// derivation was tried first: `size_of::<MIDIPacket>() - PACKET_HEADER_SIZE`
/// gives **258**, not 256, because the 4-byte-aligned struct (header 10 + data
/// 256 = 266, rounded to 268) carries two trailing padding bytes, and a bound
/// two bytes too generous lets a hostile packet address slack the AU never
/// wrote — exactly what the clamp exists to stop. Checked against the struct in
/// `tests::the_packet_abi_is_what_the_walk_assumes` instead, which fails if a
/// future SDK changes the array.
const MAX_PACKET_PAYLOAD: u16 = 256;

/// Advances a raw end-of-packet pointer to where the next packet starts,
/// reproducing Apple's `MIDIPacketNext` macro.
///
/// The architecture split is Apple's, and it is load-bearing in both directions:
/// on arm64 `MIDIPacket` must be 4-byte aligned so the macro rounds up, and
/// skipping that lands mid-header on every packet after the first whose payload
/// length is not a multiple of 4 (measured: a 3-byte message ends at +17 and the
/// next packet is at +20). On x86_64 packets are packed unaligned and rounding up
/// would *skip* bytes of the next packet's header.
#[inline]
fn next_packet(raw_next: *const u8) -> *const u8 {
    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    {
        const ALIGN: usize = align_of::<u32>();
        ((raw_next as usize + (ALIGN - 1)) & !(ALIGN - 1)) as *const u8
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
    {
        let _ = align_of::<u32>();
        raw_next
    }
}

/// Test-only: run the decoder over a packet list and collect what came out.
///
/// `#[doc(hidden)]` and `cfg(test)`-free by necessity (integration tests do not
/// see `cfg(test)`), but it is not part of the supported surface.
///
/// # Safety
/// `pktlist` must be a well-formed, non-null `MIDIPacketList`.
#[doc(hidden)]
#[cfg(feature = "testing")]
pub unsafe fn decode_packet_list_for_test(pktlist: *const MIDIPacketList) -> Vec<WireMidi> {
    let mut out = Vec::new();
    let mut decoder = Decoder::new(MidiGroup::FIRST);
    let mut flush = |events: &[WireMidi]| out.extend_from_slice(events);
    // SAFETY: the test passes a packet list it built; see `for_each_packet`.
    unsafe {
        for_each_packet(pktlist, |frame_offset, bytes| {
            decoder.decode(bytes, frame_offset, &mut flush);
        })
    };
    decoder.flush(&mut flush);
    out
}

/// Test-only: the short messages the walk found, before widening, as their
/// MIDI 1.0 bytes (running status expanded). A SysEx is not here: it arrives as
/// SysEx7 packets ([`decode_packet_list_for_test`]).
///
/// Separate from [`decode_packet_list_for_test`] so a test can tell "the walk
/// missed the message" from "the walk found it and the widening lost it": two
/// failures with the same visible symptom.
///
/// # Safety
/// As [`decode_packet_list_for_test`].
#[doc(hidden)]
#[cfg(feature = "testing")]
pub unsafe fn split_packet_list_for_test(pktlist: *const MIDIPacketList) -> Vec<(u32, Vec<u8>)> {
    let mut out = Vec::new();
    let mut parser = Midi1Parser::new(MidiGroup::FIRST);
    // SAFETY: the test passes a packet list it built; see `for_each_packet`.
    unsafe {
        for_each_packet(pktlist, |frame_offset, bytes| {
            for message in parser.feed_all(bytes) {
                for short in midi1(message) {
                    out.push((frame_offset, short.as_slice().to_vec()));
                }
            }
            parser.end_of_message();
        })
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;
    use std::sync::atomic::AtomicU64;
    use tutti_ump::NoteKeyEvent;

    /// The hand-declared callback struct must match Apple's: an
    /// `AUMIDIOutputCallback` function pointer then a `void *userData`, 16 bytes
    /// on a 64-bit target. If the fields were transposed the AU would call
    /// `userData` as code on its render thread.
    #[test]
    fn the_callback_struct_matches_the_c_abi() {
        assert_eq!(
            size_of::<AuMidiOutputCallbackStruct>(),
            2 * size_of::<usize>()
        );
        assert_eq!(
            align_of::<AuMidiOutputCallbackStruct>(),
            align_of::<usize>()
        );
        assert_eq!(
            std::mem::offset_of!(AuMidiOutputCallbackStruct, midi_output_callback),
            0,
            "the function pointer is first"
        );
        assert_eq!(
            std::mem::offset_of!(AuMidiOutputCallbackStruct, user_data),
            size_of::<usize>()
        );
        // `Option<fn>` must be niche-optimised to a bare pointer, or the null
        // withdrawal writes the wrong bytes.
        assert_eq!(
            size_of::<Option<AuMidiOutputCallbackFn>>(),
            size_of::<usize>()
        );
    }

    /// The packet ABI the walk's arithmetic rests on, taken from the binding
    /// rather than assumed. Verified against Apple's `MIDIServices.h` on macOS
    /// 15.6: `timeStamp` (`MIDITimeStamp`, 8 bytes) then `length` (`UInt16`) then
    /// `data`, with the list's `numPackets` first and `packet` at 4.
    #[test]
    fn the_packet_abi_is_what_the_walk_assumes() {
        assert_eq!(std::mem::offset_of!(MIDIPacketList, numPackets), 0);
        assert_eq!(
            std::mem::offset_of!(MIDIPacketList, packet),
            4,
            "the first packet begins right after the u32 count"
        );
        assert_eq!(std::mem::offset_of!(MIDIPacket, timeStamp), 0);
        assert_eq!(std::mem::offset_of!(MIDIPacket, length), 8);
        assert_eq!(
            PACKET_HEADER_SIZE, 10,
            "timeStamp (8) + length (2); the whole walk is arithmetic on this"
        );
        // The reason the walk never reads a `MIDIPacket` by value: the declared
        // size is vastly larger than a real 3-byte packet occupies, so a by-value
        // read would run past the end of the list.
        assert!(
            size_of::<MIDIPacket>() > PACKET_HEADER_SIZE + 3,
            "the binding's MIDIPacket includes a fixed data array, which is why \
             the walk reads fields at offsets instead"
        );
        // The clamp bound, checked against the struct rather than restated. Apple
        // declares `Byte data[256]`; the struct rounds 10 + 256 up to 268 for its
        // 4-byte alignment, so the payload sits in `size_of - header` MINUS the two
        // padding bytes. If a future SDK widens the array, this inequality is what
        // catches `MAX_PACKET_PAYLOAD` having gone stale.
        assert_eq!(
            size_of::<MIDIPacket>(),
            268,
            "MIDIServices.h declares timeStamp(8) + length(2) + data[256], aligned \
             to 4 = 268. If this changed, MAX_PACKET_PAYLOAD needs revisiting."
        );
        assert_eq!(
            MAX_PACKET_PAYLOAD, 256,
            "the documented payload maximum, which must be the array's length and \
             NOT `size_of - header` (that is 258 — it counts the trailing padding, \
             and a bound two bytes too generous is slack the AU never wrote)"
        );
        assert!(
            (MAX_PACKET_PAYLOAD as usize) < size_of::<MIDIPacket>() - PACKET_HEADER_SIZE + 1,
            "the bound must not exceed the addressable payload"
        );
        // And it must be well under what a `u16` length can claim, or the clamp is
        // not doing anything: 65535 is 255 packets' worth of memory the AU never
        // wrote. A `const` block so this is a compile-time check rather than a
        // runtime one — both operands are constants, and clippy is right that a
        // runtime `assert!` on them is the wrong tool.
        const { assert!(MAX_PACKET_PAYLOAD < u16::MAX) };
    }

    /// A packet claiming more than Apple's 256-byte maximum ends the walk.
    ///
    /// A `u16` length can say 65535, and where the next packet starts cannot be
    /// told from a length that is a lie: reading on from a clamped one would read
    /// whatever lies there as packets. Nothing of the list after it is read.
    #[test]
    fn a_length_beyond_apples_maximum_ends_the_walk() {
        // `numPackets = 2`: a packet declaring u16::MAX, then a real one that
        // must not be reached; 4 KiB of slack so a failure fails an assertion
        // rather than faulting.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2u32.to_ne_bytes());
        bytes.extend_from_slice(&0u64.to_ne_bytes());
        bytes.extend_from_slice(&u16::MAX.to_ne_bytes());
        bytes.extend_from_slice(&[0x90, 60, 100]);
        bytes.resize(bytes.len() + 4096, 0);

        // SAFETY: a well-formed header over a live 4 KiB+ allocation; the packet's
        // declared length is the thing under test and the slack bounds any
        // over-read.
        let found = unsafe { split_packet_list_for_test(bytes.as_ptr() as *const MIDIPacketList) };
        assert!(
            found.is_empty(),
            "nothing is read past a lying length: {found:?}"
        );
    }

    /// A packet count the list's documented size cannot hold is capped, and the
    /// walk ends at that size: a list of `MAX_LIST_BYTES` zero bytes claiming
    /// `u32::MAX` packets visits at most as many as fit, and never reads past it.
    #[test]
    fn a_packet_count_past_the_documented_size_is_bounded() {
        let mut bytes = vec![0u8; MAX_LIST_BYTES];
        bytes[..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        let mut visited = 0u32;
        // SAFETY: a `MAX_LIST_BYTES` allocation, the largest a list is; the
        // header's count is the thing under test.
        unsafe {
            for_each_packet(bytes.as_ptr() as *const MIDIPacketList, |_, payload| {
                assert!(payload.is_empty());
                visited += 1;
            });
        }
        assert!(visited > 0 && visited <= MAX_PACKETS, "visited {visited}");
    }

    /// Builds a list with Apple's OWN `MIDIPacketListInit`/`MIDIPacketListAdd` and
    /// check this walk finds every message at the offsets Apple wrote them to.
    #[test]
    fn the_walk_matches_apples_own_packing() {
        let mut storage = vec![0u8; 4096];
        let list = storage.as_mut_ptr() as *mut MIDIPacketList;
        // SAFETY: `storage` is a 4096-byte allocation, far more than the four
        // small packets added below need, and `list` points at its start.
        let messages: [(&[u8], u64); 4] = [
            (&[0x90, 60, 100], 0),
            (&[0x80, 60, 0], 128),
            (&[0xB0, 7, 64], 256),
            // Two note-ons packed into ONE packet — Apple's own packer does this,
            // which is why the splitter exists.
            (&[0x90, 62, 90, 0x90, 64, 80], 384),
        ];
        // SAFETY: `list` is a buffer this test sized for the packets it adds with `MIDIPacketListAdd`, within its capacity.
        unsafe {
            let mut pkt = coreaudio_sys::MIDIPacketListInit(list);
            for (bytes, ts) in messages {
                pkt = coreaudio_sys::MIDIPacketListAdd(
                    list,
                    storage.len() as u64,
                    pkt,
                    ts,
                    bytes.len() as u64,
                    bytes.as_ptr(),
                );
                assert!(!pkt.is_null(), "packet did not fit in 4 KiB");
            }
        }

        // SAFETY: `list` is the packet list this test just built.
        let found = unsafe { split_packet_list_for_test(list) };
        // Five messages from four packets: the last packet held two.
        assert_eq!(
            found.len(),
            5,
            "expected 5 messages from 4 packets (the last packet carries two); \
             got {found:?}"
        );
        assert_eq!(found[0], (0, vec![0x90, 60, 100]));
        assert_eq!(found[1], (128, vec![0x80, 60, 0]));
        assert_eq!(found[2], (256, vec![0xB0, 7, 64]));
        // Both messages of the multi-message packet carry that packet's offset.
        assert_eq!(found[3], (384, vec![0x90, 62, 90]));
        assert_eq!(found[4], (384, vec![0x90, 64, 80]));

        // And the UMP conversion, so the two halves are pinned separately.
        // SAFETY: `list` is the packet list this test just built.
        let events = unsafe { decode_packet_list_for_test(list) };
        assert_eq!(events.len(), 5);
        let note_on = |e: &WireMidi| match e.ump.note_key_event() {
            Some(NoteKeyEvent::On(key)) => Some(key.note().get()),
            _ => None,
        };
        assert_eq!(note_on(&events[0]), Some(60));
        assert_eq!(events[0].offset, 0);
        assert!(events[1].ump.is_note_off());
        assert_eq!(note_on(&events[4]), Some(64));
        assert_eq!(events[4].offset, 384);
    }

    /// **A list for a stream with no reader counts its packets**, each of which
    /// carries at least one lost message, not one for the list: three packets
    /// to stream 5 of a unit that published one stream read as 3, and the list
    /// reaches no sink.
    #[test]
    fn an_unrouted_list_counts_its_packets() {
        let mut storage = vec![0u8; 4096];
        let packets = storage.as_mut_ptr() as *mut MIDIPacketList;
        // SAFETY: `packets` is a buffer sized for the three small packets added.
        unsafe {
            let mut pkt = coreaudio_sys::MIDIPacketListInit(packets);
            for (n, bytes) in [[0x90, 60, 100], [0x80, 60, 0], [0xB0, 7, 64]]
                .iter()
                .enumerate()
            {
                pkt = coreaudio_sys::MIDIPacketListAdd(
                    packets,
                    storage.len() as u64,
                    pkt,
                    n as u64,
                    bytes.len() as u64,
                    bytes.as_ptr(),
                );
                assert!(!pkt.is_null());
            }
        }
        let delivered = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&delivered);
        let unrouted = tutti_rt::tally::<u64>();
        let mut state = MidiOutState {
            sink: Box::new(move |_, events| {
                seen.fetch_add(events.len() as u64, Ordering::Relaxed);
            }),
            decoders: Box::new([Decoder::new(MidiGroup::FIRST)]),
            unrouted: unrouted.writer(),
            reset: Arc::new(AtomicBool::new(false)),
        };
        // SAFETY: `state` is the only reference, and `packets` is the list built
        // above.
        let status = unsafe { dispatch(&mut state, 5, packets) };
        assert_eq!(status, NO_ERR);
        assert_eq!(unrouted.get(), 3);
        assert_eq!(delivered.load(Ordering::Relaxed), 0);
    }

    /// **A withdrawal with a render in flight frees nothing, and can be retried**:
    /// it is refused and hands the registration back with the claim and the state
    /// a running render may read; a unit with a registration still refuses
    /// another. Once the render ends the same registration withdraws and frees,
    /// and the unit takes a new one.
    #[test]
    fn a_withdrawal_during_a_render_is_refused_and_retried() {
        let key = crate::ComponentKey::new(
            coreaudio_sys::kAudioUnitType_Effect,
            u32::from_be_bytes(*b"dely"),
            u32::from_be_bytes(*b"appl"),
        )
        .expect("an exact key");
        let comp = crate::ComponentRef::find(key).expect("AUDelay ships with macOS");
        let (_work, handle) = tutti_plugin_types::MainWork::new();
        let unit = Unit::new(comp, &handle).expect("instantiate");
        assert!(unit.no_render_in_flight());

        let registration = install(&unit, Box::new(|_, _| {})).expect("install");
        let in_flight = unit.rendering();
        assert!(!unit.no_render_in_flight());
        let Err((registration, Error::Invalid(_))) = registration.withdraw() else {
            panic!("a withdrawal with a render in flight is refused");
        };
        assert!(
            install(&unit, Box::new(|_, _| {})).is_err(),
            "the refused registration keeps the claim"
        );
        drop(in_flight);
        assert!(
            unit.no_render_in_flight(),
            "the guard's drop ends the render"
        );

        registration.withdraw().expect("no render in flight: freed");
        let again = install(&unit, Box::new(|_, _| {})).expect("the claim was released");
        again.withdraw().expect("freed");
    }

    /// **A withdrawal on an initialized unit is refused**: an AU may have taken
    /// its callback at initialization, so the withdrawal hands the registration
    /// back with its claim; once the unit is uninitialized the same registration
    /// withdraws, and the unit takes a new one. (That the refusal writes nothing,
    /// and that the state stays allocated, is the documented behaviour of
    /// `withdraw`; neither is observable here.)
    #[test]
    fn a_withdrawal_on_an_initialized_unit_is_refused_until_it_is_uninitialized() {
        let key = crate::ComponentKey::new(
            coreaudio_sys::kAudioUnitType_Effect,
            u32::from_be_bytes(*b"dely"),
            u32::from_be_bytes(*b"appl"),
        )
        .expect("an exact key");
        let comp = crate::ComponentRef::find(key).expect("AUDelay ships with macOS");
        let (_work, handle) = tutti_plugin_types::MainWork::new();
        let unit = Unit::new(comp, &handle).expect("instantiate");
        let registration = install(&unit, Box::new(|_, _| {})).expect("install");
        unit.initialize().expect("initialize");
        let Err((registration, Error::Invalid(_))) = registration.withdraw() else {
            panic!("a withdrawal on an initialized unit is refused");
        };
        assert!(
            install(&unit, Box::new(|_, _| {})).is_err(),
            "the refused registration keeps the claim"
        );
        unit.uninitialize().expect("uninitialize");
        registration.withdraw().expect("uninitialized: freed");
        install(&unit, Box::new(|_, _| {}))
            .expect("the claim was released")
            .withdraw()
            .expect("freed");
    }

    /// What `decoder` makes of `bytes` as one list at `offset`.
    fn list(decoder: &mut Decoder, bytes: &[u8], offset: u32) -> Vec<WireMidi> {
        let mut out = Vec::new();
        decoder.begin_list();
        decoder.decode(bytes, offset, &mut |events| out.extend_from_slice(events));
        decoder.flush(&mut |events| out.extend_from_slice(events));
        out
    }

    /// **A parameter run assembles across lists**: the Data Entry LSB that ends
    /// a Registered Parameter run arrives in a later list than the select and
    /// the MSB (M2-104 Appendix D.3.3), and the run is one controller (pitch
    /// bend sensitivity of 12 semitones, zero-extended, M2-115 section 4.1).
    /// A decoder that widened each message alone would send three Control
    /// Changes and then a fourth.
    /// Spec: m2-115/4.1#1
    #[test]
    fn a_parameter_run_assembles_across_lists() {
        let mut decoder = Decoder::new(MidiGroup::FIRST);
        let first = list(&mut decoder, &[0xB0, 101, 0, 0xB0, 100, 0, 0xB0, 6, 12], 0);
        assert!(first.is_empty(), "a select and an MSB end no run");
        let second = list(&mut decoder, &[0xB0, 38, 0], 5);
        let [event] = second[..] else {
            panic!("expected one Registered Controller, got {second:?}");
        };
        assert_eq!(event.offset, 5);
        let Ok(Message::Midi2(_, tutti_ump::Voice2::Registered { bank, index, value })) =
            event.ump.decode()
        else {
            panic!(
                "expected a Registered Controller, got {:?}",
                event.ump.decode()
            );
        };
        assert_eq!((bank.get(), index.get(), value.get()), (0, 0, 12 << 25));

        // After a reset the run is forgotten: with nothing selected, the same
        // Data Entry LSB is dropped (it would otherwise complete the held run
        // into a Registered Controller).
        list(&mut decoder, &[0xB0, 101, 0, 0xB0, 100, 0, 0xB0, 6, 12], 0);
        decoder.reset();
        let after = list(&mut decoder, &[0xB0, 38, 0], 0);
        assert!(after.is_empty(), "got {after:?}");
    }

    /// **An output stream is the Group of its number**: its messages, SysEx
    /// included, carry that Group.
    #[test]
    fn a_stream_is_the_group_of_its_number() {
        let mut decoder = Decoder::new(MidiGroup::of(3));
        let events = list(&mut decoder, &[0x90, 60, 100, 0xF0, 1, 2, 3, 0xF7], 0);
        assert_eq!(events.len(), 2);
        for event in events {
            match event.ump.decode() {
                Ok(Message::Midi2(address, _)) => assert_eq!(address.group, MidiGroup::of(3)),
                Ok(Message::Sysex7(group, _)) => assert_eq!(group, MidiGroup::of(3)),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// **Decoding a list allocates nothing**, a SysEx of several packets and a
    /// parameter run included: the parser holds a packet's payload inline, the
    /// batch and the translator are fixed-size.
    #[test]
    fn decoding_allocates_nothing() {
        let mut decoder = Decoder::new(MidiGroup::FIRST);
        let mut bytes = vec![0xF0];
        bytes.extend((1..=40).map(|b| b % 0x7F));
        bytes.push(0xF7);
        bytes.extend([
            0xB0, 101, 0, 0xB0, 100, 0, 0xB0, 6, 12, 0xB0, 38, 0, 0x90, 60, 100,
        ]);
        let mut seen = 0usize;
        assert_no_alloc::assert_no_alloc(|| {
            decoder.begin_list();
            decoder.decode(&bytes, 0, &mut |events| seen += events.len());
            decoder.flush(&mut |events| seen += events.len());
        });
        // The SysEx (7 packets of at most 6 bytes), the run's one controller
        // and the note.
        assert_eq!(seen, 7 + 1 + 1);
    }

    /// `next_packet` must reproduce `MIDIPacketNext` for this architecture.
    /// Asserted arithmetically as well as against the framework above, because
    /// this is the one place a cross-architecture mistake would be invisible on
    /// the machine it was written on.
    #[test]
    fn next_packet_follows_the_architecture_rule() {
        let base = 0x1000usize;
        for len in 0..8usize {
            let raw = (base + PACKET_HEADER_SIZE + len) as *const u8;
            let got = next_packet(raw) as usize;
            #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
            {
                assert_eq!(
                    got % 4,
                    0,
                    "on ARM MIDIPacket must be 4-byte aligned; len={len}"
                );
                assert!(got >= raw as usize && got - (raw as usize) < 4);
            }
            #[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
            assert_eq!(
                got, raw as usize,
                "on Intel packets are packed unaligned; rounding up would skip \
                 bytes of the next packet"
            );
        }
    }
}
