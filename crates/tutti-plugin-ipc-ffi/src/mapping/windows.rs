//! The slab's backing file on Windows: a file in the temp directory that ends
//! with its last mapping.
//!
//! A file with a mapped view cannot be removed by name, and the peer may still
//! map it when its creator is done. So the creator opens it with
//! `FILE_FLAG_DELETE_ON_CLOSE` and keeps that handle for as long as its mapping
//! lives; the file is deleted when the last handle and the last view of it close,
//! in either process, including after a crash. The peer opens it by name with the
//! default share mode (read, write and delete), which the creator's open admits,
//! and does not ask for deletion.

use super::Open;
use crate::SlabError;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_DELETE_ON_CLOSE};

/// The open file handle kept for as long as the mapping lives: the creator's
/// carries the delete-on-close, and the name stays openable while it is held.
#[derive(Debug)]
pub(super) struct Backing(#[allow(dead_code, reason = "held for its drop")] File);

impl Backing {
    /// Keeps `file` open.
    pub(super) fn keep(file: File) -> Self {
        Self(file)
    }
}

pub(super) fn open_file(name: &str, size: usize, mode: Open) -> Result<File, SlabError> {
    let mut opts = OpenOptions::new();
    match mode {
        Open::Create => {
            // Exclusive (create-new): an existing name is refused, never
            // truncated under a process that may have it mapped.
            opts.read(true)
                .write(true)
                .create_new(true)
                .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
                .custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
        }
        Open::Existing => {
            opts.read(true).write(true);
        }
    }
    let file = opts
        .open(path(name))
        .map_err(|e| SlabError::File(format!("failed to open shared memory file: {e}")))?;
    if matches!(mode, Open::Create) {
        file.set_len(size as u64)
            .map_err(|e| SlabError::File(format!("failed to set file size: {e}")))?;
    }
    Ok(file)
}

pub(super) fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tutti_{name}"))
}
