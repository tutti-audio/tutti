//! An owned `AudioComponentInstance` and the AudioToolbox calls on it.

use std::cell::Cell;
use std::marker::PhantomData;
use std::mem::{MaybeUninit, size_of};
use std::os::raw::c_void;
use std::sync::Arc;
use tutti_rt::Mutex;

use coreaudio_sys as sys;

use crate::cf::{CfArray, CfPlist, CfString, CfUrl, cfstring_to_string_checked, take_string};
use crate::component::ComponentRef;
use crate::error::{Error, OsError, Result, check};
use crate::event_list::{EventList, Wire, send_event_list_fn};
use crate::plain::Plain;
use crate::property::{
    AudioUnitMidiProtocol, Element, FactoryPresets, LastRenderStatus, PresentPreset, Property,
    Scope,
};
use crate::status::NO_ERR;
use tutti_plugin_types::MainWorkHandle;

/// The size and writability of a property, from `AudioUnitGetPropertyInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertyInfo {
    /// The property's size in bytes.
    pub size: u32,
    /// Whether the unit accepts a write.
    pub writable: bool,
}

/// The instance behind every [`Unit`] clone; disposed when the last goes.
///
/// The home is the thread that called [`Unit::new`]. The last drop on the home
/// thread disposes the instance there; a drop on any other thread hands the
/// disposal to the owner's collector (`work`), counted in
/// `MainWork::deferred_drops`. If that collector is gone and the dropping thread
/// is not the main thread, nothing can run the disposal: the instance leaks and
/// is counted in `MainWork::stranded`.
pub(crate) struct Inner {
    pub(crate) instance: sys::AudioComponentInstance,
    /// The thread the unit was made on, which disposes it.
    home: std::thread::ThreadId,
    /// Where a drop off that thread hands the disposal.
    work: MainWorkHandle,
    /// A MIDI-output registration holds the unit
    /// ([`install`](crate::install_midi_output) takes it, a withdrawal gives it
    /// back): `kAudioUnitProperty_MIDIOutputCallback` cannot be read, so this is
    /// how a second registration is refused.
    midi_output: std::sync::atomic::AtomicBool,
    /// The safe render calls of this crate in flight on the unit
    /// ([`Unit::rendering`]): what a withdrawal checks, after its write, before
    /// it frees what the AU's callback reads.
    renders: tutti_rt::RenderCount,
    /// Whether the unit is initialized: `AudioUnitInitialize` has succeeded and
    /// `AudioUnitUninitialize` has not since. The lock is held across each of
    /// those calls and across a withdrawal's free ([`Unit::while_uninitialized`]),
    /// so no `initialize` overlaps the free; only control-side calls take it, the
    /// render path never does.
    initialized: Mutex<bool>,
}

// SAFETY: the instance is an opaque handle into AudioToolbox. Nothing in Rust
// memory is reached through it; every call on it is made through AudioToolbox,
// which documents parameter access as callable from any thread and serializes
// property access (`AUBase` locks properties, never the render path). Moving or
// sharing the handle therefore cannot race Rust data; a call that must not
// overlap another (render against initialize) is ordered by `Unit`'s `&self`
// methods being the host's to schedule, as for every AudioUnit host.
unsafe impl Send for Inner {}
// SAFETY: as for `Send`.
unsafe impl Sync for Inner {}

/// The disposal of an instance, which moves to the thread that runs it.
struct Disposal(sys::AudioComponentInstance);

// SAFETY: the handle is opaque and `AudioComponentInstanceDispose` is called
// once, on the one thread that runs this; see `Inner`'s `Send`.
unsafe impl Send for Disposal {}

impl Disposal {
    fn run(self) {
        // SAFETY: `self.0` came from `AudioComponentInstanceNew` and its last
        // reference is gone, so nothing can use it after this call.
        unsafe { sys::AudioComponentInstanceDispose(self.0) };
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let disposal = Disposal(self.instance);
        if std::thread::current().id() == self.home {
            disposal.run();
        } else {
            // A drop has no token, and an AudioUnit is disposed on the thread it was
            // made on. The disposal goes to the owner's main-thread collector, to run
            // at its next tick. A unit leaves the graph on the control side, so this is
            // never the audio thread (`MainWorkHandle::defer`, "Never from an audio
            // thread").
            self.work.defer_drop(move |_main| disposal.run());
        }
    }
}

