//! What a MIDI endpoint is and what it can carry.
//!
//! [`OsEndpoint`] is what enumeration returns: a name, an opaque
//! [`EndpointId`] to open it by, and the [`Endpoint`] the OS describes.
//!
//! Capability is a property of the **endpoint**, not of the platform: two
//! devices on the same machine, through the same backend, can differ. So it is
//! a plain value that travels with the endpoint, read from the OS rather than
//! assumed, and code that depends on it is an ordinary `match`.

use tutti_midi_device::{Capabilities, Endpoint, EndpointId, FunctionBlock, Identity, Supports};
use tutti_ump::{EndpointInfoNotification, FunctionBlockInfo, Protocol, StreamConfig};

/// One endpoint as enumeration found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OsEndpoint {
    /// Opaque handle to open it by.
    pub id: EndpointId,
    /// What the OS knows it by, for a [`DeviceId`](tutti_midi_device::DeviceId).
    pub identity: Identity,
    /// Display name, as the OS reports it.
    pub name: String,
    /// What the OS says it is: the protocol it speaks now, the protocols it
    /// supports and the Function Blocks it declares, with JR Timestamps off (the
    /// OS reports none).
    ///
    /// A legacy MIDI 1.0 port is MIDI 1.0 with no blocks. That is also what an
    /// endpoint whose protocol the OS did not say is: MIDI 2.0 is
    /// what this engine speaks and the wrong guess for a device, since it sends
    /// per-note messages a MIDI 1.0 endpoint cannot carry, while assuming MIDI 1.0
    /// only costs resolution.
    pub capability: Endpoint,
    /// How many Function Blocks the OS declared that the spec cannot hold, and
    /// that [`capability`](Self::capability) leaves out.
    pub blocks_left_out: usize,
}

/// What an OS describes ([`from_os`]): the Endpoint, and how many of its
/// declared Function Blocks the Endpoint leaves out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Described {
    pub(crate) endpoint: Endpoint,
    pub(crate) blocks_left_out: usize,
}

/// The Endpoint of a MIDI 1.0 port the OS describes no further: the MIDI 1.0
/// Protocol in use, with no JR Timestamps and no blocks.
pub(crate) fn legacy() -> Endpoint {
    from_os(Protocol::Midi1, None, std::iter::empty()).endpoint
}

/// The Endpoint an OS describes: in `protocol`, supporting what `offered` says,
/// with JR Timestamps off, and the blocks it declares ([`blocks_of`]), with how
/// many of those it leaves out.
///
/// A reserved protocol is one nobody can speak, so it reads as MIDI 1.0 in use;
/// the Endpoint supports what `offered` says, and only when that is neither
/// protocol is MIDI 1.0, the one thing this engine then speaks, claimed.
pub(crate) fn from_os<'a>(
    protocol: Protocol,
    offered: Option<&EndpointInfoNotification>,
    blocks: impl IntoIterator<Item = (FunctionBlockInfo, Option<&'a str>)>,
) -> Described {
    let supported = Capabilities {
        midi1: protocol == Protocol::Midi1 || offered.is_some_and(|o| o.midi1),
        midi2: protocol == Protocol::Midi2 || offered.is_some_and(|o| o.midi2),
        rx_jr: false,
        tx_jr: false,
    };
    let supports = Supports::new(supported).unwrap_or(Supports::of(Capabilities {
        midi1: true,
        ..Capabilities::default()
    }));
    let in_use = match protocol {
        Protocol::Midi2 => Protocol::Midi2,
        _ => Protocol::Midi1,
    };
    let mut endpoint = Endpoint::new(supports).in_use(StreamConfig {
        protocol: in_use,
        rx_jr: false,
        tx_jr: false,
    });
    let (kept, blocks_left_out) = blocks_of(blocks);
    for block in kept {
        endpoint = endpoint.block(block);
    }
    Described {
        endpoint,
        blocks_left_out,
    }
}

/// The Function Blocks an OS declares: each as the OS describes it, in the
/// order given, read as a receiver reads what a device reports
/// ([`FunctionBlock::received`]): a UI hint the direction rules out reads as
/// unknown, a MIDI 1.0 port over several Groups or of a reserved value as not a
/// port, an over-long name is cut and a name with a NUL or a Byte Order Mark
/// dropped, the block kept. A block that is still out of range (a number above
/// 31, a Group span off the end, a reserved direction) is left out, never
/// clamped, and logged at `warn` with its fields. Returns the blocks kept and
/// how many were left out.
fn blocks_of<'a>(
    blocks: impl IntoIterator<Item = (FunctionBlockInfo, Option<&'a str>)>,
) -> (Vec<FunctionBlock>, usize) {
    let mut kept = Vec::new();
    let mut left_out = 0;
    for (info, name) in blocks {
        match FunctionBlock::received(&info, name) {
            Some(block) => kept.push(block),
            None => {
                left_out += 1;
                tracing::warn!(
                    ?info,
                    ?name,
                    "an OS Function Block the spec cannot hold is left out"
                );
            }
        }
    }
    (kept, left_out)
}

/// Whether the endpoint an OS describes is a UMP Endpoint that can be asked
/// what it is: one in the MIDI 2.0 Protocol, or one that declares blocks. A MIDI
/// 1.0 port with no blocks has no UMP Stream to ask (M2-104 Appendix I).
pub(crate) fn is_ump_endpoint(os: &Endpoint) -> bool {
    carries_midi2_only(os) || os.function_blocks().next().is_some()
}

/// Whether the endpoint carries MIDI-2-only messages (per-note controllers,
/// per-note pitch bend, JR Timestamps) intact. A MIDI 1.0 endpoint carries MIDI
/// 1.0 messages, which have none of those: the output translates the graph's MIDI
/// 2.0 messages for it (`OpenOutput`).
pub(crate) fn carries_midi2_only(os: &Endpoint) -> bool {
    os.active().protocol == Protocol::Midi2
}

/// The Endpoint the session presents for a device the OS describes as `os`
/// until the device says otherwise: `os`, except that a MIDI 1.0 port that
/// declares no block and offers no MIDI 2.0 is [`Endpoint::midi1_device`], with
/// its one inferred block.
pub(crate) fn described(os: &Endpoint) -> Endpoint {
    if !carries_midi2_only(os) && os.function_blocks().next().is_none() && !os.capabilities().midi2
    {
        Endpoint::midi1_device(None)
    } else {
        *os
    }
}

#[cfg(test)]
mod tests;
