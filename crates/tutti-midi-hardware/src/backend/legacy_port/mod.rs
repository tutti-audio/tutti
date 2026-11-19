//! The logic of a MIDI 1.0 byte port, with no OS call in it.
//!
//! A byte-oriented MIDI API (Windows Multimedia is the one today) has no UMP
//! and no Endpoint discovery: a port is a list position, a name and a stream of
//! MIDI 1.0 bytes. Everything a backend over one decides lives here, so it is
//! compiled and tested on every platform, and the backend's own file holds only
//! the calls into the OS:
//!
//! - [`ports`]: a port's identity and the id that names it, stale-proof.
//! - [`bytes`]: SysEx bytes to SysEx7 packets on the way in, and a UMP to a
//!   short message or a whole SysEx message on the way out.
//! - [`anchor`]: an arrival stamp, the time since the port started, as an
//!   `Instant`.
//! - [`schedule`]: items held until their time and released in order, for an
//!   output that sends each event when it is due.
//! - [`sender`]: the loop of an output's sender thread over that queue, and what
//!   it shares with the output.
//! - [`poll`]: a device watch for an OS that cannot say a device came or went,
//!   by listing again on a timer.
//!
//! A port speaks MIDI 1.0 and is listed as such, so the session never offers it
//! a MIDI 2.0 message it cannot carry, and what arrives is type 2 packets that
//! the input node's `Midi2Translator` translates, as it does for a legacy ALSA port.

pub(super) mod anchor;
pub(super) mod bytes;
pub(super) mod poll;
pub(super) mod ports;
pub(super) mod schedule;
pub(super) mod sender;

#[cfg(test)]
mod tests;
