//! The names of Resources: the standard ones, the manufacturer-specific ones,
//! and what each standard one declares by default.

use std::fmt;

use super::super::limits::ResourceName;
use super::fields::UnlistedText;

/// What a ResourceList entry declares for a Resource when it names nothing
/// (M2-103 §14.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceDefaults {
    /// Whether the Resource can be read with Get.
    pub can_get: bool,
    /// What Set does with the Resource.
    pub can_set: CanSet,
    /// Whether the Resource can be subscribed to.
    pub can_subscribe: bool,
    /// Whether the Resource, a List Resource, answers `offset` and `limit`.
    pub can_paginate: bool,
    /// Whether every inquiry names an entry of the Resource with `resId`.
    pub require_res_id: bool,
}

/// What Set Property Data may do to a Resource (M2-103 §14.2, Table 79).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CanSet {
    /// `none`: the Resource is read only.
    None,
    /// `full`: the whole Property Data can be set.
    Full,
    /// `partial`: the whole Property Data or a part of it can be set.
    Partial,
}

impl CanSet {
    /// The property value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Full => "full",
            Self::Partial => "partial",
        }
    }

    /// The setting a property value names, or `None` for other text.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::try_from(text).ok()
    }
}

impl TryFrom<&str> for CanSet {
    type Error = UnlistedText;

    fn try_from(text: &str) -> Result<Self, UnlistedText> {
        [Self::None, Self::Full, Self::Partial]
            .into_iter()
            .find(|c| c.as_str() == text)
            .ok_or(UnlistedText {
                allowed: &["none", "full", "partial"],
            })
    }
}

/// The name of a manufacturer-specific Resource: `X-` and alphanumeric
/// characters, 36 in all (M2-103 §8.4, Table 13).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CustomResource(ResourceName);

impl CustomResource {
    /// The Resource `text` names, or `None` unless it is a valid name that
    /// starts `X-`.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        text.starts_with("X-")
            .then(|| ResourceName::new(text).map(Self))
            .flatten()
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// A Resource a specification defines and this crate has a type for: the
/// Resources of M2-103 and its companions. Its entry declares nothing of
/// `canGet`, `canSet`, `requireResId`, `canPaginate` or `schema`: they are the
/// defaults of the Resource (M2-103 §14.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StandardResource {
    /// `ResourceList`: the Resources a device has (M2-103 §14).
    ResourceList,
    /// `DeviceInfo`: what the device is (M2-105 §3).
    DeviceInfo,
    /// `ChannelList`: the Channels in use (M2-105 §4).
    ChannelList,
    /// JSONSchema: a JSON Schema a ResourceList entry refers to (M2-105 §5).
    JsonSchema,
    /// `ModeList`: the Modes of the device (M2-106 §2).
    ModeList,
    /// `CurrentMode`: the Mode in use (M2-106 §3).
    CurrentMode,
    /// `ChannelMode`: the Omni and Poly/Mono mode of a one-channel device
    /// (M2-108 §2).
    ChannelMode,
    /// `BasicChannelRx`: the Channel received on (M2-108 §3).
    BasicChannelRx,
    /// `BasicChannelTx`: the Channel sent on (M2-108 §4).
    BasicChannelTx,
    /// `ProgramList`: the Programs of a Program Collection (M2-107 §2).
    ProgramList,
    /// `LocalOn`: whether the local input drives the tone generator (M2-109 §2).
    LocalOn,
    /// `ExternalSync`: whether the clock follows external MIDI sync (M2-112 §2).
    ExternalSync,
    /// `AllCtrlList`: the controllers of every Channel (M2-117 §2).
    AllCtrlList,
    /// `ChCtrlList`: the controllers of one Channel of the ChannelList (M2-117 §3).
    ChCtrlList,
    /// `CtrlMapList`: the named values of a controller (M2-117 §4).
    CtrlMapList,
    /// `StateList`: the states of the device (M2-111 §2).
    StateList,
    /// `State`: one state of the device, to save and to restore (M2-111 §3).
    State,
}

impl StandardResource {
    const ALL: [Self; 17] = [
        Self::ResourceList,
        Self::DeviceInfo,
        Self::ChannelList,
        Self::JsonSchema,
        Self::ModeList,
        Self::CurrentMode,
        Self::ChannelMode,
        Self::BasicChannelRx,
        Self::BasicChannelTx,
        Self::ProgramList,
        Self::LocalOn,
        Self::ExternalSync,
        Self::AllCtrlList,
        Self::ChCtrlList,
        Self::CtrlMapList,
        Self::StateList,
        Self::State,
    ];

