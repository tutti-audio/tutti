//! Listing the ports winmm has.
//!
//! A port is a value that owns its list position privately; nothing outside
//! this crate sees the position or opens by it.

use std::marker::PhantomData;
use std::sync::OnceLock;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_Interface_PropertyW, CM_LOCATE_DEVNODE_NORMAL,
    CM_Locate_DevNodeW, CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows_sys::Win32::Devices::Properties::{
    DEVPKEY_Device_FriendlyName, DEVPKEY_Device_InstanceId, DEVPROP_TYPE_STRING,
};
use windows_sys::Win32::Media::Audio::{
    MIDIINCAPSW, MIDIOUTCAPSW, midiInGetDevCapsW, midiInGetNumDevs, midiInMessage,
    midiOutGetDevCapsW, midiOutGetNumDevs, midiOutMessage,
};
use windows_sys::Win32::Media::Multimedia::{
    DRV_QUERYDEVICEINTERFACE, DRV_QUERYDEVICEINTERFACESIZE,
};

use super::error::{Direction, Error, MmOperation, check};

/// What a driver reports of one port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caps {
    /// The manufacturer id (`wMid`).
    pub manufacturer: u16,
    /// The product id (`wPid`).
    pub product: u16,
    /// The product name (`szPname`), which winmm cuts to 31 characters.
    pub name: String,
}

/// What the driver reports of input position `index`.
///
/// # Errors
///
/// `BadDeviceId` when the position is past the list, or the driver's own refusal.
fn input_caps(index: u32) -> Result<Caps, Error> {
    let mut caps = MIDIINCAPSW::default();
    // SAFETY: `caps` is a live, writable `MIDIINCAPSW` and the size passed is
    // its size, so winmm writes within it.
    let code = unsafe {
        midiInGetDevCapsW(
            index as usize,
            &raw mut caps,
            size_of::<MIDIINCAPSW>() as u32,
        )
    };
    check(MmOperation::MidiInGetDevCaps, code)?;
    // The struct is packed: read each field by value, never by reference.
    let (manufacturer, product, name) = (caps.wMid, caps.wPid, caps.szPname);
    Ok(Caps {
        manufacturer,
        product,
        name: name_of(&name),
    })
}

/// What the driver reports of output position `index`.
///
/// # Errors
///
/// `BadDeviceId` when the position is past the list, or the driver's own refusal.
fn output_caps(index: u32) -> Result<Caps, Error> {
    let mut caps = MIDIOUTCAPSW::default();
    // SAFETY: `caps` is a live, writable `MIDIOUTCAPSW` and the size passed is
    // its size, so winmm writes within it.
    let code = unsafe {
        midiOutGetDevCapsW(
            index as usize,
            &raw mut caps,
            size_of::<MIDIOUTCAPSW>() as u32,
        )
    };
    check(MmOperation::MidiOutGetDevCaps, code)?;
    let (manufacturer, product, name) = (caps.wMid, caps.wPid, caps.szPname);
    Ok(Caps {
        manufacturer,
        product,
        name: name_of(&name),
    })
}

/// The text of a NUL-terminated UTF-16 field.
fn name_of(field: &[u16]) -> String {
    let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    String::from_utf16_lossy(&field[..end])
}

/// The longest interface path read, in bytes: a path past it is not one.
const MAX_INTERFACE_BYTES: u32 = 4096;

mod sealed {
    use super::Caps;
    use crate::Error;

    /// The winmm calls of one direction. Nameable only in this crate, so no
    /// other crate can call them by a position.
    pub trait Sealed {
        /// How many ports winmm lists now.
        fn count() -> u32;

        /// What the driver reports of position `index`.
        fn caps(index: u32) -> Result<Caps, Error>;

        /// The device interface path (a `\\?\usb#vid_...` string) of
        /// position `index`, which names the device across a replug where the
        /// position does not; `None` when the driver gives none.
        fn interface(index: u32) -> Option<String>;
    }
}

use sealed::Sealed;

