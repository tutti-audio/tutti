//! ResourceList (M2-103 §14): the entries that say how each Resource of a device
//! can be used.

use core::marker::PhantomData;

use super::super::header::PeEncoding;
use super::super::limits::{MediaType, ResourceName};
use super::super::tree::{PeJson, PeObject};
use super::fields::{Fields, PeDataError, PeDataFault, PeExtensions, UnlistedText, list, text};
use super::kind::{
    CanSet, CustomResource, PeResource, StandardResource, UnmodelledResource, UntypedResource,
};
use super::link::{PeColumn, flag};

/// A ResourceList entry that the rules of Property Exchange refuse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EntryError {
    /// The ResourceList does not list itself (M2-103 §14).
    #[error("ResourceList has no entry for itself")]
    ListsItself,
    /// An entry a peer sent declares other than the defaults of a standard
    /// Resource for `requireResId`, `canPaginate`, `canSet` or `canGet`
    /// (M2-103 §14.3).
    #[error("{property} of {resource} is not for an entry to override")]
    NotOverridable {
        /// The Resource.
        resource: String,
        /// The property.
        property: &'static str,
    },
    /// Only a List Resource paginates and has columns (M2-103 §14.2.1).
    #[error("{0} is not a List Resource")]
    NotAList(String),
    /// A manufacturer-specific JSON Resource gives a schema with at least a
    /// `title` and a `type` (M2-103 §14.4).
    #[error("the schema of {0} lacks a title or a type")]
    SchemaIncomplete(String),
    /// The schema of a Resource that is not JSON has a `type` (M2-103 §14.6).
    #[error("the schema of {0} has a type, and its data is not JSON")]
    SchemaTypedNonJson(String),
    /// Flow Control on Get needs `canGet`, and on Set needs `canSet`.
    #[error("{0} sets Flow Control for a transaction it does not take")]
    FlowControlWithoutTransaction(String),
    /// An encoding or a media type listed twice.
    #[error("{0} is listed twice")]
    Duplicate(String),
    /// A list of encodings or of media types with nothing in it.
    #[error("an empty list of {0}")]
    Empty(&'static str),
}

/// What a device says of one Resource it has: how it is read, set and
/// subscribed to, and how its Property Data is encoded (M2-103 §14.2). A
/// property not given has the default of the Resource (§14.3), and only a
/// property that differs is written.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceEntry {
    resource: PeResource,
    can_get: bool,
    fc_on_get: bool,
    can_set: CanSet,
    fc_on_set: bool,
    can_subscribe: bool,
    require_res_id: bool,
    can_paginate: bool,
    media_types: Option<Vec<MediaType>>,
    encodings: Option<Vec<PeEncoding>>,
    schema: Option<PeJson>,
    columns: Vec<PeColumn>,
    extensions: PeExtensions,
}

impl ResourceEntry {
    /// A builder of the entry for the standard Resource `resource`, every
    /// property its default. A standard Resource's entry declares none of
    /// `canGet`, `canSet`, `requireResId`, `canPaginate` and `schema`
    /// (M2-103 §14.3), so the builder has no setter for them:
    ///
    /// ```compile_fail
    /// use tutti_midi_ci::{CanSet, ResourceEntry, StandardResource};
    ///
    /// let _ = ResourceEntry::standard(StandardResource::DeviceInfo).can_set(CanSet::Full);
    /// ```
    #[must_use]
    pub fn standard(resource: StandardResource) -> ResourceEntryBuilder<StandardResource> {
        ResourceEntryBuilder::of(Self::bare(resource.into()))
    }

    /// A builder of the entry for the manufacturer-specific Resource `name`,
    /// with its `schema` (M2-103 §14.4).
    #[must_use]
    pub fn custom(name: CustomResource, schema: PeJson) -> ResourceEntryBuilder<UntypedResource> {
        let mut entry = Self::bare(PeResource::Untyped(name.into()));
        entry.schema = Some(schema);
        ResourceEntryBuilder::of(entry)
    }