/// An instantiated Audio Unit, initialized or not.
///
/// It owns the instance: the last `Unit` (or [`RenderUnit`], listener or other
/// registration made from it) to drop disposes it, so no registration can
/// outlive the unit it was made on. It is `Send` and not `Sync`: one thread at
/// a time drives it, as `AudioUnit` hosts do; [`RenderUnit`] is the handle the
/// render thread holds.
#[derive(Clone)]
pub struct Unit {
    inner: Arc<Inner>,
    _not_sync: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unit").finish_non_exhaustive()
    }
}

impl Unit {
    /// Instantiates `component` synchronously (`AudioComponentInstanceNew`). The
    /// calling thread is the unit's home: a last drop on another thread hands the
    /// disposal to `work`'s collector.
    ///
    /// # Errors
    /// The `OSStatus` of the call; a v3 unit that requires asynchronous
    /// instantiation answers `-10863`.
    pub fn new(
        component: ComponentRef,
        work: &MainWorkHandle,
    ) -> std::result::Result<Self, OsError> {
        let mut instance: sys::AudioComponentInstance = std::ptr::null_mut();
        // SAFETY: `component` came from the registry and `instance` is writable.
        let status = unsafe { sys::AudioComponentInstanceNew(component.raw(), &mut instance) };
        check("AudioComponentInstanceNew", status)?;
        Ok(Self {
            inner: Arc::new(Inner {
                instance,
                home: std::thread::current().id(),
                work: work.clone(),
                midi_output: std::sync::atomic::AtomicBool::new(false),
                renders: tutti_rt::RenderCount::new(),
                initialized: Mutex::new(false),
            }),
            _not_sync: PhantomData,
        })
    }

    /// Takes the unit's MIDI-output claim: `true` when it was free.
    pub(crate) fn claim_midi_output(&self) -> bool {
        !self
            .inner
            .midi_output
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Gives the claim back.
    pub(crate) fn release_midi_output(&self) {
        self.inner
            .midi_output
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// The raw `AudioUnit`, for a call this crate does not wrap. Using it is the
    /// caller's `unsafe`, and it is valid only while a `Unit` clone lives. A
    /// render made through it is not counted: hold [`rendering`](Self::rendering)
    /// across it, or keep it from overlapping
    /// [`AuMidiOutput::withdraw`](crate::AuMidiOutput::withdraw).
    #[cfg(feature = "testing")]
    pub fn as_raw(&self) -> sys::AudioUnit {
        self.inner.instance
    }

    pub(crate) fn raw(&self) -> sys::AudioUnit {
        self.inner.instance
    }

    /// The handle a drop off the home thread hands its work to.
    pub(crate) fn work(&self) -> &MainWorkHandle {
        &self.inner.work
    }

    pub(crate) fn share(&self) -> Arc<Inner> {
        Arc::clone(&self.inner)
    }

    /// A handle for the render thread that keeps the unit alive.
    pub fn render_unit(&self) -> RenderUnit {
        RenderUnit(self.share())
    }

    /// Whether `other` is a handle on the same instance.
    pub fn same_unit(&self, other: &Unit) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    // ---- lifecycle ----

    /// `AudioUnitInitialize`. Serialized with every other lifecycle call and with
    /// a withdrawal's free of what the AU's callbacks read.
    ///
    /// # Errors
    /// The `OSStatus` of the call. The unit is then taken to be uninitialized (a
    /// refused `AudioUnitInitialize` leaves it so), which is what lets a
    /// withdrawal free; an AU that took its callbacks before refusing would
    /// break that, and none measured does.
    pub fn initialize(&self) -> std::result::Result<(), OsError> {
        let mut initialized = self.inner.initialized.lock();
        // SAFETY: `raw()` is a live instance.
        check("AudioUnitInitialize", unsafe {
            sys::AudioUnitInitialize(self.raw())
        })?;
        *initialized = true;
        Ok(())
    }

    /// `AudioUnitUninitialize`. Serialized as [`initialize`](Self::initialize) is.
    ///
    /// Must not overlap a render of the unit on any thread (a `Unit` clone may
    /// be rendering): whether the AU waits for a render in flight is the AU's, and
    /// nothing here orders the two. The host keeps the lifecycle calls on the
    /// thread that renders, or between its blocks.
    ///
    /// # Errors
    /// The `OSStatus` of the call, which leaves the unit initialized.
    pub fn uninitialize(&self) -> std::result::Result<(), OsError> {
        let mut initialized = self.inner.initialized.lock();
        // SAFETY: `raw()` is a live instance.
        check("AudioUnitUninitialize", unsafe {
            sys::AudioUnitUninitialize(self.raw())
        })?;
        *initialized = false;
        Ok(())
    }

    /// Runs `f` while the unit is known uninitialized and no `initialize` can
    /// begin: `None`, without running it, when the unit is initialized (or its
    /// `AudioUnitUninitialize` failed). A unit that was never initialized is
    /// uninitialized.
    pub(crate) fn while_uninitialized<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let initialized = self.inner.initialized.lock();
        if *initialized {
            return None;
        }
        let result = f();
        drop(initialized);
        Some(result)
    }

    /// Counts a render of the unit as in flight until the guard drops. Every
    /// call this crate makes that can run the unit's MIDI-output callback
    /// (`AudioUnitRender`, `AudioUnitProcess`, `AudioUnitProcessMultiple`) holds
    /// one across it. Two atomic operations and a fence
    /// ([`RenderCount::enter`](tutti_rt::RenderCount::enter)): no lock, so the
    /// render thread may call it. A guard that is leaked counts forever, so
    /// [`AuMidiOutput::withdraw`](crate::AuMidiOutput::withdraw) on this unit
    /// always refuses and leaves the callback's state allocated.
    ///
    /// A render made through the raw handle (`as_raw` (the `testing` feature),
    /// [`RenderUnit::get`]) is not counted for its caller: it holds this guard
    /// across the call, or it does not overlap
    /// [`AuMidiOutput::withdraw`](crate::AuMidiOutput::withdraw), which frees the
    /// state the unit's MIDI-output callback reads once the count shows no render.
    pub fn rendering(&self) -> tutti_rt::RenderGuard<'_> {
        self.inner.renders.enter()
    }

