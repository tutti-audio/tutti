//! Taking a source or destination by its `UniqueId`: the checks that the object
//! CoreMIDI returns is the one asked for.
//!
//! An identity is `kMIDIPropertyUniqueID`, never a list position: a list shifts
//! when a device comes or goes, so a position read at one moment names another
//! object at the next.

use crate::checks::unique_id::UniqueId;

/// The `MIDIObjectType` of a source: `kMIDIObjectType_Source` of `MIDIServices.h`.
pub(crate) const OBJECT_TYPE_SOURCE: i32 = 2;
/// The `MIDIObjectType` of a destination: `kMIDIObjectType_Destination` of
/// `MIDIServices.h`.
pub(crate) const OBJECT_TYPE_DESTINATION: i32 = 3;

/// An endpoint was not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum FindError {
    /// No endpoint has this unique id: it was removed, or never existed.
    #[error("no CoreMIDI endpoint has unique id {0}")]
    Missing(UniqueId),
    /// The unique id names an object that is not a source (or destination) as asked.
    #[error("CoreMIDI object {0} is not the kind of endpoint asked for")]
    WrongKind(UniqueId),
    /// The object taken reads a different unique id than the one asked for (or
    /// none): the list changed between the listing and the take.
    #[error("the CoreMIDI object taken for unique id {wanted} reads {found:?}")]
    Changed {
        /// The id asked for.
        wanted: UniqueId,
        /// The id the object reads now.
        found: Option<UniqueId>,
    },
}

/// Checks an object CoreMIDI returned for `wanted`: it has the `MIDIObjectType`
/// `expected` and reads `wanted` as its unique id.
pub(crate) fn confirm(
    wanted: UniqueId,
    expected: i32,
    object_type: i32,
    found: Option<UniqueId>,
) -> Result<(), FindError> {
    if object_type != expected {
        return Err(FindError::WrongKind(wanted));
    }
    if found != Some(wanted) {
        return Err(FindError::Changed { wanted, found });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WANTED: UniqueId = UniqueId::new(1234);

    /// `kMIDIObjectType_Source` is 2 and `kMIDIObjectType_Destination` is 3.
    #[test]
    fn an_object_of_the_kind_asked_for_with_the_id_asked_for_is_taken() {
        assert_eq!(confirm(WANTED, OBJECT_TYPE_SOURCE, 2, Some(WANTED)), Ok(()));
        assert_eq!(
            confirm(WANTED, OBJECT_TYPE_DESTINATION, 3, Some(WANTED)),
            Ok(())
        );
    }

    /// A list shift between listing and take leaves the object with another id:
    /// the take fails and names both.
    #[test]
    fn an_object_with_another_id_is_refused() {
        let other = UniqueId::new(-77);
        assert_eq!(
            confirm(WANTED, OBJECT_TYPE_SOURCE, 2, Some(other)),
            Err(FindError::Changed {
                wanted: WANTED,
                found: Some(other)
            })
        );
        assert_eq!(
            confirm(WANTED, OBJECT_TYPE_DESTINATION, 3, None),
            Err(FindError::Changed {
                wanted: WANTED,
                found: None
            })
        );
    }

    /// A source is not a destination, nor a device, entity or external endpoint.
    #[test]
    fn an_object_of_another_kind_is_refused() {
        for object_type in [-1, 0, 1, 3, 16, 17, 18, 19] {
            assert_eq!(
                confirm(WANTED, OBJECT_TYPE_SOURCE, object_type, Some(WANTED)),
                Err(FindError::WrongKind(WANTED)),
                "type {object_type}"
            );
        }
        assert_eq!(
            confirm(WANTED, OBJECT_TYPE_DESTINATION, 2, Some(WANTED)),
            Err(FindError::WrongKind(WANTED))
        );
    }
}