    /// A builder of the entry for the Resource `name`, which no standard
    /// Resource and no manufacturer has. A companion specification may define
    /// it, so the entry says its own `canGet`, `canSet`, `requireResId` and
    /// `canPaginate`, which this crate cannot check.
    #[must_use]
    pub fn unmodelled(name: UnmodelledResource) -> ResourceEntryBuilder<UntypedResource> {
        ResourceEntryBuilder::of(Self::bare(PeResource::Untyped(
            UntypedResource::Unmodelled(name),
        )))
    }

    /// The entry of State with the defaults M2-111 §3.4 gives: Get and a Full Set,
    /// one state to each `resId`, Mcoded7 and `application/octet-stream`.
    #[must_use]
    pub fn device_state() -> Self {
        Self {
            encodings: Some(vec![PeEncoding::Mcoded7]),
            media_types: Some(vec![MediaType::OCTET_STREAM]),
            ..Self::bare(StandardResource::State.into())
        }
    }

    /// The builder of the entry of State, with those defaults.
    #[must_use]
    pub fn device_state_builder() -> ResourceEntryBuilder<StandardResource> {
        ResourceEntryBuilder::of(Self::device_state())
    }

    /// The entry of StateList.
    #[must_use]
    pub fn state_list() -> Self {
        Self::bare(StandardResource::StateList.into())
    }

    fn bare(resource: PeResource) -> Self {
        let d = resource.defaults();
        Self {
            can_get: d.can_get,
            fc_on_get: false,
            can_set: d.can_set,
            fc_on_set: false,
            can_subscribe: d.can_subscribe,
            require_res_id: d.require_res_id,
            can_paginate: d.can_paginate,
            media_types: None,
            encodings: None,
            schema: None,
            columns: Vec::new(),
            extensions: PeExtensions::default(),
            resource,
        }
    }

    /// The entry with `canSubscribe` false: what a store of a device with no way
    /// to tell its subscribers declares.
    pub(super) fn not_subscribable(mut self) -> Self {
        self.can_subscribe = false;
        self
    }

    /// The Resource.
    #[must_use]
    pub fn resource(&self) -> &PeResource {
        &self.resource
    }

    /// Whether Get reads the Resource.
    #[must_use]
    pub fn can_get(&self) -> bool {
        self.can_get
    }

    /// Whether a Get may use Flow Control.
    #[must_use]
    pub fn fc_on_get(&self) -> bool {
        self.fc_on_get
    }

    /// What Set may do.
    #[must_use]
    pub fn can_set(&self) -> CanSet {
        self.can_set
    }

    /// Whether a Set must use Flow Control.
    #[must_use]
    pub fn fc_on_set(&self) -> bool {
        self.fc_on_set
    }

    /// Whether the Resource can be subscribed to.
    #[must_use]
    pub fn can_subscribe(&self) -> bool {
        self.can_subscribe
    }

    /// Whether an inquiry names an entry with `resId`.
    #[must_use]
    pub fn require_res_id(&self) -> bool {
        self.require_res_id
    }

    /// Whether a Get may page the List Resource.
    #[must_use]
    pub fn can_paginate(&self) -> bool {
        self.can_paginate
    }

    /// The media types of the Property Data: `application/json` unless the
    /// entry lists others.
    #[must_use]
    pub fn media_types(&self) -> Vec<MediaType> {
        self.media_types
            .clone()
            .unwrap_or_else(|| vec![MediaType::JSON])
    }

    /// The encodings of the Property Data: ASCII unless the entry lists others.
    #[must_use]
    pub fn encodings(&self) -> &[PeEncoding] {
        self.encodings.as_deref().unwrap_or(&[PeEncoding::Ascii])
    }

    /// The JSON Schema, when the entry gives one.
    #[must_use]
    pub fn schema(&self) -> Option<&PeJson> {
        self.schema.as_ref()
    }

    /// The Columns of a List Resource.
    #[must_use]
    pub fn columns(&self) -> &[PeColumn] {
        &self.columns
    }

