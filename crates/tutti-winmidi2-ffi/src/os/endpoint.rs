//! The service's availability and the endpoints it lists.

use windows_core::HSTRING;

use super::Error;
use super::call::{call, statics};
use crate::bindings::Windows::Devices::Midi2::Diagnostics::{
    IMidiDiagnosticsStatics, MidiDiagnostics,
};
use crate::bindings::Windows::Devices::Midi2::Enumeration::{
    IMidiEndpointDeviceInformationStatics, MidiEndpointDeviceInformation,
    MidiEndpointDeviceInformationFilters, MidiEndpointDeviceInformationSortOrder,
};
use crate::bindings::Windows::Devices::Midi2::{IMidiApiStatics, MidiApi};
use windows_collections::IVectorView;

/// Which of the service's two cross-wired diagnostic loopback endpoints an endpoint is: what
/// is sent to one arrives at the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Loopback {
    /// `MidiDiagnostics.DiagnosticsLoopbackAEndpointDeviceId`.
    A,
    /// `MidiDiagnostics.DiagnosticsLoopbackBEndpointDeviceId`.
    B,
}

/// One endpoint the service lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The service's endpoint device id, which `Session::connect` takes.
    pub id: String,
    /// The endpoint's name.
    pub name: String,
    /// Which diagnostic loopback endpoint it is, if it is one.
    pub loopback: Option<Loopback>,
}

/// Starts the MIDI service if it is stopped and answers whether it is running.
///
/// # Errors
///
/// `Error::ApiMissing` when the API is not present, `Error::ServiceUnavailable` when the
/// service is disabled or the PC is in its Legacy API mode.
pub fn ensure_service_available() -> Result<(), Error> {
    let api = statics::<MidiApi, IMidiApiStatics>()?;
    if call!("EnsureServiceAvailable", api, EnsureServiceAvailable() -> bool)? {
        Ok(())
    } else {
        Err(Error::ServiceUnavailable)
    }
}

/// The ids of the two diagnostic loopback endpoints, which tell an endpoint's `loopback`.
#[derive(Clone, Debug)]
pub(super) struct LoopbackIds {
    a: HSTRING,
    b: HSTRING,
}

impl LoopbackIds {
    /// Reads the two ids from the service's API.
    pub(super) fn read() -> Result<Self, Error> {
        let diagnostics = statics::<MidiDiagnostics, IMidiDiagnosticsStatics>()?;
        Ok(Self {
            a: call!(
                "DiagnosticsLoopbackAEndpointDeviceId",
                diagnostics,
                DiagnosticsLoopbackAEndpointDeviceId() -> HSTRING
            )?,
            b: call!(
                "DiagnosticsLoopbackBEndpointDeviceId",
                diagnostics,
                DiagnosticsLoopbackBEndpointDeviceId() -> HSTRING
            )?,
        })
    }

    /// The endpoint `information` describes.
    pub(super) fn describe(
        &self,
        information: &MidiEndpointDeviceInformation,
    ) -> Result<Endpoint, Error> {
        let id =
            call!("EndpointDeviceId", *information, EndpointDeviceId() -> HSTRING)?.to_string();
        let name = call!("Name", *information, Name() -> HSTRING)?.to_string();
        // Device interface paths compare without regard to case.
        let is = |loopback: &HSTRING| id.eq_ignore_ascii_case(&loopback.to_string());
        let loopback = if is(&self.a) {
            Some(Loopback::A)
        } else if is(&self.b) {
            Some(Loopback::B)
        } else {
            None
        };
        Ok(Endpoint { id, name, loopback })
    }
}

/// The endpoints the service lists now: the standard UMP and MIDI 1.0 endpoints and the
/// diagnostic loopback pair, by endpoint device id.
///
/// # Errors
///
/// `Error::ApiMissing` when the API is not present, `Error::Call` when a read fails and
/// `Error::Refused` when the service answers with no list.
pub fn endpoints() -> Result<Vec<Endpoint>, Error> {
    let loopbacks = LoopbackIds::read()?;
    let information =
        statics::<MidiEndpointDeviceInformation, IMidiEndpointDeviceInformationStatics>()?;
    let list = call!(
        "FindAll",
        information,
        FindAll3(
            MidiEndpointDeviceInformationSortOrder::EndpointDeviceId,
            MidiEndpointDeviceInformationFilters::AllStandardEndpoints
                | MidiEndpointDeviceInformationFilters::DiagnosticLoopback
        ) -> Option<IVectorView<MidiEndpointDeviceInformation>>
    )?
    .ok_or(Error::Refused { call: "FindAll" })?;
    list.into_iter()
        .map(|endpoint| loopbacks.describe(&endpoint))
        .collect()
}
