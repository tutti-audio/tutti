//! What names a byte port: its identity, and the id it is opened by.

use crate::capability::{OsEndpoint, UmpCapability};
use std::collections::BTreeMap;

use tutti_midi_device::{EndpointId, Identity, WinMmDevice};

/// Which way a port goes. Inputs and outputs are numbered separately by such an
/// API, so a position alone names neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::backend) enum Side {
    /// A port that sends MIDI to this engine.
    Input,
    /// A port this engine sends MIDI to.
    Output,
}

/// What a driver reports of one port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::backend) struct PortInfo {
    /// The name to show: the friendly name where the device has one, shared by
    /// every port of a multi-port device.
    pub name: String,
    /// The driver's product name for this port, which differs between a
    /// device's ports where the friendly name does not.
    pub product: String,
    /// What names the device: its interface path where the driver gives one.
    pub device: WinMmDevice,
}

/// The parts of an [`EndpointId`] a byte port mints.
///
/// None of them is the port's position in the OS's list: that shifts when an
/// earlier device is unplugged, and the id of a device that never left must not
/// change with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::backend) struct Slot {
    /// The port's direction.
    pub side: Side,
    /// A digest of what names the device, 31 bits.
    pub fingerprint: u32,
    /// Which of the ports with this digest on this side it is, counting from 0
    /// in list order: devices that cannot be told apart are numbered.
    pub occurrence: u32,
}

const OUTPUT_BIT: u64 = 1 << 63;
const FINGERPRINT_BITS: u32 = 31;
const FINGERPRINT_SHIFT: u32 = 32;

/// A 31-bit FNV-1a digest of what names the device: the part of an id that says
/// which device a port is. The 64-bit hash is folded to 31 bits; two devices
/// that collide are numbered apart by their occurrence, so a collision costs
/// nothing but a possible swap of two ports' ids across an unplug.
pub(in crate::backend) fn fingerprint(device: &WinMmDevice) -> u32 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let hash = device
        .to_string()
        .bytes()
        .fold(OFFSET, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME));
    ((hash >> 32) ^ hash) as u32 & ((1 << FINGERPRINT_BITS) - 1)
}

/// The id of the `occurrence`th port on `side` that `device` names.
pub(in crate::backend) fn endpoint_id(
    side: Side,
    occurrence: u32,
    device: &WinMmDevice,
) -> EndpointId {
    let side = match side {
        Side::Input => 0,
        Side::Output => OUTPUT_BIT,
    };
    EndpointId::from_raw(
        side | u64::from(fingerprint(device)) << FINGERPRINT_SHIFT | u64::from(occurrence),
    )
}

/// The parts of `id`. Every `u64` is some port's id, so this does not fail; a
/// port that is not there is the open's refusal.
pub(in crate::backend) fn slot_of(id: EndpointId) -> Slot {
    let raw = id.raw();
    Slot {
        side: if raw & OUTPUT_BIT == 0 {
            Side::Input
        } else {
            Side::Output
        },
        fingerprint: (raw >> FINGERPRINT_SHIFT) as u32 & ((1 << FINGERPRINT_BITS) - 1),
        occurrence: raw as u32,
    }
}

/// The port `slot` names: the `occurrence`th of `listed` (a port as the OS
/// edge returns it, and what names its device: never its display name) whose
/// digest matches. `None` when the device is not listed.
pub(in crate::backend) fn locate<P>(
    slot: Slot,
    listed: impl IntoIterator<Item = (P, WinMmDevice)>,
) -> Option<P> {
    listed
        .into_iter()
        .filter(|(_, device)| fingerprint(device) == slot.fingerprint)
        .nth(slot.occurrence as usize)
        .map(|(port, _)| port)
}

/// The names to show for ports listed as `(shown, product)` in list order, each
/// distinct from the rest.
///
/// A port whose shown name no other port has keeps it. Ports that share a shown
/// name take their product names when those differ from each other; any name
/// still taken by an earlier port gets a suffix, " 2", " 3", ... (the first
/// number not taken), in list order. Display only: an id never reads a name.
pub(in crate::backend) fn distinct_names(listed: &[(&str, &str)]) -> Vec<String> {
    let shares = |shown: &str| listed.iter().filter(|(other, _)| *other == shown).count() > 1;
    let products_differ = |shown: &str| {
        let mut products: Vec<&str> = listed
            .iter()
            .filter(|(other, _)| *other == shown)
            .map(|(_, product)| *product)
            .collect();
        products.sort_unstable();
        products.windows(2).all(|pair| pair[0] != pair[1])
    };
    let mut names: Vec<String> = Vec::with_capacity(listed.len());
    for (shown, product) in listed {
        let wanted = if shares(shown) && products_differ(shown) {
            product
        } else {
            shown
        };
        let mut name = (*wanted).to_owned();
        let mut number = 2;
        while names.contains(&name) {
            name = format!("{wanted} {number}");
            number += 1;
        }
        names.push(name);
    }
    names
}

/// The endpoint of the `occurrence`th port that `info` describes on `side`: a
/// MIDI 1.0 port with no Function Blocks, named as the driver names it.
pub(in crate::backend) fn endpoint(side: Side, occurrence: u32, info: PortInfo) -> OsEndpoint {
    let identity = match side {
        Side::Input => Identity::winmm_input(info.device.clone()),
        Side::Output => Identity::winmm_output(info.device.clone()),
    };
    OsEndpoint {
        id: endpoint_id(side, occurrence, &info.device),
        identity,
        name: info.name,
        capability: UmpCapability::midi1(),
    }
}

/// The endpoints of `side`'s ports, in list order, each numbered among the
/// ports with the same digest, and named apart by [`distinct_names`].
pub(in crate::backend) fn endpoints(
    side: Side,
    ports: impl IntoIterator<Item = PortInfo>,
) -> Vec<OsEndpoint> {
    let ports: Vec<PortInfo> = ports.into_iter().collect();
    let listed: Vec<(&str, &str)> = ports
        .iter()
        .map(|info| (info.name.as_str(), info.product.as_str()))
        .collect();
    let names = distinct_names(&listed);
    let mut seen: BTreeMap<u32, u32> = BTreeMap::new();
    ports
        .into_iter()
        .zip(names)
        .map(|(info, name)| {
            let count = seen.entry(fingerprint(&info.device)).or_insert(0);
            let occurrence = *count;
            *count += 1;
            endpoint(side, occurrence, PortInfo { name, ..info })
        })
        .collect()
}