    /// Whether no render of this crate's is in flight on the unit, as of a
    /// fence after the caller's last write to the unit
    /// ([`RenderCount::quiescent`](tutti_rt::RenderCount::quiescent)).
    pub(crate) fn no_render_in_flight(&self) -> bool {
        self.inner.renders.quiescent()
    }

    /// The unit's `kAudioUnitProperty_LastRenderError`, when it is readable and
    /// not `noErr`: what the AU last failed a render with. A property read, so
    /// control side only, after a render reported a failure.
    pub fn last_render_error(&self) -> Option<i32> {
        self.get::<LastRenderStatus>(Scope::GLOBAL, Element::ZERO)
            .ok()
            .filter(|&e| e != NO_ERR)
    }

    /// `AudioUnitReset` on `scope` and `element`.
    ///
    /// # Errors
    /// The `OSStatus` of the call.
    pub fn reset(&self, scope: Scope, element: Element) -> std::result::Result<(), OsError> {
        // SAFETY: `raw()` is a live instance.
        check("AudioUnitReset", unsafe {
            sys::AudioUnitReset(self.raw(), scope.raw(), element.0)
        })
    }

    // ---- properties ----

    /// Reads a `T`-valued property.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitGetProperty`.
    fn get_raw<T: Plain>(&self, id: u32, scope: u32, element: u32) -> Result<T> {
        // SAFETY: all-zero is valid for a `Plain` type (its contract).
        let zeroed = unsafe { MaybeUninit::<T>::zeroed().assume_init() };
        self.get_in_out_raw(id, scope, element, zeroed)
    }

    /// Reads a property whose request is itself a value (a string-from-value
    /// query): `request` goes in and the unit's answer comes back.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitGetProperty`.
    fn get_in_out_raw<T: Plain>(
        &self,
        id: u32,
        scope: u32,
        element: u32,
        mut request: T,
    ) -> Result<T> {
        let mut size = size_of::<T>() as u32;
        // SAFETY: `request` is `size` writable bytes, and `T: Plain` accepts
        // whatever the unit writes into them.
        let status = unsafe {
            sys::AudioUnitGetProperty(
                self.raw(),
                id,
                scope,
                element,
                (&mut request as *mut T).cast::<c_void>(),
                &mut size,
            )
        };
        check("AudioUnitGetProperty", status)?;
        Ok(request)
    }