/// Which way a [`Port`] goes: [`Input`] or [`Output`]. Sealed.
pub trait PortKind: Sealed {
    /// The direction this marks.
    const DIRECTION: Direction;
}

/// The marker of an input port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Input;

/// The marker of an output port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output;

impl PortKind for Input {
    const DIRECTION: Direction = Direction::Input;
}

impl PortKind for Output {
    const DIRECTION: Direction = Direction::Output;
}

impl Sealed for Input {
    fn count() -> u32 {
        // SAFETY: takes no arguments and touches no memory of ours.
        unsafe { midiInGetNumDevs() }
    }

    fn caps(index: u32) -> Result<Caps, Error> {
        input_caps(index)
    }

    fn interface(index: u32) -> Option<String> {
        interface(|message, dw1, dw2| {
            // SAFETY: the position stands for the device id in these two
            // driver messages, which winmm answers itself (a handle is
            // refused). `dw1` and `dw2` are the pointer and the byte count
            // that `interface` builds for `message`: a live `u32`, or a live
            // buffer of that many bytes, for the call.
            unsafe {
                midiInMessage(
                    std::ptr::without_provenance_mut(index as usize),
                    message,
                    dw1,
                    dw2,
                )
            }
        })
    }
}

impl Sealed for Output {
    fn count() -> u32 {
        // SAFETY: takes no arguments and touches no memory of ours.
        unsafe { midiOutGetNumDevs() }
    }

    fn caps(index: u32) -> Result<Caps, Error> {
        output_caps(index)
    }

    fn interface(index: u32) -> Option<String> {
        interface(|message, dw1, dw2| {
            // SAFETY: as for `Input`: `dw1` and `dw2` are the pointer and the
            // byte count `interface` builds for `message`.
            unsafe {
                midiOutMessage(
                    std::ptr::without_provenance_mut(index as usize),
                    message,
                    dw1,
                    dw2,
                )
            }
        })
    }
}

/// Asks the driver, through `send(message, dw1, dw2)`, for the path's size in
/// bytes (`DRV_QUERYDEVICEINTERFACESIZE` writes a `u32` at `dw1`) and then for
/// the path (`DRV_QUERYDEVICEINTERFACE` writes a NUL-terminated UTF-16 string
/// into the buffer at `dw1` of `dw2` bytes). The pointers passed are live
/// buffers of those sizes for the call.
fn interface(send: impl Fn(u32, usize, usize) -> u32) -> Option<String> {
    let mut bytes = 0u32;
    if send(DRV_QUERYDEVICEINTERFACESIZE, (&raw mut bytes) as usize, 0) != 0
        || bytes == 0
        || bytes > MAX_INTERFACE_BYTES
    {
        return None;
    }
    let mut path = vec![0u16; (bytes as usize).div_ceil(2)];
    let room = path.len() * 2;
    if send(DRV_QUERYDEVICEINTERFACE, path.as_mut_ptr() as usize, room) != 0 {
        return None;
    }
    let path = name_of(&path);
    (!path.is_empty()).then_some(path)
}

/// What names a port's device: the driver's report at one position.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Device {
    caps: Caps,
    interface: Option<String>,
}

impl Device {
    /// The driver's report at position `index` of `D`'s list; `None` when it
    /// will not describe it.
    fn read<D: PortKind>(index: u32) -> Option<Self> {
        Some(Self {
            caps: D::caps(index).ok()?,
            interface: D::interface(index),
        })
    }

    /// Whether `now`, read at the same position later, is this device. The
    /// interface paths agree, or, for a driver that gives none, the caps do.
    /// The interface path alone decides: two ports of one device that share a
    /// path are not told apart.
    fn is_same(&self, now: &Self) -> bool {
        match (&self.interface, &now.interface) {
            (Some(listed), Some(now)) => listed == now,
            (None, None) => self.caps == now.caps,
            _ => false,
        }
    }
}

