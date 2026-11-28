//! A Web MIDI port as the page reads it, and the endpoints built from the ports a
//! `MIDIAccess` lists.
//!
//! The W3C Web MIDI API names a port by `MIDIPort.id`: unique to that port, and kept
//! across sessions where the browser can. That id is the identity of the endpoint
//! ([`OsEndpointIdentity::WebMidi`]) and the digest the [`EndpointId`] is made from.

use std::collections::BTreeSet;

use tutti_midi_device::{EndpointId, OsDevice, OsEndpointIdentity};

use crate::capability::{OsEndpoint, legacy};
use crate::devices::reporter::os_devices;

use super::super::fnv1a::fnv1a;
use super::super::legacy_port::ports::{Listed, distinct_names};
use super::super::side::{OUTPUT_BIT, Side};

/// What the page reads of one `MIDIPort`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::backend) struct PortSnapshot {
    /// `MIDIPort.id`.
    pub id: String,
    /// `MIDIPort.name`, which a browser may leave null.
    pub name: Option<String>,
    /// Whether `MIDIPort.state` is `"connected"`: a `"disconnected"` port is not listed.
    pub connected: bool,
}

/// The id of the port `port_id` names on `side`: the 63-bit FNV-1a digest of the port
/// id, with the direction in the top bit, the same in every listing. An input and an
/// output are different ports even when a browser gave both one id string.
pub(in crate::backend) fn endpoint_id(side: Side, port_id: &str) -> EndpointId {
    EndpointId::from_raw(fnv1a(port_id.bytes()) & !OUTPUT_BIT | side.bit())
}

/// The name shown for a port: the browser's name for it, else its id.
fn shown(port: &PortSnapshot) -> &str {
    port.name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&port.id)
}

/// The endpoints of `side`'s connected ports, in the order `ports` lists them: each a
/// MIDI 1.0 port with no Function Blocks, shown by its name or else its id, and told
/// apart from a port of the same name by a number. A port listed twice under one id is
/// listed once (the first).
pub(in crate::backend) fn endpoints(side: Side, ports: &[PortSnapshot]) -> Vec<OsEndpoint> {
    let mut seen = BTreeSet::new();
    let connected: Vec<&PortSnapshot> = ports
        .iter()
        .filter(|port| port.connected && seen.insert(&port.id))
        .collect();
    let listed: Vec<Listed<'_>> = connected
        .iter()
        .map(|port| Listed {
            shown: shown(port),
            driver_name: "",
        })
        .collect();
    connected
        .into_iter()
        .zip(distinct_names(&listed))
        .map(|(port, name)| OsEndpoint {
            id: endpoint_id(side, &port.id),
            identity: OsEndpointIdentity::WebMidi(port.id.clone()),
            name,
            capability: legacy(),
            blocks_left_out: 0,
        })
        .collect()
}

/// Every device the page lists now: the connected ports of both sides, one device each.
pub(in crate::backend) fn devices(
    inputs: &[PortSnapshot],
    outputs: &[PortSnapshot],
) -> Vec<OsDevice> {
    os_devices(
        &endpoints(Side::Input, inputs),
        &endpoints(Side::Output, outputs),
    )
}
