//! The program and the command line of a Windows spawn, as the UTF-16 strings
//! `CreateProcessW` takes.

use std::ffi::OsStr;
use std::io::{Error, ErrorKind, Result};
use std::os::windows::ffi::OsStrExt;

/// The most UTF-16 units a command line holds, its terminating NUL included.
const LONGEST: usize = 32_767;

const BACKSLASH: u16 = b'\\' as u16;
const QUOTE: u16 = b'"' as u16;

fn invalid(reason: &'static str) -> Error {
    Error::new(ErrorKind::InvalidInput, reason)
}

/// `text` as UTF-16 with no terminator.
///
/// # Errors
///
/// `text` holds a NUL, which ends a Windows string early.
pub(super) fn wide(text: &OsStr) -> Result<Vec<u16>> {
    let wide: Vec<u16> = text.encode_wide().collect();
    if wide.contains(&0) {
        return Err(invalid("a string holds a NUL"));
    }
    Ok(wide)
}

/// The NUL-terminated application name of `program`, a path that is never
/// searched for.
///
/// # Errors
///
/// `InvalidInput` for a NUL, a name with no path separator (it would be looked up
/// in the current directory), a `"` (the command line quotes the name verbatim),
/// or a `.bat` or `.cmd` (cmd.exe reads its arguments by rules of its own,
/// and a helper is an executable).
pub(super) fn application_name(program: &OsStr) -> Result<Vec<u16>> {
    let mut name = wide(program)?;
    if !name.iter().any(|&u| u == BACKSLASH || u == u16::from(b'/')) {
        return Err(invalid("the program has no path"));
    }
    if name.contains(&QUOTE) {
        return Err(invalid("the program holds a quote"));
    }
    let lower = String::from_utf16_lossy(&name).to_ascii_lowercase();
    if lower.ends_with(".bat") || lower.ends_with(".cmd") {
        return Err(invalid("the program is a batch file"));
    }
    name.push(0);
    Ok(name)
}

/// The NUL-terminated command line of `program` (quoted verbatim, as `argv[0]`
/// takes no escapes) and `args`, each quoted so that `CommandLineToArgvW` and
/// the C runtime read back the argument as it is given.
///
/// An argument is bare when it is not empty and holds no space, tab, newline,
/// vertical tab or quote. Otherwise it is wrapped in quotes, every run of
/// backslashes before a quote (or before the closing quote) is doubled, and a
/// quote is written `\"`.
///
/// # Errors
///
/// `InvalidInput` for a NUL anywhere, or a line past the 32,767 units
/// `CreateProcessW` takes.
pub(super) fn command_line<'a>(
    program: &OsStr,
    args: impl IntoIterator<Item = &'a OsStr>,
) -> Result<Vec<u16>> {
    let mut line = vec![QUOTE];
    line.extend(wide(program)?);
    line.push(QUOTE);
    for arg in args {
        line.push(u16::from(b' '));
        push_argument(&mut line, &wide(arg)?);
    }
    line.push(0);
    if line.len() > LONGEST {
        return Err(invalid("the command line is too long"));
    }
    Ok(line)
}

fn push_argument(line: &mut Vec<u16>, arg: &[u16]) {
    let bare = !arg.is_empty()
        && !arg
            .iter()
            .any(|&u| matches!(u, 0x20 | 0x09 | 0x0A | 0x0B) || u == QUOTE);
    if bare {
        line.extend_from_slice(arg);
        return;
    }
    line.push(QUOTE);
    let mut backslashes = 0;
    for &unit in arg {
        match unit {
            BACKSLASH => {
                backslashes += 1;
                line.push(BACKSLASH);
            }
            QUOTE => {
                line.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
                line.push(QUOTE);
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                line.push(unit);
            }
        }
    }
    line.extend(std::iter::repeat_n(BACKSLASH, backslashes));
    line.push(QUOTE);
}