/// `Ok` when `now`, the report at a listed port's position, is the `listed`
/// device; [`Error::PortMoved`] when it is another, or the driver reports
/// nothing there any more.
fn same_device(direction: Direction, listed: &Device, now: Option<&Device>) -> Result<(), Error> {
    if now.is_some_and(|now| listed.is_same(now)) {
        Ok(())
    } else {
        Err(Error::PortMoved { direction })
    }
}

/// The friendly name of the device behind interface path `path`
/// (`DEVPKEY_Device_FriendlyName`, which is not cut to 31 characters), or
/// `None` when the device has none.
fn friendly_name(path: &str) -> Option<String> {
    let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    // The interface names its device by instance id; the name is the device's.
    let instance = read_string(|buffer, size, kind| {
        // SAFETY: `path` is NUL-terminated; `buffer` is null or at least
        // `*size` writable bytes, and `size` and `kind` are live (see
        // `read_string`).
        unsafe {
            CM_Get_Device_Interface_PropertyW(
                path.as_ptr(),
                &DEVPKEY_Device_InstanceId,
                kind,
                buffer,
                size,
                0,
            )
        }
    })?;
    let instance: Vec<u16> = instance.encode_utf16().chain(Some(0)).collect();
    let mut node = 0u32;
    // SAFETY: `node` is a live out-pointer and `instance` is NUL-terminated.
    if unsafe { CM_Locate_DevNodeW(&raw mut node, instance.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) }
        != CR_SUCCESS
    {
        return None;
    }
    read_string(|buffer, size, kind| {
        // SAFETY: as above, for the device node found just now.
        unsafe {
            CM_Get_DevNode_PropertyW(node, &DEVPKEY_Device_FriendlyName, kind, buffer, size, 0)
        }
    })
    .filter(|name| !name.is_empty())
}

/// Reads a string property through `get(buffer, size, kind)`, a configuration
/// manager call: first with a null buffer for the size it needs
/// (`CR_BUFFER_SMALL`), then into a buffer of that size, which the second call
/// is given as at least `size` bytes. The pointers passed are live for each
/// call. The text is the property's `size` bytes up to a NUL; a size of 0 or
/// past [`MAX_INTERFACE_BYTES`], or a property that is not a string, is `None`.
fn read_string(get: impl Fn(*mut u8, *mut u32, *mut u32) -> u32) -> Option<String> {
    let (mut size, mut kind) = (0u32, 0u32);
    if get(std::ptr::null_mut(), &raw mut size, &raw mut kind) != CR_BUFFER_SMALL
        || size == 0
        || size > MAX_INTERFACE_BYTES
    {
        return None;
    }
    let mut text = vec![0u16; (size as usize).div_ceil(2)];
    if get(text.as_mut_ptr().cast(), &raw mut size, &raw mut kind) != CR_SUCCESS
        || kind != DEVPROP_TYPE_STRING
    {
        return None;
    }
    let units = (size as usize / 2).min(text.len());
    Some(name_of(&text[..units]))
}

/// A listed port of direction `D`: the only thing
/// [`MidiIn::open`](crate::MidiIn::open) or [`MidiOut::open`](crate::MidiOut::open)
/// takes.
#[derive(Clone, Debug)]
pub struct Port<D: PortKind> {
    /// The position in winmm's list when listed: opened by, and never shown.
    index: u32,
    device: Device,
    /// The friendly name, read on the first [`name`](Self::name).
    friendly: OnceLock<Option<String>>,
    side: PhantomData<D>,
}

/// Two ports are equal when they are the same position holding the same
/// device; the friendly name, which is only a cache, is not compared.
impl<D: PortKind> PartialEq for Port<D> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.device == other.device
    }
}

impl<D: PortKind> Eq for Port<D> {}

/// A listed input port.
pub type InputPort = Port<Input>;
/// A listed output port.
pub type OutputPort = Port<Output>;

