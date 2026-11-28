//! The logic of the Web MIDI backend, with no browser call in it.
//!
//! Web MIDI (W3C Web MIDI API) hands a page `MIDIAccess` after a permission request, lists
//! its ports as maps keyed by `MIDIPort.id`, and reports a port that comes or goes as a
//! `statechange` event. Everything the backend decides lives here, so it is compiled and
//! tested on every platform, and `web` holds only the calls into the browser:
//!
//! - [`ports`]: a port as the page reads it, and the endpoint, id and device list built
//!   from the ports a `MIDIAccess` lists.
//! - [`failure`]: the error a rejected `requestMIDIAccess` promise (a DOMException
//!   name) becomes.
//! - [`table`]: the page thread's table of browser objects keyed by an id, and the queue
//!   of ids another thread released for the page thread to sweep.
//!
//! A Web MIDI port is a MIDI 1.0 byte port: it is listed as MIDI 1.0 with no Function
//! Blocks, as a legacy ALSA or Windows Multimedia port is.

pub(super) mod failure;
pub(super) mod ports;
pub(super) mod table;

#[cfg(test)]
mod tests;
