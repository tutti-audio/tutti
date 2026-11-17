//! Listing the ports winmm has.
//!
//! A port is a value that owns its list position privately; nothing outside
//! this crate sees the position or opens by it.

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

use crate::error::{Direction, Error, MmOperation, check};

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

/// The device interface path of input position `index` (a `\\?\usb#vid_...`
/// string), which names the device across a replug where the list position
/// does not. `None` when the driver gives none.
fn input_interface(index: u32) -> Option<String> {
    interface(|message, dw1, dw2| {
        // SAFETY: the position stands for the device id in these two driver
        // messages, which winmm answers itself (a handle is refused);
        // `dw1` and `dw2` are what `interface` says they are.
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

/// The device interface path of output position `index`; see [`input_interface`].
fn output_interface(index: u32) -> Option<String> {
    interface(|message, dw1, dw2| {
        // SAFETY: as `input_interface`.
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
    /// Whether `now`, read at the same position later, is this device: the
    /// interface paths agree, or, for a driver that gives none, the caps do.
    fn is_same(&self, now: &Self) -> bool {
        match (&self.interface, &now.interface) {
            (Some(listed), Some(now)) => listed == now,
            (None, None) => self.caps == now.caps,
            _ => false,
        }
    }
}

/// The friendly name of the device behind interface path `path`
/// (`DEVPKEY_Device_FriendlyName`, which is not cut to 31 characters), or
/// `None` when the device has none.
fn friendly_name(path: &str) -> Option<String> {
    let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    // The interface names its device by instance id; the name is the device's.
    let instance = read_string(|buffer, size, kind| {
        // SAFETY: `path` is NUL-terminated; `buffer` is null or `*size` live
        // writable bytes, and `size` and `kind` are live (see `read_string`).
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
            CM_Get_DevNode_PropertyW(
                node,
                &DEVPKEY_Device_FriendlyName,
                kind,
                buffer,
                size,
                0,
            )
        }
    })
    .filter(|name| !name.is_empty())
}

/// Reads a string property through `get(buffer, size, kind)`, a configuration
/// manager call: first with a null buffer for the size it needs
/// (`CR_BUFFER_SMALL`), then into a buffer of that size. The pointers passed
/// are live for each call, and `buffer` is null or `*size` writable bytes.
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
    Some(name_of(&text))
}

macro_rules! port {
    ($(#[$doc:meta])* $port:ident, $list:ident, $caps:ident, $interface:ident, $direction:expr, $count:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $port {
            /// The position in winmm's list when listed: opened by, and never
            /// shown.
            index: u32,
            device: Device,
            friendly: Option<String>,
        }

        /// The ports winmm lists now, in list order. A position the driver
        /// will not describe (a device removed since the count) is left out.
        #[must_use]
        pub fn $list() -> Vec<$port> {
            // SAFETY: takes no arguments and touches no memory of ours.
            let count = unsafe { $count() };
            (0..count)
                .filter_map(|index| {
                    let caps = $caps(index)
                        .inspect_err(|e| tracing::debug!("winmm enumeration: {e}"))
                        .ok()?;
                    let interface = $interface(index);
                    let friendly = interface.as_deref().and_then(friendly_name);
                    Some($port {
                        index,
                        device: Device { caps, interface },
                        friendly,
                    })
                })
                .collect()
        }

        impl $port {
            /// What the driver reported of the port.
            #[must_use]
            pub fn caps(&self) -> &Caps {
                &self.device.caps
            }

            /// The device interface path the driver gave, which names the
            /// device across a replug; `None` when it gave none.
            #[must_use]
            pub fn interface(&self) -> Option<&str> {
                self.device.interface.as_deref()
            }

            /// The name to show: the device's friendly name where it has one,
            /// else the driver's product name, which winmm cuts to 31
            /// characters.
            #[must_use]
            pub fn name(&self) -> &str {
                self.friendly.as_deref().unwrap_or(&self.device.caps.name)
            }

            /// The position, for the open that takes this port.
            pub(crate) fn index(&self) -> u32 {
                self.index
            }

            /// Reads the port's position again and fails unless the device
            /// there is the one listed. An open calls this once the port is
            /// open: winmm answers the interface query for a device id and
            /// refuses a handle, so the check is of the position, read after
            /// the open, not of the open handle.
            pub(crate) fn confirm(&self) -> Result<(), Error> {
                let now = Device {
                    caps: $caps(self.index)?,
                    interface: $interface(self.index),
                };
                if self.device.is_same(&now) {
                    Ok(())
                } else {
                    Err(Error::PortMoved {
                        direction: $direction,
                    })
                }
            }
        }
    };
}

port!(
    /// A listed input port: the only thing [`MidiIn::open`](crate::MidiIn::open) takes.
    InputPort, input_ports, input_caps, input_interface, Direction::Input, midiInGetNumDevs
);
port!(
    /// A listed output port: the only thing [`MidiOut::open`](crate::MidiOut::open) takes.
    OutputPort, output_ports, output_caps, output_interface, Direction::Output, midiOutGetNumDevs
);

#[cfg(test)]
mod tests {
    use super::{Caps, Device};

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
    fn a_position_that_now_holds_another_interface_is_not_the_listed_device() {
        let listed = device(Some(r"\\?\usb#a"), "Keys");
        assert!(listed.is_same(&device(Some(r"\\?\usb#a"), "Keys")));
        assert!(!listed.is_same(&device(Some(r"\\?\usb#b"), "Keys")));
        assert!(!listed.is_same(&device(None, "Keys")));
    }

    #[test]
    fn without_an_interface_the_caps_decide() {
        let listed = device(None, "Keys");
        assert!(listed.is_same(&device(None, "Keys")));
        assert!(!listed.is_same(&device(None, "Pads")));
        assert!(!listed.is_same(&device(Some(r"\\?\usb#a"), "Keys")));
    }
}
