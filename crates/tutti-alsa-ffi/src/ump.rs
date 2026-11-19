//! What the sequencer says about a client's UMP Endpoint and its Function
//! Blocks (the kernel's Group Terminal Blocks).
//!
//! For a USB MIDI 2.0 device the kernel runs the UMP discovery itself when the
//! device is plugged in and keeps the answer: the Endpoint's protocol now, and
//! the protocols it offers, are the kernel's record of it, and the blocks are
//! the ones the device declared (or its Group Terminal Blocks). A user-space
//! client may publish the same records for itself.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use tutti_ump::{
    BlockDirection, EndpointInfoNotification, Field, FirstGroup, FunctionBlockCount,
    FunctionBlockId, FunctionBlockInfo, FunctionBlocks, GroupSpan, Midi1Port, OsFunctionBlock,
    Protocol, UiHint, UmpVersion, read_function_block, read_protocol,
};

use crate::address::ClientId;
use crate::client::{Client, c_name};
use crate::error::{Error, Operation, Result, check};
use crate::records::{BlockRecord, EndpointRecord};
use crate::sys;

/// Function Blocks a UMP Endpoint has: numbered 0 to 31.
const MAX_BLOCKS: u32 = 32;

/// `SND_UMP_EP_INFO_STATIC_BLOCKS`: the Endpoint's blocks never change.
const EP_STATIC_BLOCKS: u32 = 0x01;
/// Where the MIDI protocol bits (`SND_UMP_EP_INFO_PROTO_MIDI_MASK`) start.
const PROTO_MIDI_SHIFT: u32 = 8;
/// `SND_UMP_EP_INFO_PROTO_MIDI1`: set in `protocol` (the one in use) or
/// `protocol_caps` (those offered).
const PROTO_MIDI1: u32 = 0x0100;
/// `SND_UMP_EP_INFO_PROTO_MIDI2`.
const PROTO_MIDI2: u32 = 0x0200;
/// `SND_UMP_EP_INFO_PROTO_JRTS_TX`: the Endpoint sends JR Timestamps.
const PROTO_JRTS_TX: u32 = 0x0001;
/// `SND_UMP_EP_INFO_PROTO_JRTS_RX`: the Endpoint receives JR Timestamps.
const PROTO_JRTS_RX: u32 = 0x0002;

/// `SND_UMP_BLOCK_IS_MIDI1`: the block is a MIDI 1.0 port.
const BLOCK_IS_MIDI1: u32 = 1;
/// `SND_UMP_BLOCK_IS_LOWSPEED`: the block's port is limited to 31.25 kbps.
const BLOCK_IS_LOWSPEED: u32 = 2;

/// Block `id` as the number alsa-lib takes.
fn block_number(id: u32) -> Result<i32> {
    if id >= MAX_BLOCKS {
        return Err(Error::BlockId { id });
    }
    i32::try_from(id).map_err(|_| Error::BlockId { id })
}

/// A name as a C string; none is the empty name.
fn c_name_of(name: Option<&str>) -> Result<CString> {
    name.map_or_else(|| Ok(CString::default()), c_name)
}

/// Whether a failed record query means the sequencer has none: `ENODEV` for a
/// user client that never published a UMP Endpoint or block, `EBADFD` for a
/// kernel client (the System, Midi Through or a hardware port). Any other code,
/// `ENOENT` for a client that has exited among them, is a failure.
fn is_absent(rc: i32) -> bool {
    matches!(rc.checked_neg(), Some(libc::ENODEV | libc::EBADFD))
}

/// One UMP Endpoint as the sequencer records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpEndpoint {
    /// The protocol in use now. A record that names neither MIDI protocol, or
    /// both, is [`Protocol::Reserved`] with the raw protocol bits as read.
    pub protocol: Protocol,
    /// What the Endpoint offers, as its Endpoint Info Notification: the UMP
    /// version, whether its blocks are static, how many it has, and the
    /// protocols and JR Timestamp directions it supports. Publishing writes the
    /// version, the static flag and the protocols and JR directions; the count is
    /// that of [`blocks`](Self::blocks).
    pub offered: EndpointInfoNotification,
    /// The Endpoint's name, when it has one.
    pub name: Option<String>,
    /// Its blocks, by number, as many as the sequencer answered for.
    pub blocks: Vec<UmpBlock>,
}

