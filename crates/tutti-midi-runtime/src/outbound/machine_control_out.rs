//! [`MachineControlSender`]: the MIDI Machine Control commands that make a
//! device follow this transport (RP-013 pp.20 and 28), from what the transport
//! did between two looks at it.
//!
//! The sender holds no clock and sends nothing: each [`observe`](MachineControlSender::observe)
//! takes the playhead as the control side reads it and returns the commands
//! the change since the last look calls for.
//!
//! - A transport that started sends **PLAY**; one that stopped, **STOP**.
//! - A position that is not where rolling or standing still would have left it
//!   is a locate: **LOCATE (target)** to its time code, and while the transport
//!   rolls (or starts rolling in the same look) **DEFERRED PLAY** after it, so
//!   the device plays once it has got there.
//! - The first look sends nothing: there is no change to report.
//!
//! A stop that also moved the position sends STOP alone. Commands are placed at
//! a look, not on a frame: MMC carries no time of its own.

use tutti_midi_protocol::{MmcCommand, MmcCommands, MmcLocate, MmcPosition, MtcRate, Recipient};
use tutti_types::{Frame, SampleRate, Seconds};

/// Why a look at the transport could not be sent as Machine Control: the
/// transport moved to a time no time code names (before zero, NaN, or a day
/// or more). The look is taken (the next one compares against it), and nothing
/// is sent.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("{0:?} has no time code")]
pub struct NoTimeCode(pub Seconds);

/// Adds `command` to `commands`. A look sends at most a Locate (11 bytes) and a
/// Deferred Play (1), far inside the 48 bytes a message holds.
fn put(commands: &mut MmcCommands, command: &MmcCommand) {
    commands
        .push(command)
        .expect("at most two commands of a dozen bytes fit 48 bytes");
}

/// What the transport was at a look.
#[derive(Clone, Copy, Debug)]
struct Seen {
    playing: bool,
    seconds: Seconds,
    frame: Frame,
}

/// Turns the transport's changes into Machine Control commands.
///
/// ```
/// use tutti_midi_protocol::{MtcRate, Recipient};
/// use tutti_midi_runtime::MachineControlSender;
/// use tutti_types::{Frame, SampleRate, Seconds};
///
/// let mut sender = MachineControlSender::new(Recipient::AllCall, MtcRate::Fps30);
/// let rate = SampleRate::SR_48K;
/// assert_eq!(sender.observe(false, Seconds::new(0.0), Frame::new(0), rate), Ok(None));
/// let started = sender.observe(true, Seconds::new(0.0), Frame::new(512), rate).unwrap().unwrap();
/// assert_eq!(started, [0x7F, 0x7F, 0x06, 0x02]);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct MachineControlSender {
    to: Recipient,
    rate: MtcRate,
    last: Option<Seen>,
}

impl MachineControlSender {
    /// How far a position may be from where it should be before it is a locate:
    /// a millisecond (tutti's choice: it is far over the rounding of a
    /// position, and far under a frame).
    pub const LOCATE_TOLERANCE: Seconds = Seconds::new(0.001);

    /// A sender to `to` that states time codes at `rate`.
    #[must_use]
    pub fn new(to: Recipient, rate: MtcRate) -> Self {
        Self {
            to,
            rate,
            last: None,
        }
    }

    /// Where the commands go.
    #[must_use]
    pub fn to(&self) -> Recipient {
        self.to
    }

    /// Looks at the transport: whether it `playing`, its position in `seconds`
    /// and the graph `frame` count the look was made at, on a graph at `rate`.
    /// The payload of the message the change since the last look calls for, if
    /// any: pack it with `Sysex7Packets::from_payload`.
    ///
    /// # Errors
    ///
    /// [`NoTimeCode`] when the transport moved to a time no time code names;
    /// nothing is sent.
    pub fn observe(
        &mut self,
        playing: bool,
        seconds: Seconds,
        frame: Frame,
        rate: SampleRate,
    ) -> Result<Option<Vec<u8>>, NoTimeCode> {
        let now = Seen {
            playing,
            seconds,
            frame,
        };
        let Some(before) = self.last.replace(now) else {
            return Ok(None);
        };
        let mut commands = MmcCommands::new();
        if before.playing && !playing {
            put(&mut commands, &MmcCommand::Stop);
            return Ok(Some(commands.payload(self.to)));
        }
        let expected = if before.playing {
            before.seconds
                + Frame::new(frame.get().saturating_sub(before.frame.get())).to_seconds(rate)
        } else {
            before.seconds
        };
        let moved = (now.seconds - expected).get().abs() > Self::LOCATE_TOLERANCE.get();
        if moved {
            let target =
                MmcPosition::from_seconds(now.seconds, self.rate).ok_or(NoTimeCode(now.seconds))?;
            put(
                &mut commands,
                &MmcCommand::Locate(MmcLocate::Target(target)),
            );
            if playing {
                put(&mut commands, &MmcCommand::DeferredPlay);
            }
        } else if playing && !before.playing {
            put(&mut commands, &MmcCommand::Play);
        }
        Ok((!commands.is_empty()).then(|| commands.payload(self.to)))
    }
}
