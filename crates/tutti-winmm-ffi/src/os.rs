//! The Windows half: every module that calls winmm. `lib.rs` compiles it on
//! Windows only and re-exports the public surface from here.

mod caps;
mod error;
mod input;
mod output;
mod sleeper;

pub use caps::{Caps, Input, InputPort, Output, OutputPort, Port, PortKind, ports};
pub use error::{Direction, Error, MmOperation, MmResult, OsOperation};
pub use input::{Event, MidiIn};
pub use output::MidiOut;
pub use sleeper::{TimerWaiter, TimerWaker, Woken, waitable_timer};