/// One Function Block (Group Terminal Block) as the sequencer records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpBlock {
    /// What the record says of the block, as a Function Block Info
    /// Notification carries it. A value the record has that the spec does not
    /// define is the field's reserved raw, as read.
    pub info: FunctionBlockInfo,
    /// Its name, when it has one, as long as the sequencer holds it.
    pub name: Option<String>,
}

/// A number alsa-lib reports as the byte the spec's field holds: one past a
/// byte is `u8::MAX`, which no field of a Function Block defines.
fn byte(value: u32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

/// The numbers of a `snd_ump_block_info_t`, as alsa-lib gets and sets them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlockFields {
    id: u32,
    active: bool,
    flags: u32,
    direction: u32,
    first_group: u32,
    num_groups: u32,
    midi_ci_version: u32,
    sysex8_streams: u32,
    ui_hint: u32,
}

/// A field's number, or [`Error::Reserved`] naming `what` for a reserved raw: the
/// record holds only the numbers the spec defines.
fn defined<T>(field: Field<T>, what: &'static str, get: fn(T) -> u8) -> Result<u32> {
    match field {
        Field::Value(value) => Ok(u32::from(get(value))),
        Field::Reserved(_) => Err(Error::Reserved { what }),
    }
}

/// The UI hint a record's number names. The kernel's hint is the two bits the
/// wire carries (`SND_UMP_BLOCK_UI_HINT_*`), so the bits above them are not part
/// of it.
fn ui_hint_of(raw: u32) -> UiHint {
    match raw & 0x3 {
        0 => UiHint::Unknown,
        1 => UiHint::Receiver,
        2 => UiHint::Sender,
        _ => UiHint::Both,
    }
}

impl BlockFields {
    /// The record's numbers as a Function Block Info, read as the decoder
    /// reads the wire.
    fn info(self) -> FunctionBlockInfo {
        let midi1 = match (
            self.flags & BLOCK_IS_MIDI1 != 0,
            self.flags & BLOCK_IS_LOWSPEED != 0,
        ) {
            (false, _) => 0,
            (true, false) => 1,
            (true, true) => 2,
        };
        read_function_block(OsFunctionBlock {
            active: self.active,
            id: byte(self.id),
            direction: byte(self.direction),
            ui_hint: ui_hint_of(self.ui_hint),
            midi1,
            first_group: byte(self.first_group),
            groups_spanned: byte(self.num_groups),
            midi_ci_version: byte(self.midi_ci_version),
            max_sysex8_streams: byte(self.sysex8_streams),
        })
    }

    /// The numbers that record `info`.
    ///
    /// # Errors
    ///
    /// [`Error::Reserved`] for a reserved field (the block number, direction,
    /// first Group, Group span or MIDI 1.0 port), which the record cannot hold.
    fn of(info: &FunctionBlockInfo) -> Result<Self> {
        let flags = match info.midi1 {
            Midi1Port::No => 0,
            Midi1Port::Unrestricted => BLOCK_IS_MIDI1,
            Midi1Port::Restricted => BLOCK_IS_MIDI1 | BLOCK_IS_LOWSPEED,
            Midi1Port::Reserved(_) => {
                return Err(Error::Reserved {
                    what: "MIDI 1.0 port",
                });
            }
        };
        Ok(Self {
            id: defined(info.id, "block number", FunctionBlockId::get)?,
            active: info.active,
            flags,
            direction: match info.direction {
                BlockDirection::Input => 1,
                BlockDirection::Output => 2,
                BlockDirection::Bidirectional => 3,
                BlockDirection::Reserved(_) => {
                    return Err(Error::Reserved { what: "direction" });
                }
            },
            first_group: defined(info.first_group, "first Group", FirstGroup::get)?,
            num_groups: defined(info.groups_spanned, "Group span", GroupSpan::get)?,
            midi_ci_version: u32::from(info.midi_ci_version),
            sysex8_streams: u32::from(info.max_sysex8_streams),
            ui_hint: match info.ui_hint {
                UiHint::Unknown => 0,
                UiHint::Receiver => 1,
                UiHint::Sender => 2,
                UiHint::Both => 3,
            },
        })
    }
}

/// The protocol in use as the record's bits name it.
fn protocol_of(bits: u32) -> Protocol {
    read_protocol(byte(
        (bits & (PROTO_MIDI1 | PROTO_MIDI2)) >> PROTO_MIDI_SHIFT,
    ))
}

