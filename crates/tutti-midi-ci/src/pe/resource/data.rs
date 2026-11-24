//! [`PeData`]: the Property Data of a Resource, typed.

use super::super::limits::MediaType;
use super::super::tree::PeJson;
use super::super::value::PeExtra;
use super::channels::PeChannelList;
use super::controllers::{PeAllCtrlList, PeChCtrlList, PeCtrlMapList};
use super::entry::PeResourceList;
use super::fields::{InvalidValue, PeDataError, PeDataFault};
use super::info::{PeDeviceInfo, PeJsonSchema};
use super::kind::{PeResource, StandardResource, UntypedResource};
use super::link::PeLink;
use super::programs::{PeModeList, PeProgramList};
use super::simple::{
    PeBasicChannelRx, PeBasicChannelTx, PeChannelMode, PeCurrentMode, PeExternalSync, PeLocalOn,
};
use super::state::{PeState, PeStateList};

/// What the Property Data of a Resource without a type holds.
#[derive(Clone, Debug, PartialEq)]
pub enum OpaqueBody {
    /// JSON.
    Json(PeJson),
    /// Data that is not JSON.
    Binary(PeBinary),
}

impl OpaqueBody {
    /// The data as it is sent before its encoding: the media type, none for
    /// JSON, and the bytes.
    #[must_use]
    pub fn to_wire(&self) -> (Option<MediaType>, Vec<u8>) {
        match self {
            Self::Json(json) => (None, json.encode()),
            Self::Binary(binary) => (Some(binary.media_type.clone()), binary.bytes.clone()),
        }
    }

    /// The media type of the data when it is not JSON.
    #[must_use]
    pub fn media_type(&self) -> Option<&MediaType> {
        match self {
            Self::Json(_) => None,
            Self::Binary(binary) => Some(&binary.media_type),
        }
    }

    /// The data, when it is JSON.
    #[must_use]
    pub fn json(&self) -> Option<&PeJson> {
        match self {
            Self::Json(json) => Some(json),
            Self::Binary(_) => None,
        }
    }
}

/// Property Data that is not JSON, with the media type its header declares
/// (M2-103 §6.1.5, §7.5); the media type is not `application/json`, the media
/// type of Property Data that has none declared (M2-103 §6.1).
#[derive(Clone, Debug, PartialEq)]
pub struct PeBinary {
    media_type: MediaType,
    bytes: Vec<u8>,
}

impl PeBinary {
    /// The `bytes` of `media_type`, or `None` for `application/json`, which is
    /// the JSON form's.
    #[must_use]
    pub fn new(media_type: MediaType, bytes: Vec<u8>) -> Option<Self> {
        (!media_type.as_str().eq_ignore_ascii_case("application/json"))
            .then_some(Self { media_type, bytes })
    }

    /// The `bytes` that are no format a media type names:
    /// `application/octet-stream`.
    #[must_use]
    pub fn octets(bytes: Vec<u8>) -> Self {
        Self {
            media_type: MediaType::OCTET_STREAM,
            bytes,
        }
    }

    /// The media type.
    #[must_use]
    pub fn media_type(&self) -> &MediaType {
        &self.media_type
    }

    /// The data, before its encoding.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// The Property Data of a Resource this crate has no type for: a
/// manufacturer-specific Resource's, or a standard one's that is not modelled.
#[derive(Clone, Debug, PartialEq)]
pub struct PeOpaque {
    resource: UntypedResource,
    body: OpaqueBody,
}

impl PeOpaque {
    /// The Property Data `json` of `resource`. A standard Resource has a type, so
    /// its data is a [`PeData`] variant and not opaque:
    ///
    /// ```compile_fail,E0277
    /// use tutti_midi_ci::{PeJson, PeOpaque, StandardResource};
    ///
    /// let _ = PeOpaque::new(StandardResource::LocalOn, PeJson::Bool(true));
    /// ```
    #[must_use]
    pub fn new(resource: impl Into<UntypedResource>, json: PeJson) -> Self {
        Self {
            resource: resource.into(),
            body: OpaqueBody::Json(json),
        }
    }

    /// The Property Data `binary` of `resource`.
    #[must_use]
    pub fn binary(resource: impl Into<UntypedResource>, binary: PeBinary) -> Self {
        Self {
            resource: resource.into(),
            body: OpaqueBody::Binary(binary),
        }
    }

    /// The Resource.
    #[must_use]
    pub fn resource(&self) -> &UntypedResource {
        &self.resource
    }