    /// Writes a `T`-valued property.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitSetProperty`.
    fn set_raw<T: Plain>(&self, id: u32, scope: u32, element: u32, value: &T) -> Result<()> {
        self.set_value(id, scope, element, value, size_of::<T>())
    }

    /// Writes only the first `len` bytes of `value` (a variable-size record
    /// whose tail the property does not take, such as an `AudioChannelLayout`
    /// without channel descriptions); `len` is clamped to the size of `T`.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitSetProperty`.
    fn set_prefix_raw<T: Plain>(
        &self,
        id: u32,
        scope: u32,
        element: u32,
        value: &T,
        len: usize,
    ) -> Result<()> {
        self.set_value(id, scope, element, value, len.min(size_of::<T>()))
    }

    /// Writes the first `len` bytes of `*value` (`len <= size_of::<T>()`). The
    /// pointer goes straight to AudioToolbox: no byte slice is formed, so a
    /// padded `T` is never read as initialized bytes by Rust.
    fn set_value<T: Plain>(
        &self,
        id: u32,
        scope: u32,
        element: u32,
        value: &T,
        len: usize,
    ) -> Result<()> {
        // SAFETY: `value` is a live `T` and `len <= size_of::<T>()`.
        let status = unsafe {
            sys::AudioUnitSetProperty(
                self.raw(),
                id,
                scope,
                element,
                (value as *const T).cast::<c_void>(),
                len as u32,
            )
        };
        check("AudioUnitSetProperty", status)?;
        Ok(())
    }

    /// Writes a property from raw bytes (a variable-size record).
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitSetProperty`.
    fn set_bytes_raw(&self, id: u32, scope: u32, element: u32, bytes: &[u8]) -> Result<()> {
        // SAFETY: `bytes` is a live slice of the length passed.
        let status = unsafe {
            sys::AudioUnitSetProperty(
                self.raw(),
                id,
                scope,
                element,
                bytes.as_ptr().cast::<c_void>(),
                bytes.len() as u32,
            )
        };
        check("AudioUnitSetProperty", status)?;
        Ok(())
    }

    /// The size and writability of a property, without reading it.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitGetPropertyInfo`.
    fn property_info_raw(&self, id: u32, scope: u32, element: u32) -> Result<PropertyInfo> {
        let mut size: u32 = 0;
        // `outWritable` is a C `Boolean`, an unsigned char.
        let mut writable: u8 = 0;
        // SAFETY: both out-parameters are writable locals.
        let status = unsafe {
            sys::AudioUnitGetPropertyInfo(self.raw(), id, scope, element, &mut size, &mut writable)
        };
        check("AudioUnitGetPropertyInfo", status)?;
        Ok(PropertyInfo {
            size,
            writable: writable != 0,
        })
    }