/// The bits that record `protocol` in use.
///
/// # Errors
///
/// [`Error::Reserved`] for a reserved protocol.
fn protocol_bits(protocol: Protocol) -> Result<u32> {
    match protocol {
        Protocol::Midi1 => Ok(PROTO_MIDI1),
        Protocol::Midi2 => Ok(PROTO_MIDI2),
        _ => Err(Error::Reserved { what: "protocol" }),
    }
}

/// The Endpoint Info Notification a record's numbers are.
fn offered_of(version: u32, flags: u32, num_blocks: u32, caps: u32) -> EndpointInfoNotification {
    let count = FunctionBlockCount::saturating(usize::try_from(num_blocks).unwrap_or(usize::MAX));
    EndpointInfoNotification {
        version: UmpVersion::new(byte(version >> 8), byte(version & 0xFF)),
        static_function_blocks: flags & EP_STATIC_BLOCKS != 0,
        function_blocks: FunctionBlocks::Count(count),
        midi2: caps & PROTO_MIDI2 != 0,
        midi1: caps & PROTO_MIDI1 != 0,
        rx_jr: caps & PROTO_JRTS_RX != 0,
        tx_jr: caps & PROTO_JRTS_TX != 0,
    }
}

/// The record's version word: the major version over the minor.
fn version_word(version: UmpVersion) -> u32 {
    u32::from(version.major) << 8 | u32::from(version.minor)
}

/// The record's flags: whether the Endpoint's blocks are static.
fn endpoint_flags(offered: &EndpointInfoNotification) -> u32 {
    if offered.static_function_blocks {
        EP_STATIC_BLOCKS
    } else {
        0
    }
}

/// The protocols and JR Timestamp directions an Endpoint Info Notification
/// offers, as the record's bits.
fn offered_bits(offered: &EndpointInfoNotification) -> u32 {
    [
        (offered.midi1, PROTO_MIDI1),
        (offered.midi2, PROTO_MIDI2),
        (offered.rx_jr, PROTO_JRTS_RX),
        (offered.tx_jr, PROTO_JRTS_TX),
    ]
    .into_iter()
    .filter(|&(set, _)| set)
    .fold(0, |bits, (_, bit)| bits | bit)
}

/// A C string alsa-lib owns, copied; `None` for NULL or an empty name.
fn name(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: non-NULL, and alsa-lib's info getters return NUL-terminated
    // strings valid until the owning record is freed or changed; copied at once.
    let text = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    (!text.is_empty()).then_some(text)
}

impl Client {
    /// The UMP Endpoint of `client`, and its blocks; `None` when the sequencer
    /// has none for it: a legacy client, reached through the kernel's
    /// converter, or a UMP client that never published one.
    ///
    /// # Errors
    ///
    /// The code of a query that failed for any other reason than the Endpoint
    /// being absent.
    pub fn ump_endpoint(&self, client: ClientId) -> Result<Option<UmpEndpoint>> {
        let info = EndpointRecord::new()?;
        // SAFETY: `self.raw()` is open and `info` is a live record; the call's
        // `info` is typed `void *` in alsa-lib's header.
        let rc = unsafe {
            sys::snd_seq_get_ump_endpoint_info(
                self.raw(),
                i32::from(client.get()),
                info.as_ptr().cast(),
            )
        };
        if is_absent(rc) {
            return Ok(None);
        }
        check(rc, Operation::GetUmpEndpointInfo)?;
        let ptr = info.as_ptr();
        // SAFETY: `info` is a live record the call just filled; each getter
        // reads it and the name is copied before the record is used again.
        let (protocol, caps, count, version, flags, ep_name) = unsafe {
            (
                sys::snd_ump_endpoint_info_get_protocol(ptr),
                sys::snd_ump_endpoint_info_get_protocol_caps(ptr),
                sys::snd_ump_endpoint_info_get_num_blocks(ptr),
                sys::snd_ump_endpoint_info_get_version(ptr),
                sys::snd_ump_endpoint_info_get_flags(ptr),
                name(sys::snd_ump_endpoint_info_get_name(ptr)),
            )
        };
        // The UMP limit: blocks are numbered 0 to 31.
        let mut blocks = Vec::new();
        for id in 0..count.min(MAX_BLOCKS) {
            blocks.extend(self.ump_block(client, id)?);
        }
        Ok(Some(UmpEndpoint {
            protocol: protocol_of(protocol),
            offered: offered_of(version, flags, count, caps),
            name: ep_name,
            blocks,
        }))
    }

