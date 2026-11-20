//! The sources and destinations CoreMIDI lists, taken by identity.
//!
//! A listing takes each endpoint's object reference once and reads everything about
//! it from that reference: its `UniqueId`, name and protocol are one object's, never
//! a position's read twice. Nothing here returns or takes a list position, because
//! the list shifts when a device comes or goes. An endpoint is taken again by its
//! `UniqueId` (`MIDIObjectFindByUniqueID`), and the object returned is checked to be
//! the kind asked for and to read that id.

use crate::UniqueId;
use crate::find::{FindError, Kind, confirm};
use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use coremidi_sys::{
    ItemCount, MIDIEndpointRef, MIDIGetDestination, MIDIGetNumberOfDestinations,
    MIDIGetNumberOfSources, MIDIGetSource, MIDIObjectFindByUniqueID, MIDIObjectGetIntegerProperty,
    MIDIObjectGetStringProperty, MIDIObjectRef, MIDIObjectType, kMIDIPropertyDisplayName,
    kMIDIPropertyName, kMIDIPropertyProtocolID, kMIDIPropertyUniqueID,
};
use std::ptr;

impl UniqueId {
    /// The id of `object`, `None` for the null reference (an endpoint with no
    /// source or no destination) or an object that has none.
    pub(crate) fn of(object: MIDIObjectRef) -> Option<Self> {
        if object == 0 {
            return None;
        }
        let mut id: i32 = 0;
        // SAFETY: `kMIDIPropertyUniqueID` is a constant CFString CoreMIDI exports,
        // and `id` is a valid out-pointer for the call. A stale reference makes the
        // call return an error status, which reads as `None`.
        let status =
            unsafe { MIDIObjectGetIntegerProperty(object, kMIDIPropertyUniqueID, &mut id) };
        (status == 0).then_some(Self::new(id))
    }
}

/// A source or destination CoreMIDI listed or was asked for by id.
///
/// It owns its object reference privately and has no list position. The
/// reference is CoreMIDI's, valid until the endpoint is removed; a read after
/// that returns `None`, or the value of another object if CoreMIDI reuses the
/// reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    reference: MIDIEndpointRef,
    id: UniqueId,
}

impl Endpoint {
    /// The endpoint at `reference`, with the id it reads; `None` for the null
    /// reference or an object with no id.
    fn of(reference: MIDIEndpointRef) -> Option<Self> {
        Some(Self {
            reference,
            id: UniqueId::of(reference)?,
        })
    }

    /// The endpoint's object reference, for a call that takes an endpoint.
    pub(crate) fn reference(&self) -> MIDIEndpointRef {
        self.reference
    }

    /// Its `kMIDIPropertyUniqueID`, read when it was listed or taken.
    #[must_use]
    pub fn id(&self) -> UniqueId {
        self.id
    }

    /// What a user sees in Audio MIDI Setup (`kMIDIPropertyDisplayName`), else the
    /// bare endpoint name (`kMIDIPropertyName`); `None` when neither is a
    /// non-empty string.
    #[must_use]
    pub fn name(&self) -> Option<String> {
        // SAFETY: the property names are constant CFStrings CoreMIDI exports.
        let (display, name) = unsafe { (kMIDIPropertyDisplayName, kMIDIPropertyName) };
        string_property(self.reference, display).or_else(|| string_property(self.reference, name))
    }

    /// `kMIDIPropertyProtocolID`: 1 for `kMIDIProtocol_1_0`, 2 for
    /// `kMIDIProtocol_2_0`; `None` when the endpoint does not report one.
    #[must_use]
    pub fn protocol_id(&self) -> Option<i32> {
        let mut value: i32 = 0;
        // SAFETY: `kMIDIPropertyProtocolID` is a constant CFString CoreMIDI
        // exports and `value` is a valid out-pointer; a stale reference or an
        // absent property returns an error status, which reads as `None`.
        let status = unsafe {
            MIDIObjectGetIntegerProperty(self.reference, kMIDIPropertyProtocolID, &mut value)
        };
        (status == 0).then_some(value)
    }
}

