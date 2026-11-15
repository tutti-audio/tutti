//! An alsa-lib failure: the call and its code.

use std::ffi::CStr;

/// What alsa-lib did not do: a call that returned a negative code, an id that
/// is not an address, or a record it said it made and did not.
///
/// ALSA reports failure as a negated `errno`, so the sign carries the meaning
/// and the magnitude is the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// An alsa-lib call returned a negative code.
    #[error("ALSA error: {operation} ({code})")]
    Call {
        /// The alsa-lib function that failed, as a static label.
        operation: &'static str,
        /// alsa-lib's negative return.
        code: i32,
    },
    /// A client, port or queue id from alsa-lib that is no address byte, which
    /// is what a sequencer address holds.
    #[error("ALSA {what} {id} is not a sequencer address byte")]
    Address {
        /// What the id names (`"client"`, `"port"`, `"queue"`).
        what: &'static str,
        /// The id alsa-lib gave.
        id: i32,
        /// Why it does not fit a byte.
        #[source]
        source: std::num::TryFromIntError,
    },
    /// An allocation alsa-lib reported as made returned no record.
    #[error("ALSA error: {operation} made no record")]
    NoRecord {
        /// The alsa-lib function that returned the null.
        operation: &'static str,
    },
    /// The sequencer did not give a queue's real time.
    #[error("ALSA error: the queue's status has no real time")]
    NoQueueTime,
}

/// What this crate's calls return.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// `snd_strerror` for the code of a failed call: "No such file or
    /// directory" beats `-2` in a log. The other failures have no code and say
    /// themselves.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Call { code, .. } => message(*code),
            other => other.to_string(),
        }
    }
}

/// The address byte of the client, port or queue `id` that `what` names.
///
/// # Errors
///
/// [`Error::Address`] when `id` does not fit a byte.
pub fn address_byte(what: &'static str, id: i32) -> Result<u8> {
    u8::try_from(id).map_err(|source| Error::Address { what, id, source })
}

/// `snd_strerror` as an owned string.
pub(crate) fn message(code: i32) -> String {
    // SAFETY: `snd_strerror` takes any int and returns a static NUL-terminated
    // string (or NULL, handled below).
    let ptr = unsafe { crate::sys::snd_strerror(code) };
    if ptr.is_null() {
        return format!("error {code}");
    }
    // SAFETY: non-NULL, NUL-terminated and static, as above; copied at once.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// A return code as a result: failure is negative, anything else is the value.
pub(crate) fn check(code: i32, operation: &'static str) -> Result<i32> {
    if code < 0 {
        Err(Error::Call { operation, code })
    } else {
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_negative_code_is_the_failure_and_zero_is_a_value() {
        assert_eq!(
            check(-2, "op"),
            Err(Error::Call {
                operation: "op",
                code: -2
            })
        );
        assert_eq!(check(0, "op"), Ok(0));
        assert_eq!(check(7, "op"), Ok(7));
    }

    #[test]
    fn the_message_is_the_librarys() {
        assert!(message(-2).contains("No such file"));
    }

    #[test]
    fn an_id_past_a_byte_is_refused_with_why() {
        use std::error::Error as _;
        assert_eq!(address_byte("port", 254), Ok(254));
        let refused = address_byte("client", 256).unwrap_err();
        assert!(matches!(refused, Error::Address { id: 256, .. }));
        assert!(refused.source().is_some(), "the integer error is kept");
        assert_eq!(
            address_byte("port", -1).unwrap_err().to_string(),
            "ALSA port -1 is not a sequencer address byte"
        );
    }
}
