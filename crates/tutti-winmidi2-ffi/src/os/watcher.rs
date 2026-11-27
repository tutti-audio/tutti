//! The service's endpoint device watcher: endpoints appearing, disappearing and changing.

use std::sync::{Arc, Mutex, PoisonError};

use windows_core::{IInspectable, Ref};

use super::Endpoint;
use super::Error;
use super::call::{call, contained, statics};
use super::endpoint::LoopbackIds;
use crate::bindings::Windows::Devices::Midi2::Enumeration::{
    IMidiEndpointDeviceWatcherStatics, MidiEndpointDeviceInformation,
    MidiEndpointDeviceInformationAddedEventArgs, MidiEndpointDeviceInformationFilters,
    MidiEndpointDeviceInformationRemovedEventArgs, MidiEndpointDeviceInformationUpdatedEventArgs,
    MidiEndpointDeviceWatcher,
};
use crate::bindings::Windows::Foundation::TypedEventHandler;

/// A change to the endpoints the service lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// An endpoint appeared. The watcher also reports each endpoint present when it starts
    /// this way.
    Added(Endpoint),
    /// The endpoint with this device id left.
    Removed(String),
    /// An endpoint's properties changed, its name among them.
    Updated(Endpoint),
}

/// A running watch over the service's endpoint list.
///
/// Dropping it removes its handlers and stops it. A handler call already running finishes.
#[derive(Debug)]
pub struct Watcher {
    watcher: MidiEndpointDeviceWatcher,
    /// The tokens of the registered handlers, in the order they were registered, so a drop
    /// removes exactly those that were.
    tokens: Vec<Token>,
}

/// A registered handler, by the event it is for.
#[derive(Clone, Copy, Debug)]
enum Token {
    Added(i64),
    Removed(i64),
    Updated(i64),
    Stopped(i64),
}

impl Watcher {
    /// Starts watching the standard endpoints and the diagnostic loopback pair. `on_change`
    /// receives each change on a thread of the service, one at a time, behind a panic guard.
    /// A change that cannot be read, and the watcher stopping on its own
    /// (`Error::WatcherStopped`), is the `Err` it receives in its place: the list it is
    /// keeping is then out of date.
    ///
    /// The service reports every endpoint already present as an `Added` first.
    ///
    /// # Errors
    ///
    /// `Error::ApiMissing` when the API is not present, `Error::Call` when a call fails and
    /// `Error::Refused` when the service creates no watcher.
    pub fn start<F>(on_change: F) -> Result<Self, Error>
    where
        F: FnMut(Result<Change, Error>) + Send + 'static,
    {
        let loopbacks = LoopbackIds::read()?;
        let statics = statics::<MidiEndpointDeviceWatcher, IMidiEndpointDeviceWatcherStatics>()?;
        let watcher = call!(
            "MidiEndpointDeviceWatcher.Create",
            statics,
            Create2(
                MidiEndpointDeviceInformationFilters::AllStandardEndpoints
                    | MidiEndpointDeviceInformationFilters::DiagnosticLoopback
            ) -> Option<MidiEndpointDeviceWatcher>
        )?
        .ok_or(Error::Refused {
            call: "MidiEndpointDeviceWatcher.Create",
        })?;
        let sink = Arc::new(Mutex::new(on_change));
        // From here a drop undoes whatever is registered, so a failure returns with `this`.
        let mut this = Self {
            watcher,
            tokens: Vec::new(),
        };
        this.register(&sink, &loopbacks)?;
        call!("Start", this.watcher, Start())?;
        Ok(this)
    }