    /// The Property Data.
    #[must_use]
    pub fn body(&self) -> &OpaqueBody {
        &self.body
    }
}

/// The Property Data of a Resource, as the type of that Resource.
///
/// [`decode`](Self::decode) reads the JSON of a Resource by its name and
/// [`to_json`](Self::to_json) writes it. Each variant's type converts both ways
/// with `From` and `TryFrom`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PeData {
    /// ResourceList.
    ResourceList(PeResourceList),
    /// DeviceInfo.
    DeviceInfo(PeDeviceInfo),
    /// ChannelList.
    ChannelList(PeChannelList),
    /// JSONSchema.
    JsonSchema(PeJsonSchema),
    /// ModeList.
    ModeList(PeModeList),
    /// CurrentMode.
    CurrentMode(PeCurrentMode),
    /// ChannelMode.
    ChannelMode(PeChannelMode),
    /// BasicChannelRx.
    BasicChannelRx(PeBasicChannelRx),
    /// BasicChannelTx.
    BasicChannelTx(PeBasicChannelTx),
    /// ProgramList.
    ProgramList(PeProgramList),
    /// LocalOn.
    LocalOn(PeLocalOn),
    /// ExternalSync.
    ExternalSync(PeExternalSync),
    /// AllCtrlList.
    AllCtrlList(PeAllCtrlList),
    /// ChCtrlList.
    ChCtrlList(PeChCtrlList),
    /// CtrlMapList.
    CtrlMapList(PeCtrlMapList),
    /// StateList.
    StateList(PeStateList),
    /// State.
    State(PeState),
    /// A Resource with no type here.
    Opaque(PeOpaque),
}

fn items(resource: StandardResource, json: &PeJson) -> Result<&[PeJson], PeDataError> {
    json.as_array()
        .ok_or_else(|| PeDataError::new(resource.as_str(), PeDataFault::WrongType("an array")))
}

impl PeData {
    /// The Resource the data belongs to.
    #[must_use]
    pub fn resource(&self) -> PeResource {
        match self {
            Self::ResourceList(_) => PeResource::Standard(StandardResource::ResourceList),
            Self::DeviceInfo(_) => PeResource::Standard(StandardResource::DeviceInfo),
            Self::ChannelList(_) => PeResource::Standard(StandardResource::ChannelList),
            Self::JsonSchema(_) => PeResource::Standard(StandardResource::JsonSchema),
            Self::ModeList(_) => PeResource::Standard(StandardResource::ModeList),
            Self::CurrentMode(_) => PeResource::Standard(StandardResource::CurrentMode),
            Self::ChannelMode(_) => PeResource::Standard(StandardResource::ChannelMode),
            Self::BasicChannelRx(_) => PeResource::Standard(StandardResource::BasicChannelRx),
            Self::BasicChannelTx(_) => PeResource::Standard(StandardResource::BasicChannelTx),
            Self::ProgramList(_) => PeResource::Standard(StandardResource::ProgramList),
            Self::LocalOn(_) => PeResource::Standard(StandardResource::LocalOn),
            Self::ExternalSync(_) => PeResource::Standard(StandardResource::ExternalSync),
            Self::AllCtrlList(_) => PeResource::Standard(StandardResource::AllCtrlList),
            Self::ChCtrlList(_) => PeResource::Standard(StandardResource::ChCtrlList),
            Self::CtrlMapList(_) => PeResource::Standard(StandardResource::CtrlMapList),
            Self::StateList(_) => PeResource::Standard(StandardResource::StateList),
            Self::State(_) => PeResource::Standard(StandardResource::State),
            Self::Opaque(opaque) => PeResource::Untyped(opaque.resource.clone()),
        }
    }

    /// The Links the data carries: those of DeviceInfo, and of the entries of a
    /// ChannelList and a ProgramList.
    pub(super) fn links(&self) -> Vec<&PeLink> {
        match self {
            Self::DeviceInfo(info) => info.links.iter().collect(),
            Self::ChannelList(list) => list.entries().iter().flat_map(|e| &e.links).collect(),
            Self::ProgramList(list) => list.entries().iter().flat_map(|e| &e.links).collect(),
            _ => Vec::new(),
        }
    }