/// `object`'s string property `property`, `None` when it has none or it is empty.
fn string_property(object: MIDIObjectRef, property: CFStringRef) -> Option<String> {
    let mut value: CFStringRef = ptr::null();
    // SAFETY: `property` is a CFString CoreMIDI exports and `value` is a valid
    // out-pointer. A stale reference or an absent property returns an error
    // status and leaves `value` null.
    let status = unsafe { MIDIObjectGetStringProperty(object, property, &mut value) };
    if status != 0 || value.is_null() {
        return None;
    }
    // SAFETY: the call returned a CFString it created for the caller (CoreMIDI's
    // `Get...Property` functions follow the Create rule), which this takes over
    // and releases once.
    let text = unsafe { CFString::wrap_under_create_rule(value) }.to_string();
    (!text.is_empty()).then_some(text)
}

/// The sources CoreMIDI lists now. A device that comes or goes while this runs
/// may be missing from the list, never paired with another's id.
#[must_use]
pub fn sources() -> Vec<Endpoint> {
    // SAFETY: a plain query.
    let count = unsafe { MIDIGetNumberOfSources() };
    (0..count)
        .filter_map(|index: ItemCount| {
            // SAFETY: a plain query; an index past the list (it shrank) returns
            // 0, the null reference, which `Endpoint::of` reads as `None`.
            let reference = unsafe { MIDIGetSource(index) };
            Endpoint::of(reference)
        })
        .collect()
}

/// The destinations CoreMIDI lists now, as [`sources`] lists sources.
#[must_use]
pub fn destinations() -> Vec<Endpoint> {
    // SAFETY: a plain query.
    let count = unsafe { MIDIGetNumberOfDestinations() };
    (0..count)
        .filter_map(|index: ItemCount| {
            // SAFETY: as in `sources`.
            let reference = unsafe { MIDIGetDestination(index) };
            Endpoint::of(reference)
        })
        .collect()
}

/// The source with unique id `id`.
///
/// # Errors
///
/// [`FindError::Missing`] when no object has the id, [`FindError::WrongKind`] when
/// it is not a source, and [`FindError::Changed`] when the object reads another id.
pub fn find_source(id: UniqueId) -> Result<Endpoint, FindError> {
    find(id, Kind::Source)
}

/// The destination with unique id `id`.
///
/// # Errors
///
/// As [`find_source`], for a destination.
pub fn find_destination(id: UniqueId) -> Result<Endpoint, FindError> {
    find(id, Kind::Destination)
}

fn find(id: UniqueId, kind: Kind) -> Result<Endpoint, FindError> {
    let mut object: MIDIObjectRef = 0;
    let mut object_type: MIDIObjectType = 0;
    // SAFETY: `object` and `object_type` are valid out-pointers; an id no object
    // has returns an error status, which is `Missing`.
    let status = unsafe { MIDIObjectFindByUniqueID(id.get(), &mut object, &mut object_type) };
    if status != 0 || object == 0 {
        return Err(FindError::Missing(id));
    }
    confirm(id, kind, object_type, UniqueId::of(object))?;
    Ok(Endpoint {
        reference: object,
        id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The null reference has no id, so no endpoint is made of it (a listing never
    /// yields it), and a take that finds no object (`find` gives `Missing`) never
    /// reaches the id check.
    #[test]
    fn the_null_reference_has_no_endpoint() {
        assert_eq!(Endpoint::of(0), None);
        assert_eq!(UniqueId::of(0), None);
    }

    /// An id no endpoint has is `Missing`, never another device.
    #[test]
    fn an_id_no_endpoint_has_is_missing() {
        let taken: Vec<_> = sources()
            .iter()
            .chain(&destinations())
            .map(Endpoint::id)
            .collect();
        let id = (i32::MIN..)
            .map(UniqueId::new)
            .find(|id| !taken.contains(id))
            .expect("an unused id");
        assert_eq!(find_source(id), Err(FindError::Missing(id)));
        assert_eq!(find_destination(id), Err(FindError::Missing(id)));
    }

    /// Every endpoint listed is taken again by its id as the same endpoint, and
    /// as the kind it was listed as: a source is not a destination.
    #[test]
    fn a_listed_endpoint_is_taken_again_by_its_id() {
        for source in sources() {
            assert_eq!(find_source(source.id()), Ok(source));
            assert_eq!(
                find_destination(source.id()),
                Err(FindError::WrongKind(source.id()))
            );
        }
        for destination in destinations() {
            assert_eq!(find_destination(destination.id()), Ok(destination));
            assert_eq!(
                find_source(destination.id()),
                Err(FindError::WrongKind(destination.id()))
            );
        }
    }
}