    /// The manufacturer-specific properties.
    #[must_use]
    pub fn extensions(&self) -> &PeExtensions {
        &self.extensions
    }

    pub(super) fn to_json(&self) -> PeJson {
        let d = self.resource.defaults();
        let mut o = PeObject::new();
        o.insert("resource", self.resource.as_str().into());
        let flag_if = |o: &mut PeObject, name: &str, value: bool, default: bool| {
            if value != default {
                o.insert(name, PeJson::Bool(value));
            }
        };
        flag_if(&mut o, "canGet", self.can_get, d.can_get);
        flag_if(&mut o, "fcOnGet", self.fc_on_get, false);
        if self.can_set != d.can_set {
            o.insert("canSet", self.can_set.as_str().into());
        }
        flag_if(&mut o, "fcOnSet", self.fc_on_set, false);
        flag_if(&mut o, "canSubscribe", self.can_subscribe, d.can_subscribe);
        flag_if(
            &mut o,
            "requireResId",
            self.require_res_id,
            d.require_res_id,
        );
        flag_if(&mut o, "canPaginate", self.can_paginate, d.can_paginate);
        if let Some(types) = &self.media_types {
            o.insert(
                "mediaTypes",
                PeJson::Array(types.iter().map(|m| m.as_str().into()).collect()),
            );
        }
        if let Some(encodings) = &self.encodings {
            o.insert(
                "encodings",
                PeJson::Array(encodings.iter().map(|e| e.as_str().into()).collect()),
            );
        }
        if let Some(schema) = &self.schema {
            o.insert("schema", schema.clone());
        }
        if !self.columns.is_empty() {
            o.insert(
                "columns",
                PeJson::Array(self.columns.iter().map(PeColumn::to_json).collect()),
            );
        }
        self.extensions.write(&mut o);
        PeJson::Object(o)
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let mut f = Fields::new(json, "resource entry")?;
        let name = f.required("resource", "a string", text)?.to_owned();
        let resource = ResourceName::try_from(name)
            .map(|n| PeResource::from_name(&n))
            .map_err(|e| f.invalid("resource", e))?;
        let defaults = Self::bare(resource);
        let mut entry = Self {
            can_get: flag(&mut f, "canGet", defaults.can_get)?,
            can_set: f
                .optional("canSet", "a string", text)?
                .map(|t| CanSet::try_from(t).map_err(|e| f.invalid("canSet", e)))
                .transpose()?
                .unwrap_or(defaults.can_set),
            fc_on_get: flag(&mut f, "fcOnGet", false)?,
            fc_on_set: flag(&mut f, "fcOnSet", false)?,
            can_subscribe: flag(&mut f, "canSubscribe", defaults.can_subscribe)?,
            require_res_id: flag(&mut f, "requireResId", defaults.require_res_id)?,
            can_paginate: flag(&mut f, "canPaginate", defaults.can_paginate)?,
            ..defaults
        };
        let types = list(&mut f, "mediaTypes", |j| {
            let text = j.as_str().ok_or(PeDataFault::WrongType("a string"))?;
            MediaType::try_from(text).map_err(|e| PeDataFault::NotValid(e.into()))
        })?;
        if !types.is_empty() {
            entry.media_types = Some(types);
        }
        let encodings = list(&mut f, "encodings", |j| {
            let text = j.as_str().ok_or(PeDataFault::WrongType("a string"))?;
            encoding_of(text).map_err(|e| PeDataFault::NotValid(e.into()))
        })?;
        if !encodings.is_empty() {
            entry.encodings = Some(encodings);
        }
        entry.schema = f.optional("schema", "an object", |j| j.as_object().map(|_| j.clone()))?;
        let columns = list(&mut f, "columns", |j| {
            PeColumn::from_json(j).map_err(|e| e.fault)
        })?;
        entry.columns = columns;
        entry.received().map_err(|(e, field)| f.invalid(field, e))?;
        entry.extensions = f.finish()?;
        Ok(entry)
    }
}

