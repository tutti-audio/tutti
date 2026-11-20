//! The macOS release a test runs on.

use std::process::Command;

/// The major version of the macOS the process runs on (15 for macOS 15.x), from
/// `sw_vers -productVersion`; `None` off macOS, where the command does not exist.
///
/// A test whose result depends on the release (a class that exists from macOS 15)
/// asserts both sides of it from this number, so no side passes by returning early.
#[must_use]
pub fn macos_major() -> Option<u32> {
    let out = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    String::from_utf8(out.stdout)
        .ok()?
        .trim()
        .split('.')
        .next()?
        .parse()
        .ok()
}