    /// Block `id` of `client`'s Endpoint; `None` when the sequencer has no such
    /// block.
    fn ump_block(&self, client: ClientId, id: u32) -> Result<Option<UmpBlock>> {
        let info = BlockRecord::new()?;
        // SAFETY: `self.raw()` is open and `info` is a live record; the call's
        // `info` is typed `void *` in alsa-lib's header.
        let rc = unsafe {
            sys::snd_seq_get_ump_block_info(
                self.raw(),
                i32::from(client.get()),
                block_number(id)?,
                info.as_ptr().cast(),
            )
        };
        if is_absent(rc) {
            return Ok(None);
        }
        check(rc, Operation::GetUmpBlockInfo)?;
        let p = info.as_ptr();
        // SAFETY: `info` is a live record the call just filled.
        let (fields, block_name) = unsafe {
            (
                BlockFields {
                    id: sys::snd_ump_block_info_get_block_id(p),
                    active: sys::snd_ump_block_info_get_active(p) != 0,
                    flags: sys::snd_ump_block_info_get_flags(p),
                    direction: sys::snd_ump_block_info_get_direction(p),
                    first_group: sys::snd_ump_block_info_get_first_group(p),
                    num_groups: sys::snd_ump_block_info_get_num_groups(p),
                    midi_ci_version: sys::snd_ump_block_info_get_midi_ci_version(p),
                    sysex8_streams: sys::snd_ump_block_info_get_sysex8_streams(p),
                    ui_hint: sys::snd_ump_block_info_get_ui_hint(p),
                },
                name(sys::snd_ump_block_info_get_name(p)),
            )
        };
        Ok(Some(UmpBlock {
            info: fields.info(),
            name: block_name,
        }))
    }

