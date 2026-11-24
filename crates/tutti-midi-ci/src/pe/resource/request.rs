//! The inquiries of a Resource as types that refuse what the spec forbids:
//! Get, Set and Start Subscription for one Resource, with the entry of it, its
//! page and its encoding (M2-103 §7.2, §8.5, §8.6.2, §10, §11).

use std::borrow::Cow;

use super::super::body::BodyLimit;
use super::super::header::PePage;
use super::super::header::{PeEncoding, PeSetScope};
use super::super::limits::{MediaType, ResourceId};
use super::super::partial::PartialUpdate;
use super::super::reassemble::PeFlowControl;
use super::super::tree::PeObject;
use super::data::PeData;
use super::entry::ResourceEntry;
use super::fields::PeDataError;
use super::kind::{CanSet, PeResource, ResIdUse, StandardResource};
use super::link::{LinkRole, PeLink};

/// An inquiry that the rules of Property Exchange refuse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RequestRefused {
    /// The Resource has entries, and an inquiry for it names one with `resId`
    /// (M2-103 §8.5).
    #[error("{0} needs a resId")]
    ResIdRequired(PeResource),
    /// The Resource has no entries, so an inquiry for it carries no `resId`.
    #[error("{0} takes no resId")]
    ResIdUnused(PeResource),
    /// Only a List Resource is paginated (M2-103 §8.6).
    #[error("{0} is not a List Resource, which pagination is for")]
    NotPaginable(PeResource),
    /// The Property Data of a Set belongs to another Resource.
    #[error("the data is of {data} and the Set is for {target}")]
    DataOfAnotherResource {
        /// The Resource of the data.
        data: PeResource,
        /// The Resource the Set is for.
        target: PeResource,
    },
    /// The ResourceList has no entry for the Resource.
    #[error("{0} is not in the ResourceList")]
    NotListed(PeResource),
    /// The entry of the ResourceList does not let the Resource be got.
    #[error("{0} cannot be got")]
    CannotGet(PeResource),
    /// The entry does not let the Resource be set, or set in part.
    #[error("{resource} cannot be set {}", in_scope(*.scope))]
    CannotSet {
        /// The Resource.
        resource: PeResource,
        /// What was asked: the whole Property Data or a part of it.
        scope: PeSetScope,
    },
    /// The entry does not let the Resource be subscribed to.
    #[error("{0} cannot be subscribed to")]
    CannotSubscribe(PeResource),
    /// The entry does not declare pagination.
    #[error("{0} does not paginate")]
    NotDeclaredPaginable(PeResource),
    /// The entry does not declare the encoding.
    #[error("{resource} does not take {encoding}")]
    EncodingNotDeclared {
        /// The Resource.
        resource: PeResource,
        /// The encoding.
        encoding: &'static str,
    },
    /// The entry does not declare Flow Control for Get.
    #[error("{0} does not use Flow Control on Get")]
    FlowControlNotDeclared(PeResource),
    /// The entry does not list the media type of the data.
    #[error("{resource} does not take {media_type}")]
    MediaTypeNotDeclared {
        /// The Resource.
        resource: PeResource,
        /// The media type.
        media_type: MediaType,
    },
    /// The entry says whether `resId` is used, and the inquiry differs.
    #[error("the entry of {resource} says requireResId is {required}")]
    ResIdDisagrees {
        /// The Resource.
        resource: PeResource,
        /// What the entry says.
        required: bool,
    },
}

fn in_scope(scope: PeSetScope) -> &'static str {
    match scope {
        PeSetScope::Whole => "in whole",
        PeSetScope::Partial => "in part",
    }
}

/// A Resource, and the entry of it an inquiry names, as the rules of the
/// Resource have them (M2-103 §8.5): a Resource that has entries is named with
/// one, and one that has none is named without.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceTarget {
    resource: PeResource,
    res_id: Option<ResourceId>,
}

impl TryFrom<PeResource> for ResourceTarget {
    type Error = RequestRefused;

    /// The Resource without an entry.
    ///
    /// # Errors
    ///
    /// [`RequestRefused::ResIdRequired`] for a Resource that has entries.
    fn try_from(resource: PeResource) -> Result<Self, RequestRefused> {
        if resource.res_id_use() == ResIdUse::Required {
            return Err(RequestRefused::ResIdRequired(resource));
        }
        Ok(Self {
            resource,
            res_id: None,
        })
    }
}