/// The ports of direction `D` winmm lists now, in list order. A position the
/// driver will not describe (a device removed since the count) is left out.
#[must_use]
pub fn ports<D: PortKind>() -> Vec<Port<D>> {
    (0..D::count())
        .filter_map(|index| {
            let device = Device::read::<D>(index).or_else(|| {
                tracing::debug!("winmm enumeration: no description of position {index}");
                None
            })?;
            Some(Port {
                index,
                device,
                friendly: OnceLock::new(),
                side: PhantomData,
            })
        })
        .collect()
}

impl<D: PortKind> Port<D> {
    /// What the driver reported of the port.
    #[must_use]
    pub fn caps(&self) -> &Caps {
        &self.device.caps
    }

    /// The device interface path the driver gave, which names the device
    /// across a replug; `None` when it gave none.
    #[must_use]
    pub fn interface(&self) -> Option<&str> {
        self.device.interface.as_deref()
    }

    /// The name to show: the device's friendly name where it has one, else the
    /// driver's product name, which winmm cuts to 31 characters. The friendly
    /// name is read from the device node on the first call; the ports of a
    /// multi-port device may share one, so it does not tell ports apart and
    /// identity does not use it.
    #[must_use]
    pub fn name(&self) -> &str {
        self.friendly
            .get_or_init(|| self.device.interface.as_deref().and_then(friendly_name))
            .as_deref()
            .unwrap_or(&self.device.caps.name)
    }

    /// The position, for the open that takes this port.
    pub(crate) fn index(&self) -> u32 {
        self.index
    }

    /// Reads the port's position again and fails unless the device there is
    /// the one listed. An open calls this before it opens, and again once the
    /// port is open: winmm answers the interface query for a device id and
    /// refuses a handle, so the check is of the position, not of the open
    /// handle.
    ///
    /// # Errors
    ///
    /// [`Error::PortMoved`] when the device at the position is another, or the
    /// driver reports nothing there.
    pub(crate) fn verify(&self) -> Result<(), Error> {
        same_device(
            D::DIRECTION,
            &self.device,
            Device::read::<D>(self.index).as_ref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Devices::Properties::DEVPROP_TYPE_UINT32;

    fn device(interface: Option<&str>, name: &str) -> Device {
        Device {
            caps: Caps {
                manufacturer: 1,
                product: 2,
                name: name.to_string(),
            },
            interface: interface.map(str::to_string),
        }
    }

    #[test]
    fn a_position_that_now_holds_another_interface_is_moved() {
        let listed = device(Some(r"\\?\usb#a"), "Keys");
        let same = device(Some(r"\\?\usb#a"), "Keys");
        assert!(same_device(Direction::Input, &listed, Some(&same)).is_ok());
        for now in [device(Some(r"\\?\usb#b"), "Keys"), device(None, "Keys")] {
            assert!(matches!(
                same_device(Direction::Input, &listed, Some(&now)),
                Err(Error::PortMoved {
                    direction: Direction::Input
                })
            ));
        }
    }

    #[test]
    fn without_an_interface_the_caps_decide() {
        let listed = device(None, "Keys");
        assert!(same_device(Direction::Output, &listed, Some(&device(None, "Keys"))).is_ok());
        assert!(same_device(Direction::Output, &listed, Some(&device(None, "Pads"))).is_err());
        assert!(same_device(Direction::Output, &listed, Some(&device(Some("x"), "Keys"))).is_err());
    }

    #[test]
    fn a_position_that_vanished_is_moved_not_a_bad_device_id() {
        let listed = device(Some(r"\\?\usb#a"), "Keys");
        assert!(matches!(
            same_device(Direction::Output, &listed, None),
            Err(Error::PortMoved {
                direction: Direction::Output
            })
        ));
    }

    /// A configuration manager call that answers `first` to the size query and
    /// then writes `text` into the buffer; `size` and `kind` are what it
    /// reports.
    fn property(
        first: u32,
        size: u32,
        kind: u32,
        text: &[u16],
    ) -> impl Fn(*mut u8, *mut u32, *mut u32) -> u32 {
        let text = text.to_vec();
        move |buffer, out_size, out_kind| {
            // SAFETY: `read_string` passes live pointers to a `u32` each.
            unsafe {
                *out_size = size;
                *out_kind = kind;
            }
            if buffer.is_null() {
                return first;
            }
            let room = (size as usize).div_ceil(2);
            // SAFETY: `read_string` passes a buffer of `room` units
            // (`size.div_ceil(2)`), and no more than that is copied.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    text.as_ptr(),
                    buffer.cast::<u16>(),
                    text.len().min(room),
                );
            }
            CR_SUCCESS
        }
    }

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    const NO_SUCH_VALUE: u32 = 37;

