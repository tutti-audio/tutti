//! What names a byte port: its identity, and the id it is opened by.

use crate::capability::{OsEndpoint, legacy};
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
    /// The driver's name for this port (`szPname`), which differs between a
    /// device's ports where the friendly name does not.
    pub driver_name: String,
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

/// One port as the display rule sees it.
#[derive(Clone, Copy, Debug)]
pub(in crate::backend) struct Listed<'a> {
    /// The name to show before disambiguation.
    pub shown: &'a str,
    /// The driver's name for the port; empty or blank when it gives none.
    pub driver_name: &'a str,
}

impl<'a> Listed<'a> {
    /// The driver's name, trimmed; `None` when it gives none.
    fn name(&self) -> Option<&'a str> {
        let name = self.driver_name.trim();
        (!name.is_empty()).then_some(name)
    }
}

/// The names to show for `listed` ports, in list order, each distinct.
///
/// 1. A shown name that only one port has is that port's, whatever the order:
///    it is reserved before any other port is named.
/// 2. A port whose shown name is shared takes its driver name (trimmed) when it
///    is not blank, no other port (of any group) has the same one, and it is not
///    another port's shown name.
/// 3. The remaining ports of a group take the shown name, then " 2", " 3", ...
///    (the first not yet taken), in list order.
///
/// Display only: an id never reads a name.
pub(in crate::backend) fn distinct_names(listed: &[Listed<'_>]) -> Vec<String> {
    let count = |shown: &str| listed.iter().filter(|l| l.shown == shown).count();
    let mut names: Vec<Option<String>> = listed
        .iter()
        .map(|l| (count(l.shown) == 1).then(|| l.shown.to_owned()))
        .collect();
    for (at, l) in listed.iter().enumerate() {
        if names[at].is_some() {
            continue;
        }
        let Some(name) = l.name() else {
            continue;
        };
        let alone = listed
            .iter()
            .filter(|other| other.name() == Some(name))
            .count()
            == 1;
        let another_ports_shown = listed
            .iter()
            .any(|other| other.shown != l.shown && other.shown == name);
        if alone && !another_ports_shown {
            names[at] = Some(name.to_owned());
        }
    }
    for (at, l) in listed.iter().enumerate() {
        if names[at].is_some() {
            continue;
        }
        let mut name = l.shown.to_owned();
        let mut number = 2;
        while names.iter().flatten().any(|taken| *taken == name) {
            name = format!("{} {number}", l.shown);
            number += 1;
        }
        names[at] = Some(name);
    }
    names.into_iter().flatten().collect()
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
        capability: legacy(),
        blocks_left_out: 0,
    }
}

/// The endpoints of `side`'s ports, in list order, each numbered among the
/// ports with the same digest, and named apart by [`distinct_names`].
pub(in crate::backend) fn endpoints(
    side: Side,
    ports: impl IntoIterator<Item = PortInfo>,
) -> Vec<OsEndpoint> {
    let ports: Vec<PortInfo> = ports.into_iter().collect();
    let listed: Vec<Listed<'_>> = ports
        .iter()
        .map(|info| Listed {
            shown: &info.name,
            driver_name: &info.driver_name,
        })
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
