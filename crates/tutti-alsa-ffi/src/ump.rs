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

use crate::client::{Client, c_name};
use crate::error::{Error, Operation, Result, check};
use crate::records::{BlockRecord, EndpointRecord};
use crate::sys;

/// Function Blocks a UMP Endpoint has: numbered 0 to 31.
const MAX_BLOCKS: u32 = 32;

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
    matches!(-rc, libc::ENODEV | libc::EBADFD)
}

/// The Endpoint's protocol bits (`SND_UMP_EP_INFO_PROTO_MIDI_MASK`): which of
/// the two are set, in `protocol` (the one in use) or `protocol_caps` (those
/// offered).
pub const PROTO_MIDI1: u32 = 0x0100;
/// `SND_UMP_EP_INFO_PROTO_MIDI2`.
pub const PROTO_MIDI2: u32 = 0x0200;
/// `SND_UMP_EP_INFO_PROTO_JRTS_TX`: the Endpoint sends JR Timestamps.
pub const PROTO_JRTS_TX: u32 = 0x0001;
/// `SND_UMP_EP_INFO_PROTO_JRTS_RX`: the Endpoint receives JR Timestamps.
pub const PROTO_JRTS_RX: u32 = 0x0002;

/// `SND_UMP_DIR_INPUT`.
pub const DIR_INPUT: u32 = 0x01;
/// `SND_UMP_DIR_OUTPUT`.
pub const DIR_OUTPUT: u32 = 0x02;
/// `SND_UMP_DIR_BIDIRECTION`.
pub const DIR_BIDIRECTION: u32 = 0x03;

/// `SND_UMP_BLOCK_IS_MIDI1`: the block is a MIDI 1.0 port.
pub const BLOCK_IS_MIDI1: u32 = 1;
/// `SND_UMP_BLOCK_IS_LOWSPEED`: the block's port is limited to 31.25 kbps.
pub const BLOCK_IS_LOWSPEED: u32 = 2;

/// One UMP Endpoint as the sequencer records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpEndpoint {
    /// The protocol bits in use now (`PROTO_*`: one of the two MIDI
    /// protocols, and the JR Timestamp directions).
    pub protocol: u32,
    /// The protocol bits the Endpoint offers.
    pub protocol_caps: u32,
    /// The Endpoint's name, when it has one.
    pub name: Option<String>,
    /// Its blocks, by number, as many as the sequencer answered for.
    pub blocks: Vec<UmpBlock>,
}