    #[test]
    fn a_string_property_reads_up_to_its_nul() {
        let text = units("Keys\0junk");
        let get = property(CR_BUFFER_SMALL, 18, DEVPROP_TYPE_STRING, &text);
        assert_eq!(read_string(get).as_deref(), Some("Keys"));
    }

    #[test]
    fn an_unterminated_property_reads_to_its_size_and_an_odd_size_drops_the_half_unit() {
        let text = units("Keys");
        let get = property(CR_BUFFER_SMALL, 8, DEVPROP_TYPE_STRING, &text);
        assert_eq!(read_string(get).as_deref(), Some("Keys"));
        // Five bytes hold two whole units; the third is not part of the value.
        let abc = units("abc");
        let get = property(CR_BUFFER_SMALL, 5, DEVPROP_TYPE_STRING, &abc);
        assert_eq!(read_string(get).as_deref(), Some("ab"));
    }

    #[test]
    fn a_property_that_is_missing_empty_too_large_or_not_a_string_is_none() {
        let text = units("Keys\0");
        assert_eq!(
            read_string(property(NO_SUCH_VALUE, 12, DEVPROP_TYPE_STRING, &text)),
            None
        );
        assert_eq!(
            read_string(property(CR_BUFFER_SMALL, 0, DEVPROP_TYPE_STRING, &text)),
            None
        );
        assert_eq!(
            read_string(property(
                CR_BUFFER_SMALL,
                MAX_INTERFACE_BYTES + 1,
                DEVPROP_TYPE_STRING,
                &text
            )),
            None
        );
        let number = property(CR_BUFFER_SMALL, 12, DEVPROP_TYPE_UINT32, &text);
        assert_eq!(read_string(number), None);
    }

    /// A driver that answers the size message with `size` (or fails it with
    /// `size_code`) and the path message with `path`.
    fn driver(
        size_code: u32,
        size: u32,
        path_code: u32,
        path: &[u16],
    ) -> impl Fn(u32, usize, usize) -> u32 {
        let path = path.to_vec();
        move |message, dw1, dw2| {
            if message == DRV_QUERYDEVICEINTERFACESIZE {
                // SAFETY: `interface` passes a live `u32` at `dw1`.
                unsafe { *(dw1 as *mut u32) = size };
                return size_code;
            }
            let room = dw2 / 2;
            // SAFETY: `interface` passes a live buffer of `dw2` bytes at `dw1`.
            unsafe {
                std::ptr::copy_nonoverlapping(path.as_ptr(), dw1 as *mut u16, path.len().min(room));
            }
            path_code
        }
    }

    #[test]
    fn an_interface_path_is_what_the_driver_wrote_up_to_its_nul() {
        let path = units("\\\\?\\usb#a\0");
        assert_eq!(
            interface(driver(0, 22, 0, &path)).as_deref(),
            Some(r"\\?\usb#a")
        );
    }

    #[test]
    fn an_interface_the_driver_refuses_sizes_wrongly_or_leaves_empty_is_none() {
        let path = units("\\\\?\\usb#a\0");
        assert_eq!(interface(driver(1, 22, 0, &path)), None);
        assert_eq!(interface(driver(0, 0, 0, &path)), None);
        assert_eq!(
            interface(driver(0, MAX_INTERFACE_BYTES + 1, 0, &path)),
            None
        );
        assert_eq!(interface(driver(0, 22, 1, &path)), None);
        assert_eq!(interface(driver(0, 2, 0, &units("\0"))), None);
    }
}