/// The properties of a [`ResourceEntry`] as a device declares them: each option
/// takes what the entry says, and `build` checks them against each other
/// (M2-103 §14). `R` is [`StandardResource`] for the entry of a standard
/// Resource, which declares only what the spec lets it, and [`UntypedResource`]
/// for any other, which also declares its own flags.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceEntryBuilder<R>(ResourceEntry, PhantomData<R>);

impl<R> ResourceEntryBuilder<R> {
    pub(super) fn of(entry: ResourceEntry) -> Self {
        Self(entry, PhantomData)
    }

    /// Declares `canSubscribe`.
    #[must_use]
    pub fn can_subscribe(mut self, subscribe: bool) -> Self {
        self.0.can_subscribe = subscribe;
        self
    }

    /// Declares `fcOnGet`.
    #[must_use]
    pub fn fc_on_get(mut self, flow: bool) -> Self {
        self.0.fc_on_get = flow;
        self
    }

    /// Declares `fcOnSet`.
    #[must_use]
    pub fn fc_on_set(mut self, flow: bool) -> Self {
        self.0.fc_on_set = flow;
        self
    }

    /// Declares `mediaTypes`.
    #[must_use]
    pub fn media_types(mut self, media_types: Vec<MediaType>) -> Self {
        self.0.media_types = Some(media_types);
        self
    }

    /// Declares `encodings`.
    #[must_use]
    pub fn encodings(mut self, encodings: Vec<PeEncoding>) -> Self {
        self.0.encodings = Some(encodings);
        self
    }

    /// Declares `columns`.
    #[must_use]
    pub fn columns(mut self, columns: Vec<PeColumn>) -> Self {
        self.0.columns = columns;
        self
    }

    /// Takes the manufacturer-specific properties.
    #[must_use]
    pub fn extensions(mut self, extensions: PeExtensions) -> Self {
        self.0.extensions = extensions;
        self
    }
}

impl ResourceEntryBuilder<StandardResource> {
    /// The entry the options declare.
    ///
    /// # Errors
    ///
    /// The [`EntryError`] that names what the Resource or the other properties
    /// refuse: [`EntryError::ListsItself`] for `ResourceList`;
    /// [`EntryError::NotAList`] for Columns of a Resource that is no List
    /// Resource; [`EntryError::FlowControlWithoutTransaction`] for Flow Control
    /// on a transaction the Resource does not take; [`EntryError::Empty`] for no
    /// encoding or no media type, and [`EntryError::Duplicate`] for one twice.
    pub fn build(self) -> Result<ResourceEntry, EntryError> {
        let entry = self.0;
        if entry.resource == StandardResource::ResourceList {
            return Err(EntryError::ListsItself);
        }
        entry.checked()?;
        Ok(entry)
    }
}

impl ResourceEntryBuilder<UntypedResource> {
    /// Declares `canGet`.
    #[must_use]
    pub fn can_get(mut self, can_get: bool) -> Self {
        self.0.can_get = can_get;
        self
    }

    /// Declares `canSet`.
    #[must_use]
    pub fn can_set(mut self, can_set: CanSet) -> Self {
        self.0.can_set = can_set;
        self
    }

    /// Declares `requireResId`.
    #[must_use]
    pub fn require_res_id(mut self, require: bool) -> Self {
        self.0.require_res_id = require;
        self
    }

    /// Declares `canPaginate`.
    #[must_use]
    pub fn can_paginate(mut self, paginate: bool) -> Self {
        self.0.can_paginate = paginate;
        self
    }

    /// Declares `schema`.
    #[must_use]
    pub fn schema(mut self, schema: PeJson) -> Self {
        self.0.schema = Some(schema);
        self
    }