    /// The Resource a name on the wire stands for, or `None` for any other
    /// name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == name)
    }

    /// The name as the wire writes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ResourceList => "ResourceList",
            Self::DeviceInfo => "DeviceInfo",
            Self::ChannelList => "ChannelList",
            Self::JsonSchema => "JSONSchema",
            Self::ModeList => "ModeList",
            Self::CurrentMode => "CurrentMode",
            Self::ChannelMode => "ChannelMode",
            Self::BasicChannelRx => "BasicChannelRx",
            Self::BasicChannelTx => "BasicChannelTx",
            Self::ProgramList => "ProgramList",
            Self::LocalOn => "LocalOn",
            Self::ExternalSync => "ExternalSync",
            Self::AllCtrlList => "AllCtrlList",
            Self::ChCtrlList => "ChCtrlList",
            Self::CtrlMapList => "CtrlMapList",
            Self::StateList => "StateList",
            Self::State => "State",
        }
    }

    /// Whether the Resource is a List Resource: a name that ends `List`
    /// (M2-103 §8.6).
    #[must_use]
    pub fn is_list(self) -> bool {
        self.as_str().as_bytes().ends_with(b"List")
    }

    /// How an inquiry for the Resource names an entry. `ProgramList`,
    /// `ChCtrlList`, `CtrlMapList`, JSONSchema and `State` each have a set of
    /// entries and so need a `resId`; the others have none.
    #[must_use]
    pub const fn requires_res_id(self) -> bool {
        match self {
            Self::ProgramList
            | Self::ChCtrlList
            | Self::CtrlMapList
            | Self::JsonSchema
            | Self::State => true,
            Self::ResourceList
            | Self::DeviceInfo
            | Self::ChannelList
            | Self::ModeList
            | Self::CurrentMode
            | Self::ChannelMode
            | Self::BasicChannelRx
            | Self::BasicChannelTx
            | Self::LocalOn
            | Self::ExternalSync
            | Self::AllCtrlList
            | Self::StateList => false,
        }
    }

    /// The name as a header carries it.
    #[must_use]
    pub fn name(self) -> ResourceName {
        // Every standard name is a valid name; a test holds it.
        ResourceName::new(self.as_str())
            .unwrap_or_else(|| unreachable!("{self} is a resource name"))
    }

    /// What a ResourceList entry says when it names nothing.
    #[must_use]
    pub const fn defaults(self) -> ResourceDefaults {
        let base = ResourceDefaults {
            can_get: true,
            can_set: CanSet::None,
            can_subscribe: false,
            can_paginate: false,
            require_res_id: self.requires_res_id(),
        };
        match self {
            Self::State
            | Self::CurrentMode
            | Self::BasicChannelRx
            | Self::BasicChannelTx
            | Self::LocalOn
            | Self::ExternalSync => ResourceDefaults {
                can_set: CanSet::Full,
                ..base
            },
            Self::ProgramList => ResourceDefaults {
                can_paginate: true,
                ..base
            },
            Self::ResourceList
            | Self::DeviceInfo
            | Self::ChannelList
            | Self::JsonSchema
            | Self::ModeList
            | Self::ChannelMode
            | Self::AllCtrlList
            | Self::ChCtrlList
            | Self::CtrlMapList
            | Self::StateList => base,
        }
    }
}

impl fmt::Display for StandardResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The name of a Resource no AMEI/MMA specification this crate models defines
/// and that is not manufacturer-specific. A companion specification may define
/// it, with defaults this crate does not know, so its entry declares its own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UnmodelledResource(ResourceName);

impl UnmodelledResource {
    /// The Resource `text` names, or `None` for an invalid name, a standard
    /// Resource's, and a manufacturer-specific one's (`X-`).
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        if StandardResource::from_name(text).is_some() || text.starts_with("X-") {
            return None;
        }
        ResourceName::new(text).map(Self)
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// A Resource this crate has no type for: a manufacturer's own, `X-` and a name
/// (M2-103 §8.4), or one of another specification. Its Property Data is
/// opaque and its entry declares its own flags.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum UntypedResource {
    /// A manufacturer-specific Resource. Its entry gives a schema (M2-103
    /// §14.4).
    Custom(CustomResource),
    /// A Resource with a name no standard Resource and no manufacturer has.
    Unmodelled(UnmodelledResource),
}

