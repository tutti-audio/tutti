//! The three standard handles a spawned helper inherits, each a handle of its
//! own made for the spawn.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// What a helper's standard handles are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Streams {
    /// The host's own standard handles.
    Inherit,
    /// The `NUL` device.
    Null,
}

/// The inheritable handles of a child's stdin, stdout and stderr, closed when
/// this drops: the parent's copies, which the child's own are made from.
///
/// Each is a handle of its own (a duplicate of the host's, or a fresh open of
/// `NUL`), so the three are distinct values and a handle list of them holds no
/// duplicate, and none is the host's own, which stays as it was.
///
/// While the duplicates exist a concurrent std `Command::spawn` elsewhere in the
/// host (`bInheritHandles` TRUE, no handle list) can inherit them, and a held
/// pipe duplicate can delay the end of file for a reader of the host's stdout.
/// std's spawn lock is private, so it is not held here.
#[derive(Debug)]
pub(super) struct Standard {
    handles: [OwnedHandle; 3],
}

impl Standard {
    /// # Errors
    ///
    /// The OS error of opening `NUL`.
    pub(super) fn new(streams: Streams) -> std::io::Result<Self> {
        let of = |id: STD_HANDLE| match streams {
            Streams::Inherit => duplicate(id).map_or_else(null_device, Ok),
            Streams::Null => null_device(),
        };
        Ok(Self {
            handles: [
                of(STD_INPUT_HANDLE)?,
                of(STD_OUTPUT_HANDLE)?,
                of(STD_ERROR_HANDLE)?,
            ],
        })
    }

    /// The raw handles in stdin, stdout, stderr order, borrowed from `self`.
    pub(super) fn raw(&self) -> [HANDLE; 3] {
        [
            self.handles[0].as_raw_handle(),
            self.handles[1].as_raw_handle(),
            self.handles[2].as_raw_handle(),
        ]
    }
}

/// An inheritable duplicate of the host's standard handle `id`; `None` when the
/// host has none (a GUI process) or it cannot be duplicated.
fn duplicate(id: STD_HANDLE) -> Option<OwnedHandle> {
    // SAFETY: `GetStdHandle` takes a scalar and returns a handle the host owns.
    let host = unsafe { GetStdHandle(id) };
    if host.is_null() || host == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut copy: HANDLE = null_mut();
    // SAFETY: the source is this process's own handle, borrowed for the call,
    // and `copy` is a live out-parameter.
    let duplicated = unsafe {
        let me = GetCurrentProcess();
        DuplicateHandle(me, host, me, &mut copy, 0, 1, DUPLICATE_SAME_ACCESS)
    };
    // SAFETY: a successful duplicate is open, non-null and owned by no one else.
    (duplicated != 0).then(|| unsafe { OwnedHandle::from_raw_handle(copy) })
}

/// An inheritable handle to the `NUL` device.
fn null_device() -> std::io::Result<OwnedHandle> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let name: Vec<u16> = "NUL\0".encode_utf16().collect();
    // SAFETY: `name` is NUL-terminated and `attributes` is a live, correctly
    // sized struct for the call.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the open succeeded: the handle is open and owned by no one else.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}
