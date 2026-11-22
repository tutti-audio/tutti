//! Windows: a helper starts suspended with only its three standard handles
//! inherited, and a leader is in the host's kill-on-close job before its first
//! instruction.

#[path = "windows/command_line.rs"]
mod command_line;
#[path = "windows/environment.rs"]
mod environment;
#[path = "windows/standard.rs"]
mod standard;

use super::{Spawn, SpawnError};
use crate::lifetime::platform::bind_to_host;
use crate::{Process, Reaped};
use command_line::{application_name, command_line, wide};
use standard::{Standard, Streams};
use std::marker::PhantomData;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::process::Command;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, UpdateProcThreadAttribute,
};

/// A process attribute list holding one handle list, deleted when it drops.
///
/// The OS keeps a pointer into the handles, not a copy, so the list borrows them.
struct HandleList<'a> {
    /// The list's storage, aligned for the structure the OS lays out in it.
    storage: Vec<usize>,
    handles: PhantomData<&'a [HANDLE; 3]>,
}

impl<'a> HandleList<'a> {
    fn new(handles: &'a [HANDLE; 3]) -> std::io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: a null list asks only for the size the list needs, written to
        // the live `bytes`. The call reports failure for that, which is expected.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        // SAFETY: `storage` is at least `bytes` long and aligned for a pointer,
        // and `bytes` is a live in/out-parameter.
        let initialised = unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut bytes)
        };
        if initialised == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // From here the drop deletes the list.
        let mut list = Self {
            storage,
            handles: PhantomData,
        };
        // SAFETY: the list is initialised, and `handles` outlives it (`'a`), as
        // the attribute keeps a pointer to the array and not a copy.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                size_of::<[HANDLE; 3]>(),
                null_mut(),
                null(),
            )
        };
        if updated == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(list)
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}

impl Drop for HandleList<'_> {
    fn drop(&mut self) {
        // SAFETY: the list was initialised by `new` and is deleted once.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// Starts `command` as `how` says and holds the process.
///
/// The child inherits its three standard handles and no other handle of the
/// host's: they are the host's own for a [`Spawn::Leader`] and the `NUL` device
/// for a [`Spawn::Helper`] (the command's own `stdin`, `stdout` and `stderr`
/// are not read). It starts suspended, a leader is assigned to the host's job
/// while it is, and only then does it run.
///
/// The command's program is never searched for, so it names a path, and its
/// arguments, its variables set or removed and its current directory are
/// read through std's getters. A command that cleared its environment is
/// not honoured, and the hidden `=C:` per-drive current-directory variables are
/// not passed (`vars_os` skips them).
///
/// # Errors
///
/// [`SpawnError::Start`] when the program, an argument or a variable cannot be
/// passed or the OS refuses the spawn, [`SpawnError::NotBound`] when a leader
/// cannot be assigned to the job (it is killed and reaped).
pub fn spawn(command: Command, how: Spawn) -> Result<Reaped, SpawnError> {
    let streams = match how {
        Spawn::Leader => Streams::Inherit,
        Spawn::Helper => Streams::Null,
    };
    let (process, thread) = create(&command, streams).map_err(SpawnError::Start)?;
    // On any refusal below `reaped` drops: killed and reaped, never run.
    let reaped = Reaped::of(process, how == Spawn::Leader);
    if how == Spawn::Leader {
        bind_to_host(reaped.process()).map_err(SpawnError::NotBound)?;
    }
    resume(&thread).map_err(SpawnError::Start)?;
    Ok(reaped)
}

/// Creates the suspended process of `command`, and its main thread.
fn create(command: &Command, streams: Streams) -> std::io::Result<(Process, OwnedHandle)> {
    let application = application_name(command.get_program())?;
    let mut line = command_line(command.get_program(), command.get_args())?;
    let environment = environment::block(command)?;
    let directory = command
        .get_current_dir()
        .map(|path| {
            let mut path = wide(path.as_os_str())?;
            path.push(0);
            std::io::Result::Ok(path)
        })
        .transpose()?;

    let standard = Standard::new(streams)?;
    let handles = standard.raw();
    let mut attributes = HandleList::new(&handles)?;

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    [
        startup.StartupInfo.hStdInput,
        startup.StartupInfo.hStdOutput,
        startup.StartupInfo.hStdError,
    ] = handles;
    startup.lpAttributeList = attributes.as_ptr();
    // SAFETY: a plain C struct of integers and handles, for which all-zero is a
    // valid value.
    let mut created: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // SAFETY: the application name, the command line (writable) and the
    // environment (`CREATE_UNICODE_ENVIRONMENT`) are NUL-terminated UTF-16 that
    // live through the call; the directory is null or the same; `startup` is a
    // live `STARTUPINFOEXW` whose list holds `handles`, which are open and
    // inheritable, and `created` is a live out-parameter.
    let ok = unsafe {
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            environment.as_ptr().cast(),
            directory.as_ref().map_or(null(), |path| path.as_ptr()),
            std::ptr::addr_of!(startup).cast(),
            &mut created,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both handles are open, non-null and owned by no one else.
    let (process, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(created.hProcess),
            OwnedHandle::from_raw_handle(created.hThread),
        )
    };
    // The parent's copies of the standard handles close here, the attribute
    // list first (it points at them).
    drop(attributes);
    drop(standard);
    Ok((Process::new(process, created.dwProcessId), thread))
}

/// Lets the suspended process's main thread run.
fn resume(thread: &OwnedHandle) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: `thread` is an open thread handle, borrowed for the call.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
#[path = "windows/tests.rs"]
mod tests;
