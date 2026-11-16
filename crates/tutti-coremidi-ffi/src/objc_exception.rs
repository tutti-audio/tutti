//! The boundary that keeps an Objective-C exception out of Rust.
//!
//! An exception that unwinds into Rust aborts the process ("Rust cannot catch
//! foreign exceptions"). [`catch`] runs a closure of message sends and returns
//! what it raised as an [`ObjcException`]; every Objective-C send in this crate
//! goes through it.

use objc2::exception;
use objc2_foundation::NSException;
use std::panic::AssertUnwindSafe;

/// An Objective-C exception raised by a CoreMIDI call.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Objective-C exception {name}: {reason}")]
pub struct ObjcException {
    /// The exception's `name`, or its class name when it is not an `NSException`.
    pub name: String,
    /// The exception's `reason`, empty when it has none.
    pub reason: String,
}

/// Runs `f` and returns its value, or the Objective-C exception it raised.
pub(crate) fn catch<R>(f: impl FnOnce() -> R) -> Result<R, ObjcException> {
    exception::catch(AssertUnwindSafe(f)).map_err(|raised| match raised {
        None => ObjcException {
            name: String::from("nil"),
            reason: String::new(),
        },
        Some(object) => match object.downcast::<NSException>() {
            Ok(exception) => ObjcException {
                name: exception.name().to_string(),
                reason: exception
                    .reason()
                    .map(|r| r.to_string())
                    .unwrap_or_default(),
            },
            Err(other) => ObjcException {
                name: other.class().name().to_string_lossy().into_owned(),
                reason: String::new(),
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSString;

    /// A raised `NSException` comes back as an error carrying its name and
    /// reason, and a closure that raises nothing returns its value.
    #[test]
    fn a_raised_exception_is_returned_with_its_name_and_reason() {
        assert_eq!(catch(|| 7), Ok(7));
        let raised = catch(|| {
            let exception = NSException::exceptionWithName_reason_userInfo(
                &NSString::from_str("TuttiTest"),
                Some(&NSString::from_str("because")),
                None,
            );
            exception::throw(exception)
        });
        assert_eq!(
            raised,
            Err(ObjcException {
                name: "TuttiTest".into(),
                reason: "because".into()
            })
        );
    }
}