    /// Reads a variable-size property into an owned byte buffer.
    ///
    /// # Errors
    /// The `OSStatus` of either call.
    fn get_bytes_raw(&self, id: u32, scope: u32, element: u32) -> Result<Vec<u8>> {
        let mut size = self.property_info_raw(id, scope, element)?.size;
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` is `size` writable bytes.
        let status = unsafe {
            sys::AudioUnitGetProperty(
                self.raw(),
                id,
                scope,
                element,
                buf.as_mut_ptr().cast::<c_void>(),
                &mut size,
            )
        };
        check("AudioUnitGetProperty", status)?;
        buf.truncate(size as usize);
        Ok(buf)
    }

    /// Reads a variable-size property that is an array of `T` records. Trailing
    /// bytes short of a whole record are dropped rather than trusted.
    ///
    /// # Errors
    /// The `OSStatus` of either call.
    fn get_array_raw<T: Plain>(&self, id: u32, scope: u32, element: u32) -> Result<Vec<T>> {
        let bytes = self.get_bytes_raw(id, scope, element)?;
        Ok(bytes
            .chunks_exact(size_of::<T>())
            .map(|record| {
                // SAFETY: `record` is exactly `size_of::<T>()` bytes, and any
                // bit pattern is a valid `T` (`Plain`); unaligned because a
                // `Vec<u8>` guarantees one byte.
                unsafe { record.as_ptr().cast::<T>().read_unaligned() }
            })
            .collect())
    }

    /// Reads a property that is a copied `CFString`; `None` when the unit
    /// answers null, or something that is not a string.
    ///
    /// # Errors
    /// The `OSStatus` of the read.
    fn copy_string_raw(&self, id: u32, scope: u32, element: u32) -> Result<Option<String>> {
        let raw: sys::CFStringRef = self.get_cf(id, scope, element)?;
        // SAFETY: a CFString-valued property is returned under the Copy rule.
        Ok(unsafe { take_string(raw) })
    }

    /// Reads a property that is a copied `CFURL`, as a filesystem path.
    ///
    /// # Errors
    /// The `OSStatus` of the read.
    fn copy_url_path_raw(&self, id: u32, scope: u32, element: u32) -> Result<Option<String>> {
        let raw: sys::CFURLRef = self.get_cf(id, scope, element)?;
        // SAFETY: a CFURL-valued property is returned under the Copy rule.
        Ok(unsafe { CfUrl::from_copied(raw) }.and_then(|u| u.to_path_string()))
    }

    /// Reads a property that is a copied `CFArray` of `CFString`s; an entry is
    /// `None` when it is not a string.
    ///
    /// # Errors
    /// The `OSStatus` of the read.
    fn copy_string_array_raw(
        &self,
        id: u32,
        scope: u32,
        element: u32,
    ) -> Result<Vec<Option<String>>> {
        let array = self.copy_array(id, scope, element)?;
        Ok((0..array.len())
            .map(|i| {
                let ptr = array.value_at(i)?;
                // SAFETY: the array's elements are borrowed from it, live while
                // `array` is; the checked copier tolerates non-strings.
                unsafe { cfstring_to_string_checked(ptr as sys::CFStringRef) }
            })
            .collect())
    }

    fn copy_array(&self, id: u32, scope: u32, element: u32) -> Result<CfArray> {
        let raw: sys::CFArrayRef = self.get_cf(id, scope, element)?;
        // SAFETY: an array-valued property is returned under the Copy rule.
        unsafe { CfArray::from_copied(raw) }
            .ok_or_else(|| Error::Invalid("the unit answered a null array".into()))
    }

    /// The unit's factory presets as `(number, name)`. A name the unit hands
    /// back in a way that is not a string is empty.
    ///
    /// # Errors
    /// The `OSStatus` of the read.
    pub fn factory_presets(&self) -> Result<Vec<(i32, String)>> {
        let array = match self.copy_array(FactoryPresets::ID, Scope::GLOBAL.raw(), 0) {
            Ok(a) => a,
            Err(Error::Invalid(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok((0..array.len())
            .filter_map(|i| {
                let ptr = array.value_at(i)? as *const sys::AUPreset;
                if ptr.is_null()
                    || !(ptr as usize).is_multiple_of(std::mem::align_of::<sys::AUPreset>())
                {
                    return None;
                }
                // SAFETY: the elements of the factory-preset array are bare
                // `AUPreset` structs (the header's contract), checked non-null
                // and aligned above, and borrowed from `array`, which lives
                // across the read.
                let preset = unsafe { ptr.read() };
                // SAFETY: `presetName` is null or a CFString inside the struct.
                let name = unsafe { cfstring_to_string_checked(preset.presetName) };
                Some((preset.presetNumber, name.unwrap_or_default()))
            })
            .collect())
    }

    /// The current preset as `(number, name)`.
    ///
    /// # Errors
    /// The `OSStatus` of the read.
    pub fn present_preset(&self) -> Result<(i32, String)> {
        let preset = self.get::<PresentPreset>(Scope::GLOBAL, Element::ZERO)?;
        // SAFETY: `presetName` is null or a CFString we own one retain of (the
        // Copy rule of the property).
        let name = unsafe { take_string(preset.presetName) }.unwrap_or_default();
        Ok((preset.presetNumber, name))
    }

    /// Selects the factory preset `number` (`kAudioUnitProperty_PresentPreset`).
    ///
    /// # Errors
    /// The `OSStatus` of the write.
    pub fn set_present_preset(&self, number: i32) -> Result<()> {
        let preset = sys::AUPreset {
            presetNumber: number,
            presetName: std::ptr::null(),
        };
        self.set::<PresentPreset>(Scope::GLOBAL, Element::ZERO, &preset)
    }

    /// Reads a property that is a copied property list (the unit's
    /// `ClassInfo`), as binary plist bytes; empty when the unit answers null.
    ///
    /// # Errors
    /// The `OSStatus` of the read, or a plist that does not serialize.
    fn copy_plist_raw(&self, id: u32, scope: u32, element: u32) -> Result<Vec<u8>> {
        let raw: core_foundation_sys::propertylist::CFPropertyListRef =
            self.get_cf(id, scope, element)?;
        // SAFETY: a plist-valued property is returned under the Copy rule.
        match unsafe { CfPlist::from_copied(raw) } {
            Some(plist) => plist.to_binary(),
            None => Ok(Vec::new()),
        }
    }

    /// Writes a property list (decoded from `bytes`) to the property.
    ///
    /// # Errors
    /// A plist that does not decode, or the `OSStatus` of the write.
    fn set_plist_raw(&self, id: u32, scope: u32, element: u32, bytes: &[u8]) -> Result<()> {
        let plist = CfPlist::from_binary(bytes)?;
        let raw = plist.as_raw();
        // The property takes the plist by reference and retains what it keeps;
        // `plist` outlives the call.
        self.set_raw(id, scope, element, &(raw as *const c_void))
    }

    /// Writes a `CFString` to a string-valued property.
    ///
    /// # Errors
    /// The `OSStatus` of the write.
    fn set_string_raw(&self, id: u32, scope: u32, element: u32, text: &str) -> Result<()> {
        let string = CfString::new(text);
        // The property copies the string before the call returns; `string`
        // outlives the call.
        self.set_raw(id, scope, element, &(string.as_raw() as *const c_void))
    }

    /// Reads a pointer-sized, copy-rule property value.
    fn get_cf<T: Copy>(&self, id: u32, scope: u32, element: u32) -> Result<T> {
        const { assert!(size_of::<T>() == size_of::<*const c_void>()) };
        let raw: *const c_void = self.get_raw(id, scope, element)?;
        // SAFETY: `T` is one of the `CF*Ref` pointer typedefs, the same size as
        // `*const c_void` (asserted at compile time above), so the bits are the same pointer.
        Ok(unsafe { std::mem::transmute_copy::<*const c_void, T>(&raw) })
    }

    // ---- parameters ----

    /// `AudioUnitGetParameter`.
    ///
    /// # Errors
    /// The `OSStatus` of the call.
    pub fn parameter(&self, id: u32, scope: Scope, element: Element) -> Result<f32> {
        let mut value: f32 = 0.0;
        // SAFETY: `value` is writable.
        let status = unsafe {
            sys::AudioUnitGetParameter(self.raw(), id, scope.raw(), element.0, &mut value)
        };
        check("AudioUnitGetParameter", status)?;
        Ok(value)
    }

    /// `AudioUnitSetParameter`.
    ///
    /// # Errors
    /// The `OSStatus` of the call.
    pub fn set_parameter(&self, id: u32, scope: Scope, element: Element, value: f32) -> Result<()> {
        // SAFETY: `raw()` is a live instance.
        let status =
            unsafe { sys::AudioUnitSetParameter(self.raw(), id, scope.raw(), element.0, value, 0) };
        check("AudioUnitSetParameter", status)?;
        Ok(())
    }

    // ---- MIDI ----

    /// `MusicDeviceMIDIEvent`: one short MIDI 1.0 message at `frame_offset`
    /// frames into the next render.
    ///
    /// # Errors
    /// The `OSStatus` of the call.
    pub fn midi_event(&self, status: u8, data1: u8, data2: u8, frame_offset: u32) -> Result<()> {
        // SAFETY: `raw()` is a live instance; the call takes values only.
        let result = unsafe {
            sys::MusicDeviceMIDIEvent(
                self.raw(),
                u32::from(status),
                u32::from(data1),
                u32::from(data2),
                frame_offset,
            )
        };
        check("MusicDeviceMIDIEvent", result)?;
        Ok(())
    }

    /// `MusicDeviceMIDIEventList` (macOS 12): every packet of `list`, each at
    /// its own timestamp as sample frames into the next render (the base
    /// offset is 0). Empty lists are not sent.
    ///
    /// # Errors
    /// [`Error::Unavailable`] on a macOS before 12, which lacks the function;
    /// otherwise the `OSStatus` of the call.
    pub fn midi_event_list(&self, list: &EventList) -> Result<()> {
        let send = send_event_list_fn().ok_or(Error::Unavailable("MusicDeviceMIDIEventList"))?;
        if list.is_empty() {
            return Ok(());
        }
        let words = list.as_words();
        // SAFETY: `words` is the list in CoreMIDI's own layout (`event_list`'s
        // docs, checked against `MIDIEventListAdd` by the host's tests), four
        // byte aligned as a `u32` slice, and live for the call, which reads it
        // and keeps no pointer.
        let status = unsafe { send(self.raw(), 0, words.as_ptr().cast::<sys::MIDIEventList>()) };
        check("MusicDeviceMIDIEventList", status)?;
        Ok(())
    }

    /// The protocol the unit takes MIDI in (`AudioUnitMIDIProtocol`): `None`
    /// when the unit does not say (the property is absent before macOS 12, and
    /// "not set" means legacy MIDI 1.0 through `MusicDeviceMIDIEvent`).
    pub fn midi_protocol(&self) -> Option<Wire> {
        match self.get::<AudioUnitMidiProtocol>(Scope::GLOBAL, Element::ZERO) {
            Ok(1) => Some(Wire::Midi1),
            Ok(2) => Some(Wire::Midi2),
            _ => None,
        }
    }

    /// `MusicDeviceSysEx`: one complete, framed system-exclusive message.
    ///
    /// # Errors
    /// The `OSStatus` of the call.
    pub fn sysex(&self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        // SAFETY: `bytes` is a live slice of the length passed.
        let status =
            unsafe { sys::MusicDeviceSysEx(self.raw(), bytes.as_ptr(), bytes.len() as u32) };
        check("MusicDeviceSysEx", status)?;
        Ok(())
    }
}

impl Unit {
    /// Reads the plain value of property `P`.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitGetProperty`.
    pub fn get<P: Property>(&self, scope: Scope, element: Element) -> Result<P::Value>
    where
        P::Value: Plain,
    {
        self.get_raw(P::ID, scope.raw(), element.0)
    }

    /// Reads a property whose request is itself a value (a string-from-value
    /// query): `request` goes in and the unit's answer comes back.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitGetProperty`.
    pub fn get_in_out<P: Property>(
        &self,
        scope: Scope,
        element: Element,
        request: P::Value,
    ) -> Result<P::Value>
    where
        P::Value: Plain,
    {
        self.get_in_out_raw(P::ID, scope.raw(), element.0, request)
    }

    /// Writes the plain value of property `P`.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitSetProperty`.
    pub fn set<P: Property>(&self, scope: Scope, element: Element, value: &P::Value) -> Result<()>
    where
        P::Value: Plain,
    {
        self.set_raw(P::ID, scope.raw(), element.0, value)
    }

    /// Writes only the first `len` bytes of `value`, for a variable-size record
    /// whose tail the property does not take; `len` is clamped to the size of the
    /// value.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitSetProperty`.
    pub fn set_prefix<P: Property>(
        &self,
        scope: Scope,
        element: Element,
        value: &P::Value,
        len: usize,
    ) -> Result<()>
    where
        P::Value: Plain,
    {
        self.set_prefix_raw(P::ID, scope.raw(), element.0, value, len)
    }

    /// Writes a property from raw bytes (a variable-size record).
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitSetProperty`.
    pub fn set_bytes<P: Property>(
        &self,
        scope: Scope,
        element: Element,
        bytes: &[u8],
    ) -> Result<()> {
        self.set_bytes_raw(P::ID, scope.raw(), element.0, bytes)
    }

    /// The size and writability of a property, without reading it.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioUnitGetPropertyInfo`.
    pub fn property_info<P: Property>(
        &self,
        scope: Scope,
        element: Element,
    ) -> Result<PropertyInfo> {
        self.property_info_raw(P::ID, scope.raw(), element.0)
    }

    /// Reads a variable-size property into an owned byte buffer.
    ///
    /// # Errors
    /// The `OSStatus` of either call.
    pub fn get_bytes<P: Property>(&self, scope: Scope, element: Element) -> Result<Vec<u8>> {
        self.get_bytes_raw(P::ID, scope.raw(), element.0)
    }

    /// Reads a variable-size property that is an array of `P::Value` records;
    /// trailing bytes short of a whole record are dropped.
    ///
    /// # Errors
    /// The `OSStatus` of either call.
    pub fn get_array<P: Property>(&self, scope: Scope, element: Element) -> Result<Vec<P::Value>>
    where
        P::Value: Plain,
    {
        self.get_array_raw(P::ID, scope.raw(), element.0)
    }

    /// Reads a property that is a copied `CFString`; `None` when the unit answers
    /// null or something that is not a string.
    ///
    /// # Errors
    /// The `OSStatus` of the read.
    pub fn copy_string<P: Property>(
        &self,
        scope: Scope,
        element: Element,
    ) -> Result<Option<String>> {
        self.copy_string_raw(P::ID, scope.raw(), element.0)
    }

    /// Reads a property that is a copied `CFURL`, as a filesystem path.
    ///
    /// # Errors
    /// The `OSStatus` of the read.
    pub fn copy_url_path<P: Property>(
        &self,
        scope: Scope,
        element: Element,
    ) -> Result<Option<String>> {
        self.copy_url_path_raw(P::ID, scope.raw(), element.0)
    }

    /// Reads a property that is a copied `CFArray` of `CFString`s; an entry is
    /// `None` when it is not a string.
    ///
    /// # Errors
    /// The `OSStatus` of the read.
    pub fn copy_string_array<P: Property>(
        &self,
        scope: Scope,
        element: Element,
    ) -> Result<Vec<Option<String>>> {
        self.copy_string_array_raw(P::ID, scope.raw(), element.0)
    }

    /// Reads a property that is a copied property list, as binary plist bytes;
    /// empty when the unit answers null.
    ///
    /// # Errors
    /// The `OSStatus` of the read, or a plist that does not serialize.
    pub fn copy_plist<P: Property>(&self, scope: Scope, element: Element) -> Result<Vec<u8>> {
        self.copy_plist_raw(P::ID, scope.raw(), element.0)
    }

    /// Writes a property list (decoded from `bytes`) to the property.
    ///
    /// # Errors
    /// A plist that does not decode, or the `OSStatus` of the write.
    pub fn set_plist<P: Property>(
        &self,
        scope: Scope,
        element: Element,
        bytes: &[u8],
    ) -> Result<()> {
        self.set_plist_raw(P::ID, scope.raw(), element.0, bytes)
    }

    /// Writes a `CFString` to a string-valued property.
    ///
    /// # Errors
    /// The `OSStatus` of the write.
    pub fn set_string<P: Property>(
        &self,
        scope: Scope,
        element: Element,
        text: &str,
    ) -> Result<()> {
        self.set_string_raw(P::ID, scope.raw(), element.0, text)
    }
}

/// The handle the render thread holds: it keeps the unit alive and is `Send`
/// and `Sync`, so a render loop can run on a thread of its own while the
/// control thread keeps its [`Unit`].
#[derive(Clone)]
pub struct RenderUnit(Arc<Inner>);

impl std::fmt::Debug for RenderUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderUnit").finish_non_exhaustive()
    }
}

impl RenderUnit {
    pub(crate) fn from_inner(inner: Arc<Inner>) -> Self {
        Self(inner)
    }

    /// The raw `AudioUnit`, valid while a `RenderUnit` or `Unit` lives, for a
    /// call this crate does not wrap (`AudioUnitRender` from a notify
    /// callback, `schedule`). Using it is the caller's `unsafe`, and a render
    /// made through it is not counted: hold [`rendering`](Self::rendering)
    /// across it, or keep it from overlapping
    /// [`AuMidiOutput::withdraw`](crate::AuMidiOutput::withdraw), which frees the
    /// MIDI-output callback's state once no counted render is in flight.
    pub fn get(&self) -> sys::AudioUnit {
        self.0.instance
    }

    /// Counts a render through [`get`](Self::get) in flight until the guard
    /// drops: [`Unit::rendering`], for the thread that holds this handle.
    pub fn rendering(&self) -> tutti_rt::RenderGuard<'_> {
        self.0.renders.enter()
    }
}