impl UntypedResource {
    /// The name as the wire writes it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Custom(custom) => custom.as_str(),
            Self::Unmodelled(name) => name.as_str(),
        }
    }

    /// What a ResourceList entry says when it names nothing (M2-103 §14.3).
    #[must_use]
    pub const fn defaults() -> ResourceDefaults {
        ResourceDefaults {
            can_get: true,
            can_set: CanSet::None,
            can_subscribe: false,
            can_paginate: false,
            require_res_id: false,
        }
    }
}

impl From<CustomResource> for UntypedResource {
    fn from(custom: CustomResource) -> Self {
        Self::Custom(custom)
    }
}

impl fmt::Display for UntypedResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Resource of Property Exchange: a standard one, which has a type, or one
/// this crate has no type for.
///
/// A standard Resource converts with `From`, and an API that takes a Resource
/// by value takes `impl Into<PeResource>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PeResource {
    /// A Resource a specification defines and this crate types.
    Standard(StandardResource),
    /// A Resource this crate has no type for.
    Untyped(UntypedResource),
}

/// How an inquiry for a Resource names an entry of it (M2-103 §8.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResIdUse {
    /// Every inquiry carries a `resId`.
    Required,
    /// No inquiry carries a `resId`.
    Unused,
    /// The ResourceList entry says (`requireResId`).
    Declared,
}

impl fmt::Display for PeResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<StandardResource> for PeResource {
    fn from(resource: StandardResource) -> Self {
        Self::Standard(resource)
    }
}

impl From<UntypedResource> for PeResource {
    fn from(resource: UntypedResource) -> Self {
        Self::Untyped(resource)
    }
}

impl From<CustomResource> for PeResource {
    fn from(resource: CustomResource) -> Self {
        Self::Untyped(resource.into())
    }
}

impl From<&Self> for PeResource {
    fn from(resource: &Self) -> Self {
        resource.clone()
    }
}

impl PartialEq<StandardResource> for PeResource {
    fn eq(&self, other: &StandardResource) -> bool {
        *self == Self::Standard(*other)
    }
}

impl PeResource {
    /// The Resource a name on the wire stands for.
    #[must_use]
    pub fn from_name(name: &ResourceName) -> Self {
        if let Some(standard) = StandardResource::from_name(name.as_str()) {
            return Self::Standard(standard);
        }
        match CustomResource::new(name.as_str()) {
            Some(custom) => Self::Untyped(custom.into()),
            None => Self::Untyped(UntypedResource::Unmodelled(UnmodelledResource(
                name.clone(),
            ))),
        }
    }

    /// The name as the wire writes it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Standard(standard) => standard.as_str(),
            Self::Untyped(untyped) => untyped.as_str(),
        }
    }

    /// The name as a header carries it.
    #[must_use]
    pub fn name(&self) -> ResourceName {
        match self {
            Self::Untyped(UntypedResource::Custom(custom)) => custom.0.clone(),
            Self::Untyped(UntypedResource::Unmodelled(name)) => name.0.clone(),
            Self::Standard(standard) => standard.name(),
        }
    }

    /// Whether the Resource is a List Resource: a name that ends `List`
    /// (M2-103 §8.6).
    #[must_use]
    pub fn is_list(&self) -> bool {
        self.as_str().ends_with("List")
    }

    /// How an inquiry for the Resource names an entry: the standard Resource's
    /// own rule, and the entry's for any other.
    #[must_use]
    pub fn res_id_use(&self) -> ResIdUse {
        match self {
            Self::Standard(standard) if standard.requires_res_id() => ResIdUse::Required,
            Self::Standard(_) => ResIdUse::Unused,
            Self::Untyped(_) => ResIdUse::Declared,
        }
    }

    /// What a ResourceList entry says when it names nothing.
    #[must_use]
    pub fn defaults(&self) -> ResourceDefaults {
        match self {
            Self::Standard(standard) => standard.defaults(),
            Self::Untyped(_) => UntypedResource::defaults(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_standard_name_is_a_valid_resource_name() {
        for standard in StandardResource::ALL {
            let resource = PeResource::from(standard);
            assert_eq!(resource.name().as_str(), standard.as_str());
            assert_eq!(PeResource::from_name(&resource.name()), resource);
        }
    }
}