    /// Registers the four handlers, each recording its token as it succeeds.
    fn register<F>(&mut self, sink: &Arc<Mutex<F>>, loopbacks: &LoopbackIds) -> Result<(), Error>
    where
        F: FnMut(Result<Change, Error>) + Send + 'static,
    {
        let call_error = super::error::call;
        let added = {
            let (sink, loopbacks) = (Arc::clone(sink), loopbacks.clone());
            TypedEventHandler::<
                MidiEndpointDeviceWatcher,
                MidiEndpointDeviceInformationAddedEventArgs,
            >::new(
                move |_, args: Ref<MidiEndpointDeviceInformationAddedEventArgs>| {
                    if let Some(args) = args.as_ref() {
                        contained(|| {
                            let change = call!("AddedDevice", *args, AddedDevice() -> Option<MidiEndpointDeviceInformation>)
                                .and_then(|device| device.ok_or(Error::Refused { call: "AddedDevice" }))
                                .and_then(|device| loopbacks.describe(&device))
                                .map(Change::Added);
                            deliver(&sink, change);
                        });
                    }
                    Ok(())
                },
            )
        };
        let token = self.watcher.Added(&added).map_err(call_error("Added"))?;
        self.tokens.push(Token::Added(token));
        let removed = {
            let sink = Arc::clone(sink);
            TypedEventHandler::<
                MidiEndpointDeviceWatcher,
                MidiEndpointDeviceInformationRemovedEventArgs,
            >::new(
                move |_, args: Ref<MidiEndpointDeviceInformationRemovedEventArgs>| {
                    if let Some(args) = args.as_ref() {
                        contained(|| {
                            // Only the id is read: what else a departed device still answers
                            // is not defined.
                            let change = call!("RemovedDevice", *args, RemovedDevice() -> Option<MidiEndpointDeviceInformation>)
                                .and_then(|device| device.ok_or(Error::Refused { call: "RemovedDevice" }))
                                .and_then(|device| {
                                    call!("EndpointDeviceId", device, EndpointDeviceId() -> windows_core::HSTRING)
                                })
                                .map(|id| Change::Removed(id.to_string()));
                            deliver(&sink, change);
                        });
                    }
                    Ok(())
                },
            )
        };
        let token = self
            .watcher
            .Removed(&removed)
            .map_err(call_error("Removed"))?;
        self.tokens.push(Token::Removed(token));
        let updated = {
            let (sink, loopbacks) = (Arc::clone(sink), loopbacks.clone());
            TypedEventHandler::<
                MidiEndpointDeviceWatcher,
                MidiEndpointDeviceInformationUpdatedEventArgs,
            >::new(
                move |_, args: Ref<MidiEndpointDeviceInformationUpdatedEventArgs>| {
                    if let Some(args) = args.as_ref() {
                        contained(|| {
                            let change = call!("UpdatedDevice", *args, UpdatedDevice() -> Option<MidiEndpointDeviceInformation>)
                                .and_then(|device| device.ok_or(Error::Refused { call: "UpdatedDevice" }))
                                .and_then(|device| loopbacks.describe(&device))
                                .map(Change::Updated);
                            deliver(&sink, change);
                        });
                    }
                    Ok(())
                },
            )
        };
        let token = self
            .watcher
            .Updated(&updated)
            .map_err(call_error("Updated"))?;
        self.tokens.push(Token::Updated(token));
        let stopped = {
            let sink = Arc::clone(sink);
            TypedEventHandler::<MidiEndpointDeviceWatcher, IInspectable>::new(move |_, _| {
                contained(|| deliver(&sink, Err(Error::WatcherStopped)));
                Ok(())
            })
        };
        let token = self
            .watcher
            .Stopped(&stopped)
            .map_err(call_error("Stopped"))?;
        self.tokens.push(Token::Stopped(token));
        Ok(())
    }
}

/// Hands the handler one change, or why there is none.
fn deliver<F: FnMut(Result<Change, Error>)>(sink: &Mutex<F>, change: Result<Change, Error>) {
    let mut handler = sink.lock().unwrap_or_else(PoisonError::into_inner);
    (*handler)(change);
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Failures are dropped on purpose, as in `Connection::close`: a drop has nothing left
        // to undo and must neither panic nor report. The handlers go first, so the stop's own
        // `Stopped` event reaches nobody.
        for token in &self.tokens {
            let _ = match *token {
                Token::Added(t) => self.watcher.RemoveAdded(t),
                Token::Removed(t) => self.watcher.RemoveRemoved(t),
                Token::Updated(t) => self.watcher.RemoveUpdated(t),
                Token::Stopped(t) => self.watcher.RemoveStopped(t),
            };
        }
        let _ = call!("Stop", self.watcher, Stop());
    }
}