    /// The entry the options declare.
    ///
    /// # Errors
    ///
    /// The [`EntryError`] that names what the other properties refuse:
    /// [`EntryError::SchemaIncomplete`] unless the schema of a
    /// manufacturer-specific Resource is an object with a `title` string and a
    /// `type` (a string or an array), or, when its media types leave out
    /// `application/json`, a `title` and no `type` (or an empty array), and for
    /// a schema with no `title` on any other;
    /// [`EntryError::SchemaTypedNonJson`] for a `type` on a schema of a Resource
    /// that is not JSON;
    /// [`EntryError::NotAList`] for pagination or Columns of a Resource that is
    /// no List Resource; [`EntryError::FlowControlWithoutTransaction`] for Flow
    /// Control on a transaction the Resource does not take;
    /// [`EntryError::Empty`] for no encoding or no media type, and
    /// [`EntryError::Duplicate`] for one twice.
    pub fn build(self) -> Result<ResourceEntry, EntryError> {
        let entry = self.0;
        entry.checked()?;
        entry.schema_authored()?;
        Ok(entry)
    }
}

impl ResourceEntry {
    /// Checks an entry a peer sent. A standard Resource keeps its defaults of
    /// `canGet`, `canSet`, `requireResId` and `canPaginate` (M2-103 §14.3), and
    /// the schema rules bind the sender, so any schema is taken. A refusal names
    /// the property it is of.
    fn received(&self) -> Result<(), (EntryError, &'static str)> {
        if let PeResource::Standard(standard) = &self.resource {
            let d = standard.defaults();
            let declared = [
                ("canGet", self.can_get != d.can_get),
                ("canSet", self.can_set != d.can_set),
                ("requireResId", self.require_res_id != d.require_res_id),
                ("canPaginate", self.can_paginate != d.can_paginate),
            ];
            if let Some((property, _)) = declared.into_iter().find(|(_, differs)| *differs) {
                let error = EntryError::NotOverridable {
                    resource: self.resource.as_str().to_owned(),
                    property,
                };
                return Err((error, property));
            }
        }
        self.checked().map_err(|error| {
            let field = self.field_of(&error);
            (error, field)
        })
    }

    /// The property of the entry that `error` refuses.
    fn field_of(&self, error: &EntryError) -> &'static str {
        match error {
            EntryError::ListsItself => "resource",
            EntryError::NotOverridable { property, .. } => property,
            EntryError::NotAList(_) if self.can_paginate => "canPaginate",
            EntryError::NotAList(_) => "columns",
            EntryError::SchemaIncomplete(_) | EntryError::SchemaTypedNonJson(_) => "schema",
            EntryError::FlowControlWithoutTransaction(_) if self.fc_on_get && !self.can_get => {
                "fcOnGet"
            }
            EntryError::FlowControlWithoutTransaction(_) => "fcOnSet",
            EntryError::Empty("encodings") => "encodings",
            EntryError::Empty(_) => "mediaTypes",
            EntryError::Duplicate(twice)
                if self
                    .encodings
                    .as_ref()
                    .is_some_and(|e| e.iter().any(|e| e.as_str() == twice)) =>
            {
                "encodings"
            }
            EntryError::Duplicate(_) => "mediaTypes",
        }
    }

    /// What an author of an entry owes its schema (M2-103 §14.3, §14.4): a
    /// manufacturer-specific Resource gives one with at least a `title` and a
    /// `type`. A received entry takes whatever schema its peer sent.
    fn schema_authored(&self) -> Result<(), EntryError> {
        let Some(schema) = &self.schema else {
            return Ok(());
        };
        let member = |name| schema.as_object().and_then(|o| o.get(name));
        let titled = member("title").and_then(PeJson::as_str).is_some();
        // A Resource whose media types leave out JSON is not JSON (§14.6): its schema
        // has a title and no type, and `type` as an empty array counts as none.
        let json = self
            .media_types
            .as_ref()
            .is_none_or(|types| types.contains(&MediaType::JSON));
        let name = || self.resource.as_str().to_owned();
        let typed = member("type");
        if !titled {
            return Err(EntryError::SchemaIncomplete(name()));
        }
        if json {
            let typed = typed.is_some_and(|t| t.as_str().is_some() || t.as_array().is_some());
            return if typed {
                Ok(())
            } else {
                Err(EntryError::SchemaIncomplete(name()))
            };
        }
        match typed {
            None => Ok(()),
            Some(t) if t.as_array().is_some_and(<[PeJson]>::is_empty) => Ok(()),
            Some(_) => Err(EntryError::SchemaTypedNonJson(name())),
        }
    }

    fn checked(&self) -> Result<(), EntryError> {
        if (self.can_paginate || !self.columns.is_empty()) && !self.resource.is_list() {
            return Err(EntryError::NotAList(self.resource.as_str().to_owned()));
        }
        if (self.fc_on_get && !self.can_get) || (self.fc_on_set && self.can_set == CanSet::None) {
            return Err(EntryError::FlowControlWithoutTransaction(
                self.resource.as_str().to_owned(),
            ));
        }
        if let Some(encodings) = &self.encodings {
            if encodings.is_empty() {
                return Err(EntryError::Empty("encodings"));
            }
            if let Some(twice) = encodings
                .iter()
                .enumerate()
                .find(|(at, e)| encodings[..*at].contains(e))
            {
                return Err(EntryError::Duplicate(twice.1.as_str().to_owned()));
            }
        }
        if let Some(media_types) = &self.media_types {
            if media_types.is_empty() {
                return Err(EntryError::Empty("media types"));
            }
            if let Some(twice) = media_types
                .iter()
                .enumerate()
                .find(|(at, m)| media_types[..*at].contains(m))
            {
                return Err(EntryError::Duplicate(twice.1.as_str().to_owned()));
            }
        }
        Ok(())
    }
}