/// One Function Block (Group Terminal Block) as the sequencer records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpBlock {
    /// Its number.
    pub id: u32,
    /// Whether it is active.
    pub active: bool,
    /// `BLOCK_*` bits.
    pub flags: u32,
    /// `DIR_*`.
    pub direction: u32,
    /// The first Group it spans, from 0.
    pub first_group: u32,
    /// How many Groups it spans.
    pub num_groups: u32,
    /// Its MIDI-CI version, `0` for none.
    pub midi_ci_version: u32,
    /// Its SysEx 8 streams.
    pub sysex8_streams: u32,
    /// The UI hint: `0` unknown, `1` receiver, `2` sender, `3` both.
    pub ui_hint: u32,
    /// Its name, when it has one.
    pub name: Option<String>,
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
    pub fn ump_endpoint(&self, client: i32) -> Result<Option<UmpEndpoint>> {
        let info = EndpointRecord::new()?;
        // SAFETY: `self.raw()` is open and `info` is a live record; the call's
        // `info` is typed `void *` in alsa-lib's header.
        let rc =
            unsafe { sys::snd_seq_get_ump_endpoint_info(self.raw(), client, info.as_ptr().cast()) };
        if is_absent(rc) {
            return Ok(None);
        }
        check(rc, Operation::GetUmpEndpointInfo)?;
        let ptr = info.as_ptr();
        // SAFETY: `info` is a live record the call just filled; each getter
        // reads it and the name is copied before the record is used again.
        let (protocol, protocol_caps, count, ep_name) = unsafe {
            (
                sys::snd_ump_endpoint_info_get_protocol(ptr),
                sys::snd_ump_endpoint_info_get_protocol_caps(ptr),
                sys::snd_ump_endpoint_info_get_num_blocks(ptr),
                name(sys::snd_ump_endpoint_info_get_name(ptr)),
            )
        };
        // The UMP limit: blocks are numbered 0 to 31.
        let mut blocks = Vec::new();
        for id in 0..count.min(MAX_BLOCKS) {
            blocks.extend(self.ump_block(client, id)?);
        }
        Ok(Some(UmpEndpoint {
            protocol,
            protocol_caps,
            name: ep_name,
            blocks,
        }))
    }

    /// Block `id` of `client`'s Endpoint; `None` when the sequencer has no such
    /// block.
    fn ump_block(&self, client: i32, id: u32) -> Result<Option<UmpBlock>> {
        let info = BlockRecord::new()?;
        // SAFETY: `self.raw()` is open and `info` is a live record; the call's
        // `info` is typed `void *` in alsa-lib's header.
        let rc = unsafe {
            sys::snd_seq_get_ump_block_info(
                self.raw(),
                client,
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
        let block = unsafe {
            UmpBlock {
                id: sys::snd_ump_block_info_get_block_id(p),
                active: sys::snd_ump_block_info_get_active(p) != 0,
                flags: sys::snd_ump_block_info_get_flags(p),
                direction: sys::snd_ump_block_info_get_direction(p),
                first_group: sys::snd_ump_block_info_get_first_group(p),
                num_groups: sys::snd_ump_block_info_get_num_groups(p),
                midi_ci_version: sys::snd_ump_block_info_get_midi_ci_version(p),
                sysex8_streams: sys::snd_ump_block_info_get_sysex8_streams(p),
                ui_hint: sys::snd_ump_block_info_get_ui_hint(p),
                name: name(sys::snd_ump_block_info_get_name(p)),
            }
        };
        Ok(Some(block))
    }

    /// Publishes `endpoint` and its blocks as this client's own UMP Endpoint,
    /// as a user-space device does; the sequencer announces the change.
    ///
    /// # Errors
    ///
    /// [`Error::NulInName`] for a name with a NUL and [`Error::BlockId`] for a
    /// block past number 31, both before anything is published; else the code
    /// of the call that failed.
    pub fn publish_ump_endpoint(&self, endpoint: &UmpEndpoint) -> Result<()> {
        let info = EndpointRecord::new()?;
        let p = info.as_ptr();
        let cname = c_name_of(endpoint.name.as_deref())?;
        let mut numbered = Vec::with_capacity(endpoint.blocks.len());
        for block in &endpoint.blocks {
            numbered.push((
                block,
                block_number(block.id)?,
                c_name_of(block.name.as_deref())?,
            ));
        }
        // The count is the highest block number plus one: the sequencer refuses a
        // block numbered at or past it.
        let count = endpoint
            .blocks
            .iter()
            .map(|block| block.id + 1)
            .max()
            .unwrap_or(0);
        // SAFETY: `info` is a live record; `cname` outlives the call that copies it.
        unsafe {
            sys::snd_ump_endpoint_info_set_protocol_caps(p, endpoint.protocol_caps);
            sys::snd_ump_endpoint_info_set_protocol(p, endpoint.protocol);
            sys::snd_ump_endpoint_info_set_num_blocks(p, count);
            sys::snd_ump_endpoint_info_set_name(p, cname.as_ptr());
        }
        // SAFETY: `self.raw()` is open and `info` is a live record.
        let rc = unsafe { sys::snd_seq_set_ump_endpoint_info(self.raw(), p.cast()) };
        check(rc, Operation::SetUmpEndpointInfo)?;
        for (block, id, cname) in &numbered {
            let info = BlockRecord::new()?;
            let p = info.as_ptr();
            // SAFETY: `info` is a live record; `cname` outlives the call that copies it.
            unsafe {
                sys::snd_ump_block_info_set_block_id(p, block.id);
                sys::snd_ump_block_info_set_active(p, u32::from(block.active));
                sys::snd_ump_block_info_set_direction(p, block.direction);
                sys::snd_ump_block_info_set_first_group(p, block.first_group);
                sys::snd_ump_block_info_set_num_groups(p, block.num_groups);
                sys::snd_ump_block_info_set_ui_hint(p, block.ui_hint);
                sys::snd_ump_block_info_set_flags(p, block.flags);
                sys::snd_ump_block_info_set_midi_ci_version(p, block.midi_ci_version);
                sys::snd_ump_block_info_set_sysex8_streams(p, block.sysex8_streams);
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
}
