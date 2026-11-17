//! Walking the sequencer's clients and ports.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::Arc;

use crate::address::{Address, PortCaps};
use crate::client::Client;
use crate::error::{Error, Operation, check};
use crate::records::{ClientRecord, PortRecord};
use crate::sys;
use crate::ump::UmpEndpoint;

/// `-ENOENT`: no client or port after this one, the end of a query.
const ENOENT: i32 = -2;

/// One port of another client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortEntry {
    /// The port's address.
    pub address: Address,
    /// The owning client's name, or `Client <id>` when it reports none.
    pub client_name: String,
    /// The port's name, or `Port <n>` when it reports none.
    pub name: String,
    /// What the port allows.
    pub caps: PortCaps,
    /// The client's UMP Endpoint, when the sequencer has one: a genuine UMP
    /// endpoint rather than a legacy client reached through the kernel's
    /// converter. The same record, shared, for every port of a client.
    pub ump: Option<Arc<UmpEndpoint>>,
}

/// What [`Client::list_ports`] found.
#[derive(Debug)]
pub struct Ports {
    /// The ports found, in the sequencer's order.
    pub ports: Vec<PortEntry>,
    /// Why the walk is short, when it is: the first query that failed with
    /// anything but `-ENOENT`, the end of a list (a client that exits mid-walk
    /// answers `-ENXIO`), or an Endpoint query or a record that failed. A short
    /// walk is not a look.
    pub incomplete: Option<Error>,
}

/// A C string alsa-lib owns, copied. `None` for NULL: "the library reported no
/// name" and "the name is empty" are different, and the caller substitutes its
/// own label for the former.
fn owned(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: non-NULL, and alsa-lib's info getters return NUL-terminated
    // strings valid until the owning info record is freed or changed; copied
    // at once, so nothing borrows past that point.
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

impl Client {
    /// Every port of every client but this one and the kernel's own (Timer,
    /// Announce), which are not devices.
    ///
    /// Offering this client's own port back as a device invites a feedback
    /// loop, and it is never what a caller meant.
    #[must_use]
    pub fn list_ports(&self) -> Ports {
        let mut out = Ports {
            ports: Vec::new(),
            incomplete: None,
        };
        let (cinfo, pinfo) = match (ClientRecord::new(), PortRecord::new()) {
            (Ok(cinfo), Ok(pinfo)) => (cinfo, pinfo),
            (Err(error), _) | (_, Err(error)) => {
                out.incomplete = Some(error);
                return out;
            }
        };
        let seq = self.raw();

        // SAFETY: `cinfo` is a live record; `query_next_client` walks from the
        // id set here, and -1 starts at the first.
        unsafe { sys::snd_seq_client_info_set_client(cinfo.as_ptr(), -1) };
        loop {
            // SAFETY: `seq` is open and `cinfo` is a live record.
            let rc = unsafe { sys::snd_seq_query_next_client(seq, cinfo.as_ptr()) };
            if rc < 0 {
                out.end_of(rc, Operation::QueryNextClient);
                break;
            }
            // SAFETY: `cinfo` is a live record the query just filled.
            let client = unsafe { sys::snd_seq_client_info_get_client(cinfo.as_ptr()) };
            if client == i32::from(self.id.get()) || client == sys::SND_SEQ_CLIENT_SYSTEM {
                continue;
            }
            // SAFETY: as above; the name is copied before the record changes.
            let client_name = owned(unsafe { sys::snd_seq_client_info_get_name(cinfo.as_ptr()) })
                .unwrap_or_else(|| format!("Client {client}"));

            // SAFETY: `pinfo` is a live record.
            unsafe {
                sys::snd_seq_port_info_set_client(pinfo.as_ptr(), client);
                sys::snd_seq_port_info_set_port(pinfo.as_ptr(), -1);
            }
            let mut ump: Option<Option<Arc<UmpEndpoint>>> = None;
            loop {
                // SAFETY: `seq` is open and `pinfo` is a live record.
                let rc = unsafe { sys::snd_seq_query_next_port(seq, pinfo.as_ptr()) };
                if rc < 0 {
                    out.end_of(rc, Operation::QueryNextPort);
                    break;
                }
                // SAFETY: `pinfo` is a live record the query just filled.
                let (port, caps) = unsafe {
                    (
                        sys::snd_seq_port_info_get_port(pinfo.as_ptr()),
                        sys::snd_seq_port_info_get_capability(pinfo.as_ptr()),
                    )
                };
                let Some(address) = Address::from_ints(client, port) else {
                    // A port the sequencer lists with a number past its tables
                    // is not offered, and the walk is marked incomplete.
                    out.incomplete
                        .get_or_insert(Error::NotAddress { client, port });
                    continue;
                };
                // SAFETY: as above; the name is copied before the record changes.
                let name = owned(unsafe { sys::snd_seq_port_info_get_name(pinfo.as_ptr()) })
                    .unwrap_or_else(|| format!("Port {port}"));
                if ump.is_none() {
                    match self.ump_endpoint(address.client()) {
                        Ok(endpoint) => ump = Some(endpoint.map(Arc::new)),
                        // Not knowing a port's Endpoint is not a legacy port: the
                        // client's ports are left out and the look is short.
                        Err(error) => {
                            out.incomplete.get_or_insert(error);
                            break;
                        }
                    }
                }
                out.ports.push(PortEntry {
                    address,
                    client_name: client_name.clone(),
                    name,
                    caps: PortCaps::from_bits(caps),
                    ump: ump.clone().flatten(),
                });
            }
        }
        out
    }
}

impl Ports {
    /// Records why a query ended: `-ENOENT` is the end of the list and nothing
    /// is wrong; any other code leaves the walk short.
    fn end_of(&mut self, rc: i32, operation: Operation) {
        if rc != ENOENT
            && let Err(error) = check(rc, operation)
        {
            self.incomplete.get_or_insert(error);
        }
    }
}