    /// Publishes `endpoint` and its blocks as this client's own UMP Endpoint,
    /// as a user-space device does; the sequencer announces the change.
    ///
    /// # Errors
    ///
    /// [`Error::NulInName`] for a name with a NUL, [`Error::BlockId`] for a
    /// block past number 31 and [`Error::Reserved`] for a reserved protocol or
    /// MIDI 1.0 port, all before anything is published; else the code of the
    /// call that failed.
    pub fn publish_ump_endpoint(&self, endpoint: &UmpEndpoint) -> Result<()> {
        let info = EndpointRecord::new()?;
        let p = info.as_ptr();
        let cname = c_name_of(endpoint.name.as_deref())?;
        let protocol = protocol_bits(endpoint.protocol)?;
        let mut numbered = Vec::with_capacity(endpoint.blocks.len());
        for block in &endpoint.blocks {
            let fields = BlockFields::of(&block.info)?;
            numbered.push((
                fields,
                block_number(fields.id)?,
                c_name_of(block.name.as_deref())?,
            ));
        }
        // The count is the highest block number plus one: the sequencer refuses a
        // block numbered at or past it.
        let count = numbered
            .iter()
            .map(|(fields, ..)| fields.id + 1)
            .max()
            .unwrap_or(0);
        // SAFETY: `info` is a live record; `cname` outlives the call that copies it.
        unsafe {
            sys::snd_ump_endpoint_info_set_protocol_caps(p, offered_bits(&endpoint.offered));
            sys::snd_ump_endpoint_info_set_protocol(p, protocol);
            sys::snd_ump_endpoint_info_set_version(p, version_word(endpoint.offered.version));
            sys::snd_ump_endpoint_info_set_flags(p, endpoint_flags(&endpoint.offered));
            sys::snd_ump_endpoint_info_set_num_blocks(p, count);
            sys::snd_ump_endpoint_info_set_name(p, cname.as_ptr());
        }
        // SAFETY: `self.raw()` is open and `info` is a live record.
        let rc = unsafe { sys::snd_seq_set_ump_endpoint_info(self.raw(), p.cast()) };
        check(rc, Operation::SetUmpEndpointInfo)?;
        for (fields, id, cname) in &numbered {
            let info = BlockRecord::new()?;
            let p = info.as_ptr();
            // SAFETY: `info` is a live record; `cname` outlives the call that copies it.
            unsafe {
                sys::snd_ump_block_info_set_block_id(p, fields.id);
                sys::snd_ump_block_info_set_active(p, u32::from(fields.active));
                sys::snd_ump_block_info_set_direction(p, fields.direction);
                sys::snd_ump_block_info_set_first_group(p, fields.first_group);
                sys::snd_ump_block_info_set_num_groups(p, fields.num_groups);
                sys::snd_ump_block_info_set_ui_hint(p, fields.ui_hint);
                sys::snd_ump_block_info_set_flags(p, fields.flags);
                sys::snd_ump_block_info_set_midi_ci_version(p, fields.midi_ci_version);
                sys::snd_ump_block_info_set_sysex8_streams(p, fields.sysex8_streams);
                sys::snd_ump_block_info_set_name(p, cname.as_ptr());
            }
            // SAFETY: `self.raw()` is open and `info` is a live record.
            let rc = unsafe { sys::snd_seq_set_ump_block_info(self.raw(), *id, p.cast()) };
            check(rc, Operation::SetUmpBlockInfo)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocks are numbered 0 to 31: a number past that, or one that does not fit
    /// alsa-lib's int, is refused, never published as another block.
    #[test]
    fn a_block_number_past_31_is_refused() {
        assert_eq!(block_number(0).unwrap(), 0);
        assert_eq!(block_number(31).unwrap(), 31);
        assert!(matches!(block_number(32), Err(Error::BlockId { id: 32 })));
        assert!(matches!(
            block_number(u32::MAX),
            Err(Error::BlockId { id: u32::MAX })
        ));
    }

    /// A kernel or legacy client has no record (`ENODEV`, `EBADFD`); a client
    /// that exited (`ENOENT`) or any other code is a failure.
    #[test]
    fn only_no_record_codes_mean_absent() {
        assert!(is_absent(-libc::ENODEV));
        assert!(is_absent(-libc::EBADFD));
        assert!(!is_absent(-libc::ENOENT));
        assert!(!is_absent(-libc::EINVAL));
        assert!(!is_absent(0));
    }

    /// A name with a NUL is refused; none is the empty name.
    #[test]
    fn a_name_is_nul_free_or_refused() {
        assert!(matches!(
            c_name_of(Some("a\0b")),
            Err(Error::NulInName { .. })
        ));
        assert_eq!(c_name_of(None).unwrap().as_bytes(), b"");
        assert_eq!(c_name_of(Some("ok")).unwrap().as_bytes(), b"ok");
    }

    /// The reserved MIDI 1.0 port value the spec's table leaves open.
    fn reserved_port() -> Midi1Port {
        read_function_block(OsFunctionBlock {
            active: true,
            id: 0,
            direction: 3,
            ui_hint: UiHint::Unknown,
            midi1: 3,
            first_group: 0,
            groups_spanned: 1,
            midi_ci_version: 0,
            max_sysex8_streams: 0,
        })
        .midi1
    }

    fn fields() -> BlockFields {
        BlockFields {
            id: 3,
            active: true,
            flags: 0,
            direction: 3,
            first_group: 4,
            num_groups: 2,
            midi_ci_version: 2,
            sysex8_streams: 5,
            ui_hint: 3,
        }
    }

    /// A record the spec can hold reads as the Function Block Info it says, and
    /// the numbers that record that info are the record's: no field is lost or
    /// changed either way.
    #[test]
    fn a_record_the_spec_holds_converts_both_ways_without_loss() {
        for flags in [0, BLOCK_IS_MIDI1, BLOCK_IS_MIDI1 | BLOCK_IS_LOWSPEED] {
            let record = BlockFields { flags, ..fields() };
            let info = record.info();
            assert_eq!(BlockFields::of(&info).unwrap(), record);
        }
        let info = fields().info();
        assert_eq!(info.id.value().map(FunctionBlockId::get), Some(3));
        assert_eq!(info.direction, BlockDirection::Bidirectional);
        assert_eq!(info.first_group.value().map(FirstGroup::get), Some(4));
        assert_eq!(info.groups_spanned.value().map(GroupSpan::get), Some(2));
        assert_eq!(info.ui_hint, UiHint::Both);
        assert_eq!((info.midi_ci_version, info.max_sysex8_streams), (2, 5));
    }

    /// The sequencer's two flag bits are the spec's three port values: not a
    /// port, a port, and a port restricted to 31.25 kbps; a low-speed bit on a
    /// block that is no port is no port.
    #[test]
    fn the_flag_bits_are_the_midi1_port_value() {
        let port = |flags| BlockFields { flags, ..fields() }.info().midi1;
        assert_eq!(port(0), Midi1Port::No);
        assert_eq!(port(BLOCK_IS_LOWSPEED), Midi1Port::No);
        assert_eq!(port(BLOCK_IS_MIDI1), Midi1Port::Unrestricted);
        assert_eq!(
            port(BLOCK_IS_MIDI1 | BLOCK_IS_LOWSPEED),
            Midi1Port::Restricted
        );
    }

    /// A value the sequencer holds that the spec does not define is the field's
    /// reserved raw, never another value: direction 0, block 40, a span of 0 and
    /// a number past a byte; and a reserved value is refused where a record is
    /// published.
    #[test]
    fn an_undefined_value_is_reserved_not_defaulted() {
        let info = BlockFields {
            id: 40,
            direction: 0,
            num_groups: 0,
            first_group: 300,
            ..fields()
        }
        .info();
        assert!(matches!(info.direction, BlockDirection::Reserved(r) if r.get() == 0));
        assert!(matches!(info.id, Field::Reserved(r) if r.get() == 40));
        assert!(matches!(info.groups_spanned, Field::Reserved(r) if r.get() == 0));
        assert!(matches!(info.first_group, Field::Reserved(r) if r.get() == u8::MAX));
        // The record holds only what the spec defines: each reserved field is
        // refused, not written through.
        let refused = |edit: fn(&mut BlockFields)| {
            let mut record = fields();
            edit(&mut record);
            BlockFields::of(&record.info())
        };
        assert!(matches!(
            refused(|r| r.id = 40),
            Err(Error::Reserved { .. })
        ));
        assert!(matches!(
            refused(|r| r.direction = 0),
            Err(Error::Reserved { .. })
        ));
        assert!(matches!(
            refused(|r| r.first_group = 16),
            Err(Error::Reserved { .. })
        ));
        assert!(matches!(
            refused(|r| r.num_groups = 0),
            Err(Error::Reserved { .. })
        ));
        assert!(refused(|_| {}).is_ok());
        let reserved_port = FunctionBlockInfo {
            midi1: reserved_port(),
            ..fields().info()
        };
        assert!(matches!(
            BlockFields::of(&reserved_port),
            Err(Error::Reserved { .. })
        ));
        assert!(matches!(
            protocol_bits(read_protocol(0)),
            Err(Error::Reserved { .. })
        ));
    }

    /// The record's protocol bits name one of the two protocols; none or both is
    /// reserved with the raw bits, and the offered protocols and JR directions
    /// are the Endpoint Info's bools.
    #[test]
    fn the_protocol_bits_read_as_the_spec_names_them() {
        assert_eq!(protocol_of(PROTO_MIDI1), Protocol::Midi1);
        assert_eq!(protocol_of(PROTO_MIDI2 | PROTO_JRTS_TX), Protocol::Midi2);
        assert!(matches!(protocol_of(0), Protocol::Reserved(r) if r.get() == 0));
        assert!(matches!(
            protocol_of(PROTO_MIDI1 | PROTO_MIDI2),
            Protocol::Reserved(r) if r.get() == 3
        ));
        let offered = offered_of(0x0101, EP_STATIC_BLOCKS, 2, PROTO_MIDI2 | PROTO_JRTS_RX);
        assert_eq!(offered.version, UmpVersion::new(1, 1));
        assert!(offered.static_function_blocks);
        assert!(offered.midi2 && !offered.midi1 && offered.rx_jr && !offered.tx_jr);
        assert_eq!(offered_bits(&offered), PROTO_MIDI2 | PROTO_JRTS_RX);
        // The version and the static flag survive a publish and a read.
        let back = offered_of(
            version_word(offered.version),
            endpoint_flags(&offered),
            2,
            offered_bits(&offered),
        );
        assert_eq!(back.version, offered.version);
        assert!(back.static_function_blocks);
        assert_eq!(
            offered_of(0, 0, 40, 0).function_blocks,
            FunctionBlocks::Count(FunctionBlockCount::MAX)
        );
        assert_eq!(
            protocol_bits(protocol_of(PROTO_MIDI2)).unwrap(),
            PROTO_MIDI2
        );
    }
}