    /// The Property Data of a Resource with a type as its JSON, or the body of
    /// the Resource without one.
    fn typed_json(&self) -> Result<PeJson, &OpaqueBody> {
        Ok(match self {
            Self::ResourceList(v) => v.to_json(),
            Self::DeviceInfo(v) => v.to_json(),
            Self::ChannelList(v) => v.to_json(),
            Self::JsonSchema(v) => v.to_json(),
            Self::ModeList(v) => v.to_json(),
            Self::CurrentMode(v) => v.to_json(),
            Self::ChannelMode(v) => v.to_json(),
            Self::BasicChannelRx(v) => v.to_json(),
            Self::BasicChannelTx(v) => v.to_json(),
            Self::ProgramList(v) => v.to_json(),
            Self::LocalOn(v) => v.to_json(),
            Self::ExternalSync(v) => v.to_json(),
            Self::AllCtrlList(v) => v.to_json(),
            Self::ChCtrlList(v) => v.to_json(),
            Self::CtrlMapList(v) => v.to_json(),
            Self::StateList(v) => v.to_json(),
            Self::State(v) => return Err(v.body()),
            Self::Opaque(v) => return Err(v.body()),
        })
    }

    /// The Property Data as JSON, or `None` for the data of a Resource that is not
    /// JSON.
    #[must_use]
    pub fn to_json(&self) -> Option<PeJson> {
        match self.typed_json() {
            Ok(json) => Some(json),
            Err(body) => body.json().cloned(),
        }
    }

    /// The media type of the data when it is not JSON.
    #[must_use]
    pub fn media_type(&self) -> Option<&MediaType> {
        match self {
            Self::State(state) => state.body().media_type(),
            Self::Opaque(opaque) => opaque.body().media_type(),
            Self::ResourceList(_)
            | Self::DeviceInfo(_)
            | Self::ChannelList(_)
            | Self::JsonSchema(_)
            | Self::ModeList(_)
            | Self::CurrentMode(_)
            | Self::ChannelMode(_)
            | Self::BasicChannelRx(_)
            | Self::BasicChannelTx(_)
            | Self::ProgramList(_)
            | Self::LocalOn(_)
            | Self::ExternalSync(_)
            | Self::AllCtrlList(_)
            | Self::ChCtrlList(_)
            | Self::CtrlMapList(_)
            | Self::StateList(_) => None,
        }
    }

    /// The Property Data as it is sent before its encoding: the media type, which is
    /// none for JSON, and the bytes.
    #[must_use]
    pub fn to_wire(&self) -> (Option<MediaType>, Vec<u8>) {
        match self.typed_json() {
            Ok(json) => (None, json.encode()),
            Err(body) => body.to_wire(),
        }
    }

    /// Reads the Property Data `json` of `resource`.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] for Property Data that is not the Resource's.
    pub fn decode(resource: impl Into<PeResource>, json: &PeJson) -> Result<Self, PeDataError> {
        match resource.into() {
            PeResource::Standard(standard) => Self::decode_standard(standard, json),
            PeResource::Untyped(untyped) => Ok(Self::Opaque(PeOpaque::new(untyped, json.clone()))),
        }
    }

    fn decode_standard(resource: StandardResource, json: &PeJson) -> Result<Self, PeDataError> {
        Ok(match resource {
            StandardResource::ResourceList => {
                Self::ResourceList(PeResourceList::from_items(items(resource, json)?)?)
            }
            StandardResource::DeviceInfo => Self::DeviceInfo(PeDeviceInfo::from_json(json)?),
            StandardResource::ChannelList => {
                Self::ChannelList(PeChannelList::from_items(items(resource, json)?)?)
            }
            StandardResource::JsonSchema => Self::JsonSchema(PeJsonSchema::from_json(json)?),
            StandardResource::ModeList => {
                Self::ModeList(PeModeList::from_items(items(resource, json)?)?)
            }
            StandardResource::CurrentMode => Self::CurrentMode(PeCurrentMode::from_json(json)?),
            StandardResource::ChannelMode => Self::ChannelMode(PeChannelMode::from_json(json)?),
            StandardResource::BasicChannelRx => {
                Self::BasicChannelRx(PeBasicChannelRx::from_json(json)?)
            }
            StandardResource::BasicChannelTx => {
                Self::BasicChannelTx(PeBasicChannelTx::from_json(json)?)
            }
            StandardResource::ProgramList => {
                Self::ProgramList(PeProgramList::from_items(items(resource, json)?)?)
            }
            StandardResource::LocalOn => Self::LocalOn(PeLocalOn::from_json(json)?),
            StandardResource::ExternalSync => Self::ExternalSync(PeExternalSync::from_json(json)?),
            StandardResource::AllCtrlList => {
                Self::AllCtrlList(PeAllCtrlList::from_items(items(resource, json)?)?)
            }
            StandardResource::ChCtrlList => {
                Self::ChCtrlList(PeChCtrlList::from_items(items(resource, json)?)?)
            }
            StandardResource::CtrlMapList => {
                Self::CtrlMapList(PeCtrlMapList::from_items(items(resource, json)?)?)
            }
            StandardResource::StateList => {
                Self::StateList(PeStateList::from_items(items(resource, json)?)?)
            }
            StandardResource::State => Self::State(PeState::json(json.clone())),
        })
    }