impl TryFrom<StandardResource> for ResourceTarget {
    type Error = RequestRefused;

    /// The Resource without an entry.
    ///
    /// # Errors
    ///
    /// [`RequestRefused::ResIdRequired`] for a Resource that has entries.
    fn try_from(resource: StandardResource) -> Result<Self, RequestRefused> {
        Self::try_from(PeResource::from(resource))
    }
}

impl ResourceTarget {
    /// The State list, a Resource with no entry.
    pub(super) fn state_list() -> Self {
        Self {
            resource: StandardResource::StateList.into(),
            res_id: None,
        }
    }

    /// The entry `res_id` of the Resource.
    ///
    /// # Errors
    ///
    /// [`RequestRefused::ResIdUnused`] for a Resource that has no entries.
    pub fn entry(
        resource: impl Into<PeResource>,
        res_id: ResourceId,
    ) -> Result<Self, RequestRefused> {
        let resource = resource.into();
        if resource.res_id_use() == ResIdUse::Unused {
            return Err(RequestRefused::ResIdUnused(resource));
        }
        Ok(Self {
            resource,
            res_id: Some(res_id),
        })
    }

    /// The Resource.
    #[must_use]
    pub fn resource(&self) -> &PeResource {
        &self.resource
    }

    /// The entry of the Resource.
    #[must_use]
    pub fn res_id(&self) -> Option<&ResourceId> {
        self.res_id.as_ref()
    }

    /// Whether the target follows what the ResourceList entry says of `resId`.
    fn agrees_with(&self, entry: &ResourceEntry) -> Result<(), RequestRefused> {
        if entry.require_res_id() != self.res_id.is_some() {
            return Err(RequestRefused::ResIdDisagrees {
                resource: self.resource.clone(),
                required: entry.require_res_id(),
            });
        }
        Ok(())
    }
}

/// Get Property Data for a Resource, or the Start of a Subscription to it, which
/// has the same header (M2-103 §7.2, §11.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceGet {
    target: ResourceTarget,
    page: Option<PePage>,
    encoding: PeEncoding,
    flow: PeFlowControl,
    limit: Option<BodyLimit>,
}

impl ResourceGet {
    /// A Get of the whole Property Data, in ASCII, with no Flow Control.
    #[must_use]
    pub fn new(target: ResourceTarget) -> Self {
        Self {
            target,
            page: None,
            encoding: PeEncoding::Ascii,
            flow: PeFlowControl::Off,
            limit: None,
        }
    }

    /// Ends the Get, telling the Responder to stop, if the reply's Property Data
    /// grows past `limit`.
    #[must_use]
    pub fn limited(mut self, limit: BodyLimit) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The most the reply may be.
    #[must_use]
    pub fn limit(&self) -> Option<BodyLimit> {
        self.limit
    }

    /// Asks for a page of a List Resource: `offset` and `limit` together
    /// (M2-103 §8.6.2). [`check_get`](Self::check_get) refuses a page of a
    /// Resource that is no List Resource.
    #[must_use]
    pub fn paged(mut self, page: PePage) -> Self {
        self.page = Some(page);
        self
    }

    /// Asks for the reply in `encoding`.
    #[must_use]
    pub fn encoded(mut self, encoding: PeEncoding) -> Self {
        self.encoding = encoding;
        self
    }

    /// Asks for the reply under Flow Control, when the Resource declares it.
    #[must_use]
    pub fn flow_controlled(mut self) -> Self {
        self.flow = PeFlowControl::On;
        self
    }

    /// The Resource and its entry.
    #[must_use]
    pub fn target(&self) -> &ResourceTarget {
        &self.target
    }

    /// The page asked for.
    #[must_use]
    pub fn page(&self) -> Option<PePage> {
        self.page
    }

    /// The encoding asked for.
    #[must_use]
    pub fn encoding(&self) -> PeEncoding {
        self.encoding
    }

    /// Whether the reply is asked for under Flow Control.
    #[must_use]
    pub fn flow(&self) -> PeFlowControl {
        self.flow
    }

    /// Checks the inquiry against the entry of the ResourceList.
    ///
    /// # Errors
    ///
    /// The [`RequestRefused`] that names what the entry does not allow.
    pub fn check_get(&self, entry: &ResourceEntry) -> Result<(), RequestRefused> {
        let name = || self.target.resource.clone();
        if !entry.can_get() {
            return Err(RequestRefused::CannotGet(name()));
        }
        self.check_header(entry)
    }

