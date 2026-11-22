//! Windows: every server is assigned to one Job Object, set to end its
//! processes when its last handle closes. The host holds that handle for as
//! long as it runs, so the kernel closes it when the host dies, by any means.

use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

/// The host's job, as the handle's integer (a raw handle is not `Send`), made
/// once and never closed: its close is the host's end.
static JOB: OnceLock<Result<usize, i32>> = OnceLock::new();

/// A server's helpers end with the job, not by a group's signal.
pub fn kill_group(_leader: u32) {}

/// Never known without reaping on Windows; a group is the job's, not a signal's.
#[must_use]
pub fn has_ended(_leader: u32) -> bool {
    false
}

fn make_job() -> std::io::Result<HANDLE> {
    // SAFETY: null attributes and name make an unnamed job with default
    // security; the handle is kept for the process's life.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a plain C struct of integers and pointers, for which all-zero is
    // a valid value.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `job` is the handle made above, and `info` is a live,
    // correctly sized `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` for the call.
    let set = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(job)
}

/// Assigns `child` to the host's kill-on-close Job Object, made on the first
/// call: the kernel ends the child when the host ends. A child that spawned
/// processes before this call leaves those outside the job.
///
/// # Errors
///
/// The job cannot be made or the child cannot be assigned (the OS error).
pub(crate) fn bind_to_host(child: &Child) -> std::io::Result<()> {
    let job = JOB.get_or_init(|| {
        make_job()
            .map(|job| job as usize)
            .map_err(|e| e.raw_os_error().unwrap_or(-1))
    });
    let job = match job {
        Ok(job) => *job as HANDLE,
        Err(code) => return Err(std::io::Error::from_raw_os_error(*code)),
    };
    // SAFETY: `job` is the live job handle held for the process's life, and
    // the child's handle is borrowed from `child` for the call.
    if unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use windows_sys::Win32::System::JobObjects::{IsProcessInJob, QueryInformationJobObject};

    /// **A bound child is in the host's job**: the kernel then ends it with the
    /// host. The job's kill-on-close limit is read back from the OS.
    #[test]
    fn a_bound_child_is_in_the_host_job_that_kills_on_close() {
        let mut command = Command::new("cmd");
        command.args(["/C", "ping -n 30 127.0.0.1 >NUL"]);
        let mut child = command.spawn().expect("cmd spawns");
        bind_to_host(&child).expect("the child is assigned");

        let job = JOB
            .get()
            .and_then(|job| job.as_ref().ok())
            .map(|job| *job as HANDLE)
            .expect("the job exists");
        let mut inside = 0;
        // SAFETY: the child's handle is borrowed from `child`, `job` is live,
        // and `inside` is a live out-parameter.
        let asked = unsafe { IsProcessInJob(child.as_raw_handle(), job, &mut inside) };
        assert!(asked != 0 && inside != 0, "the child is not in the job");

        // SAFETY: a plain C struct, for which all-zero is a valid value.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `job` is live, and `info` is a live, correctly sized
        // out-buffer of the class asked for; the returned length is not wanted.
        let queried = unsafe {
            QueryInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of_mut!(info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        assert!(queried != 0, "the job's limits cannot be read");
        assert!(
            info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0,
            "the job does not end its processes when it closes"
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}
