//! The unix backend of the main loop: `poll(2)` on the control socket, the
//! plugin's descriptors and the bell. An X11 editor runs its own display
//! connection, so there is no window system to pump; the native loop for the
//! platform that needs one (macOS: `CFRunLoop` with the socket as a source)
//! replaces the wait, without touching the loop's logic.

use tutti_plugin_ipc_ffi::{Interest, LaneWire, Ready, SlabError, Watch};
use tutti_plugin_types::{IpcResult, Wakeups};

#[path = "unix_bell.rs"]
mod bell;

pub(crate) use bell::Bell;

/// The platform's concrete stream, not the crate's dispatch enum.
///
/// Unix needs the concrete stream because only it implements `AsFd`, which the
/// wait polls.
pub(crate) type Stream = interprocess::os::unix::uds_local_socket::Stream;

/// The server's end of the connection the host made: the stream socket the host
/// handed this process as its standard input.
pub type HostEnd = std::os::fd::OwnedFd;

/// Takes the host's end of the connection from this process's standard input
/// ([`tutti_plugin_ipc_ffi::adopt_host_socket`]). Call it first thing in `main`,
/// before any thread starts.
///
/// # Errors
///
/// Standard input is not a stream socket, which is the case for a server started
/// by hand rather than by its host.
pub fn adopt_host() -> IpcResult<HostEnd> {
    tutti_plugin_ipc_ffi::adopt_host_socket()
        .map_err(|e| tutti_plugin_types::IpcError::Io(std::io::Error::other(e)))
}

/// The host's connection as a stream, in blocking mode.
///
/// # Errors
///
/// Never on unix.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn meet(host: HostEnd) -> IpcResult<Stream> {
    Ok(Stream::from(host))
}

/// What a slab is received over on `stream`: the socket's descriptor.
pub(crate) fn connection(stream: &Stream) -> tutti_plugin_ipc_ffi::Connection<'_> {
    use std::os::fd::AsFd;
    stream.as_fd()
}

/// The server's end of a lane the host opened: the stream, which arrived as a
/// descriptor.
#[derive(Debug)]
pub(crate) struct LaneEnd(Stream);

impl LaneEnd {
    /// The lane's stream, once the host has been told the lane is open.
    ///
    /// # Errors
    ///
    /// Never on unix.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn accept(self) -> IpcResult<Stream> {
        Ok(self.0)
    }
}

/// The lane `wire` announced, received over `stream` right after its frame.
///
/// # Errors
///
/// The descriptor did not arrive as the frame announced.
pub(crate) fn receive_lane(stream: &Stream, wire: &LaneWire) -> Result<LaneEnd, SlabError> {
    let end = tutti_plugin_ipc_ffi::receive_lane(connection(stream), wire)?;
    Ok(LaneEnd(Stream::from(end)))
}

/// The reactor: what the loop waits with. It keeps the scratch of the wait, so a
/// steady watch set allocates nothing.
#[derive(Debug, Default)]
pub(crate) struct Native {
    asked: Vec<Watch>,
    got: Vec<Ready>,
}

impl Native {
    /// The one `poll(2)` over `stream`, the bell and the plugin's descriptors:
    /// whichever is ready first ends the wait the instant it is.
    pub(super) fn wait_on(
        &mut self,
        stream: &Stream,
        wakeups: &Wakeups,
        timeout: std::time::Duration,
        bell: &Bell,
        ready: &mut Vec<Ready>,
    ) -> IpcResult<bool> {
        use std::os::fd::AsFd;

        self.asked.clear();
        self.asked.extend_from_slice(wakeups.watches());
        self.asked.push(Watch {
            fd: bell.fd(),
            interest: Interest::READ,
        });
        let readable = loop {
            match tutti_plugin_ipc_ffi::poll_wait(
                Some(stream.as_fd()),
                &self.asked,
                Some(timeout),
                &mut self.got,
            ) {
                // A signal during the wait is not a failure; wait again.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        if self.got.iter().any(|r| r.fd == bell.fd()) {
            bell.drain();
        }
        ready.extend(self.got.iter().filter(|r| r.fd != bell.fd()).copied());
        Ok(readable)
    }
}

/// Waits up to `timeout` (`None`: for ever) for `stream` to have something to
/// read: `Ok(true)` when a read now finds a frame begun (or the host gone, which
/// the read reports as the error it is), `Ok(false)` on a timeout or when
/// `wake` rang first. It returns **the instant** either happens.
///
/// # Errors
///
/// The OS refuses the wait.
pub(crate) fn wait_readable(
    stream: &Stream,
    timeout: Option<std::time::Duration>,
    wake: &Bell,
) -> IpcResult<bool> {
    use std::os::fd::AsFd;

    loop {
        match tutti_plugin_ipc_ffi::wait_readable_either(stream.as_fd(), wake.as_fd(), timeout) {
            // A signal during the wait is not a failure; wait again.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
            Ok(readable) => {
                if readable.second {
                    wake.drain();
                }
                return Ok(readable.first);
            }
        }
    }
}