    /// Checks the Start of a Subscription against the entry of the ResourceList.
    ///
    /// # Errors
    ///
    /// The [`RequestRefused`] that names what the entry does not allow.
    pub fn check_subscribe(&self, entry: &ResourceEntry) -> Result<(), RequestRefused> {
        if !entry.can_subscribe() {
            return Err(RequestRefused::CannotSubscribe(
                self.target.resource.clone(),
            ));
        }
        self.check_header(entry)
    }

    /// Refuses a page of a Resource that is no List Resource, which needs no
    /// entry of the ResourceList (M2-103 §8.6.2).
    pub(super) fn check_page_target(&self) -> Result<(), RequestRefused> {
        if self.page.is_some() && !self.target.resource.is_list() {
            return Err(RequestRefused::NotPaginable(self.target.resource.clone()));
        }
        Ok(())
    }

    fn check_header(&self, entry: &ResourceEntry) -> Result<(), RequestRefused> {
        let name = || self.target.resource.clone();
        self.target.agrees_with(entry)?;
        self.check_page_target()?;
        if self.page.is_some() && !entry.can_paginate() {
            return Err(RequestRefused::NotDeclaredPaginable(name()));
        }
        if !entry.encodings().contains(&self.encoding) {
            return Err(RequestRefused::EncodingNotDeclared {
                resource: name(),
                encoding: self.encoding.as_str(),
            });
        }
        if self.flow == PeFlowControl::On && !entry.fc_on_get() {
            return Err(RequestRefused::FlowControlNotDeclared(name()));
        }
        Ok(())
    }
}

/// What a Set sends: the whole Property Data, or the changes of a Partial Set
/// (M2-103 §10).
#[derive(Clone, Debug, PartialEq)]
pub enum SetBody {
    /// Method 1: the whole Property Data a Get would return.
    Full(PeData),
    /// Method 2: `setPartial`, each change a JSON Pointer and a scalar.
    Partial(PartialUpdate),
}

/// Set Property Data for a Resource (M2-103 §10).
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceSet {
    target: ResourceTarget,
    body: SetBody,
    encoding: PeEncoding,
}

impl TryFrom<&PeLink> for ResourceSet {
    type Error = ActionError;

    /// The Set a `simpleAction` Link asks for: its Resource and entry, and its
    /// `propData` as the Property Data (M2-103 §13.1.1).
    ///
    /// # Errors
    ///
    /// [`ActionError`] for a Link that is no `simpleAction` and for `propData`
    /// that is not the Resource's.
    fn try_from(link: &PeLink) -> Result<Self, ActionError> {
        let Some(LinkRole::Simple(action)) = &link.role else {
            return Err(ActionError::NotThatAction("simpleAction"));
        };
        let data = PeData::decode(&link.resource, &action.prop_data)?;
        Ok(Self::full(
            link_target(&link.resource, link.res_id.as_ref())?,
            data,
        )?)
    }
}

impl ResourceSet {
    /// A Full Set of `data`, in ASCII, to the Resource the data is of.
    ///
    /// # Errors
    ///
    /// [`RequestRefused::DataOfAnotherResource`] when `target` is another
    /// Resource.
    pub fn full(target: ResourceTarget, data: PeData) -> Result<Self, RequestRefused> {
        if data.resource() != *target.resource() {
            return Err(RequestRefused::DataOfAnotherResource {
                data: data.resource(),
                target: target.resource().clone(),
            });
        }
        Ok(Self {
            target,
            body: SetBody::Full(data),
            encoding: PeEncoding::Ascii,
        })
    }

    /// A Partial Set of `update`, in ASCII.
    #[must_use]
    pub fn partial(target: ResourceTarget, update: PartialUpdate) -> Self {
        Self {
            target,
            body: SetBody::Partial(update),
            encoding: PeEncoding::Ascii,
        }
    }

    /// Sends the Property Data in `encoding`.
    #[must_use]
    pub fn encoded(mut self, encoding: PeEncoding) -> Self {
        self.encoding = encoding;
        self
    }

    /// The Resource and its entry.
    #[must_use]
    pub fn target(&self) -> &ResourceTarget {
        &self.target
    }

    /// What is sent.
    #[must_use]
    pub fn body(&self) -> &SetBody {
        &self.body
    }

    /// The encoding of the Property Data.
    #[must_use]
    pub fn encoding(&self) -> PeEncoding {
        self.encoding
    }

