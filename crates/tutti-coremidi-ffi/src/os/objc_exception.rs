//! The boundary that keeps an Objective-C exception out of Rust.
//!
//! An exception that unwinds into Rust aborts the process ("Rust cannot catch
//! foreign exceptions"). [`catch`] runs a closure of message sends and returns
//! what it raised as an [`ObjcException`]; every Objective-C send in this crate
//! goes through it.

use objc2::exception;
use objc2::rc::Retained;
use objc2::sel;
use objc2_foundation::NSException;
use std::panic::AssertUnwindSafe;

/// An Objective-C exception raised by a CoreMIDI call.
///
/// Catching one leaks: unwinding skips the releases of the objects that the
/// Objective-C and CoreMIDI frames (not Rust's) between the `@throw` and the
/// catch retained, and `objc2`'s `exception::catch` does not give them back.
/// Each catch costs that small, unrecoverable leak.
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
        Some(object) => describe(object),
    })
}

/// The name and reason of a raised object. Each send runs inside its own
/// `exception::catch`, so reading an exception cannot raise another one out;
/// the class name and an empty reason stand in when it does.
fn describe(object: Retained<exception::Exception>) -> ObjcException {
    let class_name = object.class().name().to_string_lossy().into_owned();
    // `isKindOfClass:` is NSObject's; an object that does not answer it is not
    // an `NSException`.
    let answers = exception::catch(AssertUnwindSafe(|| {
        object.class().responds_to(sel!(isKindOfClass:))
    }))
    .unwrap_or(false);
    if !answers {
        return ObjcException {
            name: class_name,
            reason: String::new(),
        };
    }
    let read = exception::catch(AssertUnwindSafe(|| {
        let exception = object.downcast::<NSException>().ok()?;
        let reason = exception.reason().map(|r| r.to_string());
        Some((exception.name().to_string(), reason.unwrap_or_default()))
    }));
    match read {
        Ok(Some((name, reason))) => ObjcException { name, reason },
        _ => ObjcException {
            name: class_name,
            reason: String::new(),
        },
    }
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
        let raised = catch::<()>(|| {
            NSException::new(
                &NSString::from_str("TuttiTest"),
                Some(&NSString::from_str("because")),
                None,
            )
            .expect("an NSException is created")
            .raise()
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
