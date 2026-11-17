//! The slab's backing file on Windows: a file in the temp directory that ends
//! with its last mapping.
//!
//! A file with a mapped view cannot be removed by name, and the peer may still
//! map it when its creator is done. So the creator opens it with
//! `FILE_FLAG_DELETE_ON_CLOSE` and keeps that handle for as long as its mapping
//! lives; the file is deleted when the last handle and the last view of it close,
//! in either process, including after a crash (observed NTFS behaviour, not a
//! documented guarantee). The file sits in the temp directory, so its access is
//! that directory's ACL. The peer opens it by name with the
//! default share mode (read, write and delete), which the creator's open admits,
//! and does not ask for deletion.

use super::Open;
use crate::SlabError;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_OPEN_REPARSE_POINT,
};

/// A handle to the file kept for as long as the mapping lives: the creator's
/// carries the delete-on-close, and the name stays openable while it is held.
/// It exists from the moment the creator's file does, so a failure after that
/// closes it and deletes the file.
#[derive(Debug)]
pub(super) struct Backing {
    _file: File,
}

/// Creates or opens the slab file, with the [`Backing`] that ends it.
pub(super) fn open_file(name: &str, size: usize, mode: Open) -> Result<(File, Backing), SlabError> {
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
            // The reparse point itself is opened, never its target, as
            // `O_NOFOLLOW` does on unix.
            opts.read(true)
                .write(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
    }
    let file = opts
        .open(path(name))
        .map_err(|source| SlabError::File { op: "open", source })?;
    let backing = Backing {
        _file: file.try_clone().map_err(|source| SlabError::File {
            op: "duplicate the handle of",
            source,
        })?,
    };
    if matches!(mode, Open::Create) {
        file.set_len(size as u64)
            .map_err(|source| SlabError::File {
                op: "set the size of",
                source,
            })?;
    }
    Ok((file, backing))
}

pub(super) fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tutti_{name}"))
}
