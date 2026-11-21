//! Walking the sequencer's clients and ports.

use crate::capability::{Described, OsEndpoint, from_os, legacy};
use tutti_alsa_ffi::{Client, PortCaps, PortEntry, UmpEndpoint};
use tutti_midi_device::{Identity, PortDirection};

/// Whether a port allows `direction`: a port that is readable but not
/// *subscribe*-readable cannot be connected to, so listing it would offer the
/// user a device that fails to open.
fn allows(direction: PortDirection, caps: PortCaps) -> bool {
    (!direction.is_input() || caps.can_read_from())
        && (!direction.is_output() || caps.can_write_to())
}

#[cfg(test)]
mod tests;

/// Every port on the system usable in `direction`.
///
/// Skips this client's own ports — offering the engine's own port back as a
/// device invites a feedback loop, and it is never what a user meant.
///
/// Names are `"Client: Port"`, matching how `aconnect -l` presents them, unless
/// the port name already starts with the client's.
///
/// A walk that stopped on an error gives the ports it reached. Its caller is
/// `Devices::capability` (through `inputs()` and `outputs()`): a short list
/// there is a `None` capability for a device that is still present, and the
/// next call walks again and finds it. Only a change report
/// must be whole, since a short one would remove devices that are still there
/// ([`both`]).
pub(super) fn endpoints(seq: &Client, direction: PortDirection) -> Vec<OsEndpoint> {
    walk(seq, [direction]).lists.into_iter().flatten().collect()
}

/// The inputs and the outputs from **one** walk, so a port is read once. Two
/// walks can see a port being created (the kernel lists it before it sets its
/// capabilities) in different states and report a duplex as an output.
///
/// `None` when the walk stopped on a query error, so the lists may be short:
/// a short list reported as a look would remove devices that are still there.
pub(super) fn both(seq: &Client) -> Option<(Vec<OsEndpoint>, Vec<OsEndpoint>)> {
    let Walk {
        lists: [inputs, outputs],
        complete,
    } = walk(seq, [PortDirection::Input, PortDirection::Output]);
    complete.then_some((inputs, outputs))
}

/// What a walk found.
struct Walk<const N: usize> {
    /// One list per direction asked for, in order.
    lists: [Vec<OsEndpoint>; N],
    /// Whether every query ended with `-ENOENT`, the end of the list.
    complete: bool,
}

/// One list per entry of `directions`, in order.
fn walk<const N: usize>(seq: &Client, directions: [PortDirection; N]) -> Walk<N> {
    let mut out: [Vec<OsEndpoint>; N] = std::array::from_fn(|_| Vec::new());
    // `-ENOENT` is the last client or port. A client that exits mid-walk answers
    // `-ENXIO` (`snd_seq_ioctl_query_next_port`): not the end of its ports, so
    // the walk is short and is taken again.
    let found = seq.list_ports();
    if let Some(error) = &found.incomplete {
        tracing::debug!("ALSA port walk is short: {}", crate::error::chain(error));
    }
    for port in found.ports {
        let caps = port.caps;
        let listed = |d: PortDirection| allows(d, caps);
        if !directions.iter().any(|&d| listed(d)) {
            continue;
        }
        let endpoint = endpoint_of(port);
        for (list, &d) in out.iter_mut().zip(&directions) {
            if listed(d) {
                list.push(endpoint.clone());
            }
        }
    }
    Walk {
        lists: out,
        complete: found.incomplete.is_none(),
    }
}

/// The endpoint a listed port is.
fn endpoint_of(port: PortEntry) -> OsEndpoint {
    let PortEntry {
        address,
        client_name,
        name: port_name,
        ump,
        ..
    } = port;
    let described = capability_of(ump.as_deref());
    OsEndpoint {
        id: super::handle_of(address),
        identity: Identity::alsa(client_name.as_str(), port_name.as_str()),
        // "Client: Port" is how `aconnect -l` presents them, and a
        // bare port name ("MIDI 1") is ambiguous across devices.
        name: if port_name.starts_with(&client_name) {
            port_name
        } else {
            format!("{client_name}: {port_name}")
        },
        capability: described.endpoint,
        blocks_left_out: described.blocks_left_out,
    }
}

/// What the sequencer says a client carries.
///
/// A client with a UMP Endpoint record speaks the protocol the record says it
/// has in use (the kernel runs the USB discovery and keeps the outcome there),
/// supports the protocols it offers, and declares the blocks it lists. One with
/// no record is legacy, a MIDI 1.0 source: MIDI 1.0, with no blocks. The
/// distinction is real and worth recording: a per-note controller sent to a
/// legacy port is dropped in translation, not carried. Having a block is not
/// having the MIDI 2.0 Protocol: an Endpoint that offers both and is in MIDI 1.0
/// reads as MIDI 1.0.
pub(super) fn capability_of(ump: Option<&UmpEndpoint>) -> Described {
    let Some(endpoint) = ump else {
        return Described {
            endpoint: legacy(),
            blocks_left_out: 0,
        };
    };
    from_os(
        endpoint.protocol,
        Some(&endpoint.offered),
        endpoint
            .blocks
            .iter()
            .map(|block| (block.info, block.name.as_deref())),
    )
}
