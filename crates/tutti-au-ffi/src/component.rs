//! Components in the system registry.

use std::num::NonZeroU32;

use coreaudio_sys as sys;

use crate::cf::CfString;
use crate::error::{OsError, check};

/// The registry's key of one Audio Unit: its type, subtype and manufacturer,
/// each a nonzero code.
///
/// A description with a zero field is a wildcard that matches many components,
/// so it is no key: [`new`](Self::new) refuses it, and
/// [`ComponentRef::find`] takes only a key. A search over a wildcard is
/// [`ComponentRef::all_matching`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentKey {
    component_type: NonZeroU32,
    sub_type: NonZeroU32,
    manufacturer: NonZeroU32,
}

impl ComponentKey {
    /// The key of the unit with these four-char codes, or `None` when any is
    /// zero (a wildcard).
    #[must_use]
    pub fn new(component_type: u32, sub_type: u32, manufacturer: u32) -> Option<Self> {
        Some(Self {
            component_type: NonZeroU32::new(component_type)?,
            sub_type: NonZeroU32::new(sub_type)?,
            manufacturer: NonZeroU32::new(manufacturer)?,
        })
    }

    /// The key a description names, or `None` when one of its type, subtype or
    /// manufacturer is zero. Its flags and mask are not part of the key.
    #[must_use]
    pub fn from_description(desc: &sys::AudioComponentDescription) -> Option<Self> {
        Self::new(
            desc.componentType,
            desc.componentSubType,
            desc.componentManufacturer,
        )
    }

    /// The `componentType` code.
    #[must_use]
    pub fn component_type(self) -> u32 {
        self.component_type.get()
    }

    /// The `componentSubType` code.
    #[must_use]
    pub fn sub_type(self) -> u32 {
        self.sub_type.get()
    }

    /// The `componentManufacturer` code.
    #[must_use]
    pub fn manufacturer(self) -> u32 {
        self.manufacturer.get()
    }

    /// The description that matches this key, with no flag filter.
    fn description(self) -> sys::AudioComponentDescription {
        sys::AudioComponentDescription {
            componentType: self.component_type(),
            componentSubType: self.sub_type(),
            componentManufacturer: self.manufacturer(),
            componentFlags: 0,
            componentFlagsMask: 0,
        }
    }
}

/// No Audio Unit with this key is registered with the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "no Audio Unit is registered with type {:#010x}, subtype {:#010x}, manufacturer {:#010x}",
    .0.component_type(), .0.sub_type(), .0.manufacturer()
)]
pub struct NotRegistered(pub ComponentKey);

/// An `AudioComponent` the registry holds: one installed Audio Unit.
///
/// The only way to get one is to ask the registry ([`ComponentRef::find`],
/// [`ComponentRef::all_matching`]), so it always came from
/// `AudioComponentFindNext`. It is `Copy` and names the component, not an
/// instance; [`Unit::new`](crate::Unit::new) makes the instance.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentRef(sys::AudioComponent);

impl std::fmt::Debug for ComponentRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ComponentRef").field(&self.0).finish()
    }
}

// SAFETY: an `AudioComponent` is an opaque pointer to a record of the
// process-wide component registry. The registry owns the record, keeps it for
// the life of the process (a component is never freed, only an instance of
// one), and AudioToolbox documents its lookup and description calls as
// callable from any thread. The value is only ever passed back to those calls.
unsafe impl Send for ComponentRef {}
// SAFETY: as above; the value is immutable and every call that takes it reads
// the registry through AudioToolbox's own locking.
unsafe impl Sync for ComponentRef {}

impl ComponentRef {
    /// The next registered component matching `desc` after `after` (the first
    /// when `after` is `None`), or `None` at the end of the list. A zero field
    /// of `desc` matches anything.
    fn find_next(
        after: Option<ComponentRef>,
        desc: &sys::AudioComponentDescription,
    ) -> Option<ComponentRef> {
        let after = after.map_or(std::ptr::null_mut(), |c| c.0);
        // SAFETY: `after` is null or a component the registry gave us, and
        // `desc` is a live description for the call.
        let next = unsafe { sys::AudioComponentFindNext(after, desc) };
        (!next.is_null()).then_some(ComponentRef(next))
    }

    /// The registered component with exactly this key.
    ///
    /// # Errors
    /// [`NotRegistered`] when no installed Audio Unit has the key.
    pub fn find(key: ComponentKey) -> Result<ComponentRef, NotRegistered> {
        Self::find_next(None, &key.description()).ok_or(NotRegistered(key))
    }