/// The encoding a ResourceList names. The spec spells Mcoded7 with a capital C
/// once (M2-103 Table 82), so the names are read without regard to case.
fn encoding_of(text: &str) -> Result<PeEncoding, UnlistedText> {
    [
        PeEncoding::Ascii,
        PeEncoding::Mcoded7,
        PeEncoding::ZlibMcoded7,
    ]
    .into_iter()
    .find(|e| e.as_str().eq_ignore_ascii_case(text))
    .ok_or(UnlistedText {
        allowed: &["ASCII", "Mcoded7", "zlib+Mcoded7"],
    })
}

/// The Property Data of ResourceList: an entry for each Resource of the device,
/// not counting ResourceList (M2-103 §14).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PeResourceList(Vec<ResourceEntry>);

impl PeResourceList {
    /// A list with no entry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an entry, replacing the one for the same Resource.
    #[must_use]
    pub fn with(mut self, entry: ResourceEntry) -> Self {
        match self.0.iter_mut().find(|e| e.resource == entry.resource) {
            Some(old) => *old = entry,
            None => self.0.push(entry),
        }
        self
    }

    /// The entries, in order.
    #[must_use]
    pub fn entries(&self) -> &[ResourceEntry] {
        &self.0
    }

    /// The entry for `resource`.
    #[must_use]
    pub fn entry<R>(&self, resource: &R) -> Option<&ResourceEntry>
    where
        PeResource: PartialEq<R>,
    {
        self.0.iter().find(|e| e.resource == *resource)
    }

    pub(super) fn to_json(&self) -> PeJson {
        PeJson::Array(self.0.iter().map(ResourceEntry::to_json).collect())
    }

    pub(super) fn from_items(items: &[PeJson]) -> Result<Self, PeDataError> {
        let mut out = Self::new();
        for item in items {
            let entry = ResourceEntry::from_json(item)?;
            if out.entry(&entry.resource).is_some() {
                return Err(PeDataError::new(
                    entry.resource.as_str(),
                    PeDataFault::Conflicts("another entry of the same Resource"),
                ));
            }
            if entry.resource == StandardResource::ResourceList {
                return Err(PeDataError::new(
                    "ResourceList",
                    PeDataFault::NotValid(EntryError::ListsItself.into()),
                ));
            }
            out.0.push(entry);
        }
        Ok(out)
    }
}
