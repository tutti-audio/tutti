//! The environment block of a Windows spawn: the host's variables with the
//! command's changes applied.

use super::command_line::wide;
use std::ffi::OsStr;
use std::io::{Error, ErrorKind, Result};
use std::os::windows::ffi::OsStrExt;
use std::process::Command;

/// A name as Windows compares it: each UTF-16 unit upper-cased.
fn key(name: &OsStr) -> Vec<u16> {
    name.encode_wide()
        .map(|unit| {
            let mut upper = char::from_u32(u32::from(unit))
                .into_iter()
                .flat_map(char::to_uppercase);
            match (upper.next(), upper.next()) {
                (Some(one), None) => u16::try_from(u32::from(one)).unwrap_or(unit),
                _ => unit,
            }
        })
        .collect()
}

/// The `lpEnvironment` block of a spawn of `command`: this process's variables
/// (the hidden `=C:` entries too), a variable the command sets replacing the
/// one of the same name in any case and one it removes gone, each as
/// `name=value\0` sorted by upper-cased name, and an extra `\0` at the end.
///
/// This process's own environment is read and never written. A command's
/// `env_clear` is not readable through std, so a spawn never has one. The hidden
/// per-drive current-directory variables (`=C:`) are not listed by `vars_os`, so
/// the child does not get them.
///
/// # Errors
///
/// `InvalidInput` for a NUL, an empty name or a `=` in a name set by the command.
pub(super) fn block(command: &Command) -> Result<Vec<u16>> {
    let mut vars: Vec<(Vec<u16>, Vec<u16>)> = std::env::vars_os()
        .map(|(name, value)| (key(&name), entry(&name, &value)))
        .collect();
    for (name, value) in command.get_envs() {
        let wanted = key(name);
        vars.retain(|(have, _)| *have != wanted);
        let Some(value) = value else { continue };
        let bad = name.is_empty() || name.encode_wide().skip(1).any(|u| u == u16::from(b'='));
        if bad {
            return Err(Error::new(ErrorKind::InvalidInput, "a bad variable name"));
        }
        wide(name)?;
        wide(value)?;
        vars.push((wanted, entry(name, value)));
    }
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    let mut block: Vec<u16> = Vec::new();
    for (_, pair) in &vars {
        block.extend_from_slice(pair);
        block.push(0);
    }
    // An empty block is two NULs.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// `name=value` as UTF-16.
fn entry(name: &OsStr, value: &OsStr) -> Vec<u16> {
    let mut pair: Vec<u16> = name.encode_wide().collect();
    pair.push(u16::from(b'='));
    pair.extend(value.encode_wide());
    pair
}