    /// Reads the Property Data `bytes` of `resource` that is of `media_type`: JSON
    /// when the media type is none or `application/json`, and otherwise the
    /// bytes of a Resource that has no type.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] for JSON that is not the Resource's, and for data that is
    /// not JSON of a Resource that is typed.
    pub fn decode_bytes(
        resource: impl Into<PeResource>,
        media_type: Option<&MediaType>,
        bytes: &[u8],
    ) -> Result<Self, PeDataError> {
        let resource = resource.into();
        if resource == StandardResource::State {
            return PeState::from_wire(media_type, bytes).map(Self::State);
        }
        match media_type.and_then(|m| PeBinary::new(m.clone(), bytes.to_vec())) {
            None => {
                let json = PeJson::try_from(bytes).map_err(|e| {
                    PeDataError::new(resource.as_str(), PeDataFault::NotValid(e.into()))
                })?;
                Self::decode(resource, &json)
            }
            Some(binary) => match resource {
                PeResource::Untyped(untyped) => Ok(Self::Opaque(PeOpaque::binary(untyped, binary))),
                PeResource::Standard(standard) => Err(PeDataError::new(
                    standard.as_str(),
                    PeDataFault::NotValid(InvalidValue::ResourceIsTyped),
                )),
            },
        }
    }

    /// Writes the properties of a reply's header the data carries: the revision
    /// and time of a state (M2-111 §3.2).
    pub(super) fn write_header(&self, extra: &mut PeExtra) {
        if let Self::State(state) = self {
            state.write_header(extra);
        }
    }

    /// The data with the properties of a reply's header it takes.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] for a property the data cannot take.
    pub(super) fn read_header(self, extra: &PeExtra) -> Result<Self, PeDataError> {
        match self {
            Self::State(state) => state.read_header(extra).map(Self::State),
            other => Ok(other),
        }
    }

    /// The pages of a List Resource joined: the entries of each page in order.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] when a page is not an array of entries.
    pub(super) fn decode_pages(
        resource: &PeResource,
        pages: &[PeJson],
    ) -> Result<Self, PeDataError> {
        let mut all = Vec::new();
        for page in pages {
            let page_items = page.as_array().ok_or_else(|| {
                PeDataError::new(resource.as_str(), PeDataFault::WrongType("an array"))
            })?;
            all.extend_from_slice(page_items);
        }
        Self::decode(resource, &PeJson::Array(all))
    }
}

macro_rules! convert {
    ($($variant:ident($ty:ty)),+ $(,)?) => {
        $(
            impl From<$ty> for PeData {
                fn from(value: $ty) -> Self {
                    Self::$variant(value)
                }
            }

            impl TryFrom<PeData> for $ty {
                type Error = PeData;

                fn try_from(data: PeData) -> Result<Self, PeData> {
                    match data {
                        PeData::$variant(value) => Ok(value),
                        other => Err(other),
                    }
                }
            }
        )+
    };
}

convert! {
    ResourceList(PeResourceList),
    DeviceInfo(PeDeviceInfo),
    ChannelList(PeChannelList),
    JsonSchema(PeJsonSchema),
    ModeList(PeModeList),
    CurrentMode(PeCurrentMode),
    ChannelMode(PeChannelMode),
    BasicChannelRx(PeBasicChannelRx),
    BasicChannelTx(PeBasicChannelTx),
    ProgramList(PeProgramList),
    LocalOn(PeLocalOn),
    ExternalSync(PeExternalSync),
    AllCtrlList(PeAllCtrlList),
    ChCtrlList(PeChCtrlList),
    CtrlMapList(PeCtrlMapList),
    StateList(PeStateList),
    State(PeState),
    Opaque(PeOpaque),
}
