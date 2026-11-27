//! The Windows MIDI Services calls against the Windows this runs on. Every test here
//! is Windows-only and `services-tests`': they need the MIDI service (which
//! `ensure_service_available` starts) and `Windows.Devices.Midi2.dll` beside the test
//! binary, and a missing one is a failure with its name, never a skip.

#[cfg(all(windows, feature = "services-tests"))]
mod deadline;
#[cfg(all(windows, feature = "services-tests"))]
mod loopback;
#[cfg(all(windows, feature = "services-tests"))]
mod virtual_device;
#[cfg(all(windows, feature = "services-tests"))]
mod watcher;
