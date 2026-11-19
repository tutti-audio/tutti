//! Elsewhere: the protection is the platform's own (macOS's hardened runtime
//! is the app's signing) or has no such flag.

/// Does nothing on this platform.
///
/// # Errors
///
/// Never.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the signature of the Linux module"
)]
pub fn protect_host_memory() -> std::io::Result<()> {
    Ok(())
}