    /// Every registered component matching `desc`, in registry order. A zero
    /// field of `desc` matches anything, so a search over a type or a
    /// manufacturer is this, filtered by the caller.
    pub fn all_matching(desc: &sys::AudioComponentDescription) -> Vec<ComponentRef> {
        let mut out = Vec::new();
        let mut at = None;
        while let Some(next) = Self::find_next(at, desc) {
            out.push(next);
            at = Some(next);
        }
        out
    }

    /// The component's description: its type, subtype, manufacturer and flags.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioComponentGetDescription`.
    pub fn description(self) -> Result<sys::AudioComponentDescription, OsError> {
        let mut desc = sys::AudioComponentDescription::default();
        // SAFETY: `self.0` is a registry component and `desc` is writable.
        let status = unsafe { sys::AudioComponentGetDescription(self.0, &mut desc) };
        check("AudioComponentGetDescription", status)?;
        Ok(desc)
    }

    /// The component's display name, or `None` when the registry has none.
    pub fn name(self) -> Option<String> {
        let mut name: sys::CFStringRef = std::ptr::null();
        // SAFETY: `self.0` is a registry component and `name` is writable.
        let status = unsafe { sys::AudioComponentCopyName(self.0, &mut name) };
        check("AudioComponentCopyName", status).ok()?;
        // SAFETY: `AudioComponentCopyName` follows the Copy rule: on success
        // `name` is null or a CFString we own one retain of.
        unsafe { CfString::from_copied(name) }.map(|s| s.to_string())
    }

    /// The component's version word (`major << 16 | minor << 8 | bugfix`).
    pub fn version(self) -> Option<u32> {
        let mut raw: u32 = 0;
        // SAFETY: `self.0` is a registry component and `raw` is writable.
        let status = unsafe { sys::AudioComponentGetVersion(self.0, &mut raw) };
        check("AudioComponentGetVersion", status).ok().map(|()| raw)
    }

    /// The raw `AudioComponent`, for a call this crate does not wrap. Using the
    /// pointer is the caller's `unsafe`.
    pub fn as_raw(self) -> sys::AudioComponent {
        self.0
    }

    pub(crate) fn raw(self) -> sys::AudioComponent {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EFFECT: u32 = sys::kAudioUnitType_Effect;
    const DELAY: u32 = u32::from_be_bytes(*b"dely");
    const APPLE: u32 = u32::from_be_bytes(*b"appl");

    /// A description with a zero type, subtype or manufacturer is a wildcard
    /// and names no key.
    #[test]
    fn a_zero_code_is_no_key() {
        assert!(ComponentKey::new(0, DELAY, APPLE).is_none());
        assert!(ComponentKey::new(EFFECT, 0, APPLE).is_none());
        assert!(ComponentKey::new(EFFECT, DELAY, 0).is_none());
        let key = ComponentKey::new(EFFECT, DELAY, APPLE).expect("three codes");
        assert_eq!(
            (key.component_type(), key.sub_type(), key.manufacturer()),
            (EFFECT, DELAY, APPLE)
        );
    }

    /// An exact key finds the one component with that triple; an unregistered
    /// key is `NotRegistered`, naming the key.
    #[test]
    fn an_exact_key_finds_its_component_and_an_absent_one_is_named() {
        let key = ComponentKey::new(EFFECT, DELAY, APPLE).expect("three codes");
        let found = ComponentRef::find(key).expect("AUDelay ships with macOS");
        let desc = found.description().expect("a registered component");
        assert_eq!(ComponentKey::from_description(&desc), Some(key));

        let absent =
            ComponentKey::new(EFFECT, u32::from_be_bytes(*b"zzzz"), APPLE).expect("three codes");
        assert_eq!(ComponentRef::find(absent), Err(NotRegistered(absent)));
    }

    /// A wildcard search is `all_matching`, which lists every match and so
    /// includes the exact one.
    #[test]
    fn a_wildcard_search_lists_every_match() {
        let wildcard = sys::AudioComponentDescription {
            componentType: EFFECT,
            ..Default::default()
        };
        let all = ComponentRef::all_matching(&wildcard);
        let key = ComponentKey::new(EFFECT, DELAY, APPLE).expect("three codes");
        assert!(all.len() > 1, "more than one effect is installed");
        assert!(all.contains(&ComponentRef::find(key).expect("AUDelay")));
    }
}