    /// Checks the Set against the entry of the ResourceList.
    ///
    /// # Errors
    ///
    /// The [`RequestRefused`] that names what the entry does not allow.
    pub fn check(&self, entry: &ResourceEntry) -> Result<(), RequestRefused> {
        let (scope, media_type) = match &self.body {
            SetBody::Full(data) => (PeSetScope::Whole, data.media_type()),
            SetBody::Partial(_) => (PeSetScope::Partial, None),
        };
        check_set(entry, &self.target, scope, self.encoding, media_type)
    }

    /// The Set a `selectorAction` Link asks for once the user chose `chosen`, an
    /// entry of the List Resource of the Link: the action Resource, with the
    /// `selectedProperty` of the entry as the Property Data (M2-103 §13.1.2).
    ///
    /// # Errors
    ///
    /// [`ActionError`] for a Link that is no `selectorAction`, an entry that lacks
    /// the property, and a value that is not the action Resource's.
    pub fn from_selection(link: &PeLink, chosen: &PeObject) -> Result<Self, ActionError> {
        let Some(LinkRole::Selector(action)) = &link.role else {
            return Err(ActionError::NotThatAction("selectorAction"));
        };
        let value = action.selection(chosen).ok_or_else(|| {
            ActionError::NoSelectedProperty(action.selected_property.as_str().to_owned())
        })?;
        let data = PeData::decode(&action.action_resource, value)?;
        let target = link_target(&action.action_resource, action.action_res_id.as_ref())?;
        Ok(Self::full(target, data)?)
    }
}

/// Checks a Set of `scope` in `encoding`, with the `media_type` of its Property Data
/// (JSON when none), against the entry of the ResourceList. A Responder that has the
/// header of a Set and not yet its body checks with this what [`ResourceSet::check`]
/// checks of a Set it holds.
pub(super) fn check_set(
    entry: &ResourceEntry,
    target: &ResourceTarget,
    scope: PeSetScope,
    encoding: PeEncoding,
    media_type: Option<&MediaType>,
) -> Result<(), RequestRefused> {
    let name = || target.resource().clone();
    // `partial` includes `full` (M2-103 §14.2).
    let allowed = matches!(
        (entry.can_set(), scope),
        (CanSet::Partial, _) | (CanSet::Full, PeSetScope::Whole)
    );
    if !allowed {
        return Err(RequestRefused::CannotSet {
            resource: name(),
            scope,
        });
    }
    target.agrees_with(entry)?;
    if !entry.encodings().contains(&encoding) {
        return Err(RequestRefused::EncodingNotDeclared {
            resource: name(),
            encoding: encoding.as_str(),
        });
    }
    let media_type = media_type.unwrap_or(&MediaType::JSON);
    if !entry.media_types().contains(media_type) {
        return Err(RequestRefused::MediaTypeNotDeclared {
            resource: name(),
            media_type: media_type.clone(),
        });
    }
    Ok(())
}

/// The entry that judges an inquiry for `resource`: `held`, the one a ResourceList
/// gives for it, or ResourceList's own, which no list holds.
///
/// # Errors
///
/// [`RequestRefused::NotListed`] when `held` is none for another Resource.
pub(super) fn judging_entry<'a>(
    held: Option<&'a ResourceEntry>,
    resource: &PeResource,
) -> Result<Cow<'a, ResourceEntry>, RequestRefused> {
    if *resource == StandardResource::ResourceList {
        return Ok(Cow::Owned(ResourceEntry::resource_list()));
    }
    held.map(Cow::Borrowed)
        .ok_or_else(|| RequestRefused::NotListed(resource.clone()))
}

pub(super) fn link_target(
    resource: &PeResource,
    res_id: Option<&ResourceId>,
) -> Result<ResourceTarget, RequestRefused> {
    match res_id {
        Some(id) => ResourceTarget::entry(resource.clone(), id.clone()),
        None => ResourceTarget::try_from(resource.clone()),
    }
}

/// A Link whose Action cannot be made into a Set.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ActionError {
    /// The Link has not the role.
    #[error("the Link is not a {0} Link")]
    NotThatAction(&'static str),
    /// The chosen entry has no property `selectedProperty` names.
    #[error("the chosen entry has no {0}")]
    NoSelectedProperty(String),
    /// The data is not the Resource's.
    #[error(transparent)]
    Data(#[from] PeDataError),
    /// The Set breaks a rule of the Resource.
    #[error(transparent)]
    Refused(#[from] RequestRefused),
}
